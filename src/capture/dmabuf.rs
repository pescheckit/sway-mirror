use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use wayland_client::{protocol::wl_output, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::export_dmabuf::v1::client::{
    zwlr_export_dmabuf_frame_v1::{self, ZwlrExportDmabufFrameV1},
    zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
};

use crate::wayland::AppState;

#[derive(Debug, Clone)]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
    #[allow(dead_code)]
    pub modifier: u64,
}

#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub format: u32, // DRM fourcc
    pub planes: Vec<DmabufPlane>,
    #[allow(dead_code)]
    pub fds: Vec<OwnedFd>, // Keep fds alive
}

/// Newtype wrapper for frame capture state to satisfy orphan rules
pub struct FrameCaptureData(pub Arc<Mutex<FrameCaptureState>>);

pub struct FrameCaptureState {
    pub frame: Option<CapturedFrame>,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub num_objects: u32,
    pub planes: Vec<DmabufPlane>,
    pub fds: Vec<OwnedFd>,
    pub done: bool,
    pub cancelled: bool,
}

impl FrameCaptureState {
    pub fn new() -> Self {
        Self {
            frame: None,
            width: 0,
            height: 0,
            format: 0,
            num_objects: 0,
            planes: Vec::new(),
            fds: Vec::new(),
            done: false,
            cancelled: false,
        }
    }

    pub fn reset(&mut self) {
        self.frame = None;
        self.width = 0;
        self.height = 0;
        self.format = 0;
        self.num_objects = 0;
        self.planes.clear();
        self.fds.clear();
        self.done = false;
        self.cancelled = false;
    }
}

pub struct DmabufCapture {
    pub capture_state: Arc<Mutex<FrameCaptureState>>,
}

impl DmabufCapture {
    pub fn new() -> Self {
        Self {
            capture_state: Arc::new(Mutex::new(FrameCaptureState::new())),
        }
    }

    pub fn request_frame(
        &self,
        manager: &ZwlrExportDmabufManagerV1,
        output: &wl_output::WlOutput,
        qh: &QueueHandle<AppState>,
        include_cursor: bool,
    ) -> ZwlrExportDmabufFrameV1 {
        let mut state = self.capture_state.lock().unwrap();
        state.reset();
        drop(state);

        manager.capture_output(
            if include_cursor { 1 } else { 0 },
            output,
            qh,
            FrameCaptureData(self.capture_state.clone()),
        )
    }

    pub fn is_done(&self) -> bool {
        let state = self.capture_state.lock().unwrap();
        state.done || state.cancelled
    }

    pub fn take_frame(&self) -> Option<CapturedFrame> {
        let mut state = self.capture_state.lock().unwrap();
        state.frame.take()
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
                format,
                num_objects,
                ..
            } => {
                capture.width = width;
                capture.height = height;
                capture.format = format;
                capture.num_objects = num_objects;
            }
            zwlr_export_dmabuf_frame_v1::Event::Object {
                fd,
                offset,
                stride,
                plane_index,
                ..
            } => {
                let owned_fd = unsafe { OwnedFd::from_raw_fd(fd.as_raw_fd()) };
                std::mem::forget(fd); // Don't close the original

                // Ensure we have enough space
                while capture.planes.len() <= plane_index as usize {
                    capture.planes.push(DmabufPlane {
                        fd: -1,
                        offset: 0,
                        stride: 0,
                        modifier: 0,
                    });
                }

                capture.planes[plane_index as usize] = DmabufPlane {
                    fd: owned_fd.as_raw_fd(),
                    offset,
                    stride,
                    modifier: 0,
                };
                capture.fds.push(owned_fd);
            }
            zwlr_export_dmabuf_frame_v1::Event::Ready { .. } => {
                capture.frame = Some(CapturedFrame {
                    width: capture.width,
                    height: capture.height,
                    format: capture.format,
                    planes: capture.planes.clone(),
                    fds: std::mem::take(&mut capture.fds),
                });
                capture.done = true;
                proxy.destroy();
            }
            zwlr_export_dmabuf_frame_v1::Event::Cancel { .. } => {
                capture.cancelled = true;
                proxy.destroy();
            }
            _ => {}
        }
    }
}
