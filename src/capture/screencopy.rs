use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wayland_client::{
    protocol::{wl_output, wl_shm},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::shm::{fourcc_name, wenum_raw, wl_shm_to_drm, ShmBuffer, SUPPORTED_SHM_FORMATS};
use super::ShmFrame;
use crate::wayland::{AppState, WaylandConnection};

#[derive(Default)]
pub struct ScreencopyState {
    /// (drm format, width, height, stride) of the offered shm buffer
    shm_info: Option<(u32, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

pub struct ScreencopyData(Arc<Mutex<ScreencopyState>>);

/// wlr-screencopy capture into shared memory: slower, but works on any GPU
pub struct ScreencopyCapture {
    manager: ZwlrScreencopyManagerV1,
    shm: wl_shm::WlShm,
    output: wl_output::WlOutput,
    include_cursor: bool,
    state: Arc<Mutex<ScreencopyState>>,
    buffer: Option<ShmBuffer>,
    y_invert: bool,
}

impl ScreencopyCapture {
    pub fn new(
        conn: &WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
    ) -> Result<Self> {
        let manager = conn
            .state
            .screencopy_manager
            .clone()
            .ok_or_else(|| anyhow::anyhow!("zwlr_screencopy_manager_v1 not available"))?;
        let shm = conn
            .state
            .shm
            .clone()
            .ok_or_else(|| anyhow::anyhow!("wl_shm not available"))?;
        Ok(Self {
            manager,
            shm,
            output: output.clone(),
            include_cursor,
            state: Arc::new(Mutex::new(ScreencopyState::default())),
            buffer: None,
            y_invert: false,
        })
    }

    fn wait(
        &self,
        conn: &mut WaylandConnection,
        running: &AtomicBool,
        done: impl Fn(&ScreencopyState) -> bool,
    ) -> Result<bool> {
        loop {
            if done(&self.state.lock().unwrap()) {
                return Ok(true);
            }
            if !running.load(Ordering::SeqCst) {
                return Ok(false);
            }
            conn.dispatch_timeout(100)?;
        }
    }

    pub fn next_frame(
        &mut self,
        conn: &mut WaylandConnection,
        running: &AtomicBool,
    ) -> Result<Option<ShmFrame<'_>>> {
        *self.state.lock().unwrap() = ScreencopyState::default();

        let qh = conn.queue_handle();
        let frame = self.manager.capture_output(
            self.include_cursor as i32,
            &self.output,
            &qh,
            ScreencopyData(self.state.clone()),
        );

        // v3 ends buffer negotiation with buffer_done; v1/v2 send a single buffer event
        let has_buffer_done = frame.version() >= 3;
        if !self.wait(conn, running, |s| {
            s.failed || s.buffer_done || (!has_buffer_done && s.shm_info.is_some())
        })? {
            frame.destroy();
            return Ok(None);
        }

        let (format, width, height, stride) = {
            let state = self.state.lock().unwrap();
            if state.failed {
                frame.destroy();
                bail!("screencopy failed before buffer negotiation");
            }
            match state.shm_info {
                Some(info) => info,
                None => {
                    frame.destroy();
                    bail!("screencopy: compositor offered no shm buffer");
                }
            }
        };
        if !SUPPORTED_SHM_FORMATS.contains(&format) {
            frame.destroy();
            bail!(
                "screencopy: unsupported shm format {} ({:#x})",
                fourcc_name(format),
                format
            );
        }

        if !self
            .buffer
            .as_ref()
            .is_some_and(|b| b.matches(width, height, stride, format))
        {
            self.buffer = Some(ShmBuffer::new(
                &self.shm, &qh, width, height, stride, format,
            )?);
        }
        let buffer = self.buffer.as_ref().unwrap();
        frame.copy(&buffer.buffer);

        let finished = self.wait(conn, running, |s| s.ready || s.failed)?;
        frame.destroy();
        if !finished {
            return Ok(None);
        }

        let state = self.state.lock().unwrap();
        if state.failed {
            // Transient (e.g. output mode change): renegotiate next time
            self.buffer = None;
            return Ok(None);
        }
        self.y_invert = state.y_invert;
        drop(state);

        let buffer = self.buffer.as_ref().unwrap();
        Ok(Some(ShmFrame {
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            format: buffer.format,
            data: buffer.data(),
            y_invert: self.y_invert,
        }))
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyData> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        data: &ScreencopyData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut state = data.0.lock().unwrap();
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let drm = wl_shm_to_drm(wenum_raw(format));
                // Prefer the first supported format if several are offered
                let replace = match state.shm_info {
                    None => true,
                    Some((current, ..)) => !SUPPORTED_SHM_FORMATS.contains(&current),
                };
                if replace {
                    state.shm_info = Some((drm, width, height, stride));
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.y_invert = matches!(flags, WEnum::Value(f)
                    if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}
