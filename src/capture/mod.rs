pub mod dmabuf;
pub mod image_copy;
pub mod screencopy;
pub mod shm;

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use wayland_client::protocol::wl_output;

pub use dmabuf::{CapturedFrame, DRM_FORMAT_MOD_INVALID};
pub use shm::fourcc_name;

use crate::wayland::WaylandConnection;
use dmabuf::ExportDmabufCapture;
use image_copy::ImageCopyCapture;
use screencopy::ScreencopyCapture;

/// CPU-readable frame in shared memory
pub struct ShmFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32, // DRM fourcc
    pub data: &'a [u8],
    pub y_invert: bool,
}

pub enum Frame<'a> {
    /// GPU buffer, imported zero-copy
    Dmabuf(&'a CapturedFrame),
    /// Shared memory, uploaded to a texture
    Shm(ShmFrame<'a>),
}

impl Frame<'_> {
    pub fn describe(&self) -> String {
        match self {
            Frame::Dmabuf(f) => format!(
                "dmabuf {}x{} {} modifier 0x{:016x} planes {}",
                f.width,
                f.height,
                fourcc_name(f.format),
                f.modifier,
                f.planes.len()
            ),
            Frame::Shm(f) => format!(
                "shm {}x{} {} stride {}",
                f.width,
                f.height,
                fourcc_name(f.format),
                f.stride
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMethod {
    /// Pick the best available method and fall back on failure
    Auto,
    /// wlr-export-dmabuf (zero-copy)
    ExportDmabuf,
    /// ext-image-copy-capture (zero-copy when possible, else shm)
    ImageCopy,
    /// wlr-screencopy (shm, CPU copy)
    Screencopy,
}

pub enum Capturer {
    ExportDmabuf(ExportDmabufCapture),
    ImageCopy(ImageCopyCapture),
    Screencopy(ScreencopyCapture),
}

impl Capturer {
    /// Create the capturer for `method`. With `Auto`, tries export-dmabuf,
    /// then ext-image-copy-capture, then wlr-screencopy.
    pub fn new(
        method: CaptureMethod,
        conn: &mut WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
        egl_modifiers: &HashMap<u32, Vec<u64>>,
    ) -> Result<Self> {
        let order: &[CaptureMethod] = match method {
            CaptureMethod::Auto => &[
                CaptureMethod::ExportDmabuf,
                CaptureMethod::ImageCopy,
                CaptureMethod::Screencopy,
            ],
            CaptureMethod::ExportDmabuf => &[CaptureMethod::ExportDmabuf],
            CaptureMethod::ImageCopy => &[CaptureMethod::ImageCopy],
            CaptureMethod::Screencopy => &[CaptureMethod::Screencopy],
        };

        let mut errors = Vec::new();
        for &m in order {
            match Self::create(m, conn, output, include_cursor, egl_modifiers, true) {
                Ok(c) => return Ok(c),
                Err(e) => errors.push(format!("{}: {:#}", method_name(m), e)),
            }
        }
        bail!("No usable capture method:\n  {}", errors.join("\n  "))
    }

    fn create(
        method: CaptureMethod,
        conn: &mut WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
        egl_modifiers: &HashMap<u32, Vec<u64>>,
        allow_dmabuf: bool,
    ) -> Result<Self> {
        Ok(match method {
            CaptureMethod::ExportDmabuf => {
                Capturer::ExportDmabuf(ExportDmabufCapture::new(conn, output, include_cursor)?)
            }
            CaptureMethod::ImageCopy => Capturer::ImageCopy(ImageCopyCapture::new(
                conn,
                output,
                include_cursor,
                egl_modifiers.clone(),
                allow_dmabuf,
            )?),
            CaptureMethod::Screencopy => {
                Capturer::Screencopy(ScreencopyCapture::new(conn, output, include_cursor)?)
            }
            CaptureMethod::Auto => unreachable!(),
        })
    }

    /// Switch to a CPU copy after a dmabuf capture/import failure.
    /// ext-image-copy-capture can switch to shm buffers itself; switching
    /// to another protocol only happens if `switch_protocol` is set.
    /// Returns false if there is nothing left to fall back to.
    pub fn fallback(
        &mut self,
        conn: &mut WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
        switch_protocol: bool,
    ) -> bool {
        match self {
            Capturer::ImageCopy(ic) if ic.uses_dmabuf() => {
                ic.force_shm(conn);
                true
            }
            Capturer::ExportDmabuf(_) if switch_protocol => {
                let empty = HashMap::new();
                let next = [CaptureMethod::ImageCopy, CaptureMethod::Screencopy]
                    .into_iter()
                    .find_map(|m| {
                        Self::create(m, conn, output, include_cursor, &empty, false).ok()
                    });
                match next {
                    Some(c) => {
                        *self = c;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Whether frames are delivered as GPU buffers (vs. a CPU copy)
    pub fn is_zero_copy(&self) -> bool {
        match self {
            Capturer::ExportDmabuf(_) => true,
            Capturer::ImageCopy(ic) => ic.uses_dmabuf(),
            Capturer::Screencopy(_) => false,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Capturer::ExportDmabuf(_) => "wlr-export-dmabuf (zero-copy)".into(),
            Capturer::ImageCopy(ic) if ic.uses_dmabuf() => {
                "ext-image-copy-capture (zero-copy)".into()
            }
            Capturer::ImageCopy(_) => "ext-image-copy-capture (shm, CPU copy)".into(),
            Capturer::Screencopy(_) => "wlr-screencopy (shm, CPU copy)".into(),
        }
    }

    /// Wait for the next frame. Ok(None) means "nothing this time" (Ctrl+C
    /// or a transient failure) and the caller should check `running`.
    pub fn next_frame(
        &mut self,
        conn: &mut WaylandConnection,
        running: &AtomicBool,
    ) -> Result<Option<Frame<'_>>> {
        match self {
            Capturer::ExportDmabuf(c) => Ok(c.next_frame(conn, running)?.map(Frame::Dmabuf)),
            Capturer::ImageCopy(c) => c.next_frame(conn, running),
            Capturer::Screencopy(c) => Ok(c.next_frame(conn, running)?.map(Frame::Shm)),
        }
    }
}

fn method_name(m: CaptureMethod) -> &'static str {
    match m {
        CaptureMethod::Auto => "auto",
        CaptureMethod::ExportDmabuf => "export-dmabuf",
        CaptureMethod::ImageCopy => "image-copy",
        CaptureMethod::Screencopy => "screencopy",
    }
}
