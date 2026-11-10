use anyhow::{bail, Result};
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wayland_client::{protocol::wl_output, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::export_dmabuf::v1::client::{
    zwlr_export_dmabuf_frame_v1::{self, CancelReason, ZwlrExportDmabufFrameV1},
    zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
};

use crate::wayland::{AppState, WaylandConnection};

/// "No explicit modifier" sentinel from drm_fourcc.h
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// zwlr_export_dmabuf_frame_v1 buffer_flags: y_invert
const BUFFER_FLAG_Y_INVERT: u32 = 1;

/// ~1s of "temporary" cancels in a row means the compositor can't export dmabufs (e.g. pixman)
const MAX_CONSECUTIVE_CANCELS: u32 = 60;

#[derive(Debug, Clone)]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Description of a dmabuf ready to be imported into EGL
#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub format: u32,   // DRM fourcc
    pub modifier: u64, // DRM format modifier (tiling/compression layout)
    pub planes: Vec<DmabufPlane>,
    pub y_invert: bool,
    /// Owned plane fds (export-dmabuf); empty when owned elsewhere (ext-image-copy)
    #[allow(dead_code)]
    pub fds: Vec<OwnedFd>,
}

/// Newtype wrapper for frame capture state to satisfy orphan rules
pub struct FrameCaptureData(pub Arc<Mutex<FrameCaptureState>>);

#[derive(Default)]
pub struct FrameCaptureState {
    pub frame: Option<CapturedFrame>,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub y_invert: bool,
    pub planes: Vec<DmabufPlane>,
    pub fds: Vec<OwnedFd>,
    pub done: bool,
    pub cancelled: bool,
    pub permanent_failure: bool,
}

impl FrameCaptureState {
    fn reset(&mut self) {
        *self = Self {
            modifier: DRM_FORMAT_MOD_INVALID,
            ..Default::default()
        };
    }
}

/// Zero-copy capture via zwlr_export_dmabuf_manager_v1
pub struct ExportDmabufCapture {
    manager: ZwlrExportDmabufManagerV1,
    output: wl_output::WlOutput,
    include_cursor: bool,
    capture_state: Arc<Mutex<FrameCaptureState>>,
    current: Option<CapturedFrame>,
    consecutive_cancels: u32,
}

impl ExportDmabufCapture {
    pub fn new(
        conn: &WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
    ) -> Result<Self> {
        let manager = conn
            .state
            .dmabuf_manager
            .clone()
            .ok_or_else(|| anyhow::anyhow!("zwlr_export_dmabuf_manager_v1 not available"))?;
        Ok(Self {
            manager,
            output: output.clone(),
            include_cursor,
            capture_state: Arc::new(Mutex::new(FrameCaptureState::default())),
            current: None,
            consecutive_cancels: 0,
        })
    }

    /// Capture the next frame; Ok(None) if interrupted or temporarily cancelled
    pub fn next_frame(
        &mut self,
        conn: &mut WaylandConnection,
        running: &AtomicBool,
    ) -> Result<Option<&CapturedFrame>> {
        self.current = None;
        self.capture_state.lock().unwrap().reset();

        let qh = conn.queue_handle();
        let frame = self.manager.capture_output(
            self.include_cursor as i32,
            &self.output,
            &qh,
            FrameCaptureData(self.capture_state.clone()),
        );

        loop {
            {
                let state = self.capture_state.lock().unwrap();
                if state.done || state.cancelled {
                    break;
                }
            }
            if !running.load(Ordering::SeqCst) {
                frame.destroy();
                return Ok(None);
            }
            conn.dispatch_timeout(100)?;
        }

        let mut state = self.capture_state.lock().unwrap();
        if state.permanent_failure {
            bail!("export-dmabuf capture was cancelled permanently by the compositor");
        }
        self.current = state.frame.take();
        if self.current.is_some() {
            self.consecutive_cancels = 0;
        } else {
            self.consecutive_cancels += 1;
            if self.consecutive_cancels >= MAX_CONSECUTIVE_CANCELS {
                bail!(
                    "export-dmabuf: compositor cancelled {} frames in a row",
                    self.consecutive_cancels
                );
            }
        }
        Ok(self.current.as_ref())
    }
}

impl Dispatch<ZwlrExportDmabufFrameV1, FrameCaptureData> for AppState {
    fn event(
        _state: &mut Self,
        proxy: &ZwlrExportDmabufFrameV1,
        event: zwlr_export_dmabuf_frame_v1::Event,
        data: &FrameCaptureData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut capture = data.0.lock().unwrap();

        match event {
            zwlr_export_dmabuf_frame_v1::Event::Frame {
                width,
                height,
                buffer_flags,
                format,
                mod_high,
                mod_low,
                ..
            } => {
                capture.width = width;
                capture.height = height;
                capture.format = format;
                capture.modifier = ((mod_high as u64) << 32) | mod_low as u64;
                capture.y_invert = buffer_flags & BUFFER_FLAG_Y_INVERT != 0;
            }
            zwlr_export_dmabuf_frame_v1::Event::Object {
                fd,
                offset,
                stride,
                plane_index,
                ..
            } => {
                // Ensure we have enough space
                while capture.planes.len() <= plane_index as usize {
                    capture.planes.push(DmabufPlane {
                        fd: -1,
                        offset: 0,
                        stride: 0,
                    });
                }

                capture.planes[plane_index as usize] = DmabufPlane {
                    fd: fd.as_raw_fd(),
                    offset,
                    stride,
                };
                capture.fds.push(fd);
            }
            zwlr_export_dmabuf_frame_v1::Event::Ready { .. } => {
                capture.frame = Some(CapturedFrame {
                    width: capture.width,
                    height: capture.height,
                    format: capture.format,
                    modifier: capture.modifier,
                    planes: capture.planes.clone(),
                    y_invert: capture.y_invert,
                    fds: std::mem::take(&mut capture.fds),
                });
                capture.done = true;
                proxy.destroy();
            }
            zwlr_export_dmabuf_frame_v1::Event::Cancel { reason } => {
                capture.cancelled = true;
                capture.permanent_failure = matches!(reason, WEnum::Value(CancelReason::Permanent));
                proxy.destroy();
            }
            _ => {}
        }
    }
}
