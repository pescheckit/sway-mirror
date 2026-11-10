use anyhow::{bail, Context, Result};
use gbm::{BufferObject, BufferObjectFlags, Format as GbmFormat, Modifier as GbmModifier};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wayland_client::{
    event_created_child,
    protocol::{wl_buffer::WlBuffer, wl_output, wl_shm},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use super::dmabuf::{CapturedFrame, DmabufPlane, DRM_FORMAT_MOD_INVALID};
use super::shm::{fourcc_name, wenum_raw, wl_shm_to_drm, ShmBuffer, SUPPORTED_SHM_FORMATS};
use super::{Frame, ShmFrame};
use crate::wayland::{AppState, WaylandConnection};

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const NUM_BUFFERS: usize = 2;
const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Buffer constraints advertised by the session (replaced on every `done`)
#[derive(Default, Clone)]
pub struct Constraints {
    width: u32,
    height: u32,
    shm_formats: Vec<u32>, // DRM fourcc
    dmabuf_device: Option<u64>,
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
}

#[derive(Default)]
pub struct SessionState {
    pending: Constraints,
    current: Constraints,
    /// Incremented on every `done`; 0 = no constraints received yet
    generation: u64,
    stopped: bool,
}

pub struct SessionData(Arc<Mutex<SessionState>>);

#[derive(Default)]
pub struct IcFrameState {
    ready: bool,
    failed: Option<WEnum<FailureReason>>,
}

pub struct IcFrameData(Arc<Mutex<IcFrameState>>);

#[derive(Default)]
pub struct ParamsState {
    result: Option<Option<WlBuffer>>,
}

pub struct ParamsData(Arc<Mutex<ParamsState>>);

struct DmabufBuffer {
    wl: WlBuffer,
    desc: CapturedFrame,
    _fds: Vec<OwnedFd>,
    _bo: BufferObject<()>,
}

impl Drop for DmabufBuffer {
    fn drop(&mut self) {
        self.wl.destroy();
    }
}

enum IcBuffer {
    Dmabuf(DmabufBuffer),
    Shm(ShmBuffer),
}

impl IcBuffer {
    fn wl(&self) -> &WlBuffer {
        match self {
            IcBuffer::Dmabuf(b) => &b.wl,
            IcBuffer::Shm(b) => &b.buffer,
        }
    }
}

/// ext-image-copy-capture-v1 capture: GPU buffers when possible, shm otherwise
pub struct ImageCopyCapture {
    session: ExtImageCopyCaptureSessionV1,
    source: ExtImageCaptureSourceV1,
    copy_manager: ExtImageCopyCaptureManagerV1,
    options: Options,
    state: Arc<Mutex<SessionState>>,
    shm: Option<wl_shm::WlShm>,
    linux_dmabuf: Option<ZwpLinuxDmabufV1>,
    /// Modifiers our EGL can import, per DRM format
    egl_modifiers: HashMap<u32, Vec<u64>>,
    allow_dmabuf: bool,
    gbm: Option<(u64, gbm::Device<File>)>,
    buffers: Vec<IcBuffer>,
    alloc_generation: u64,
    next: usize,
    y_invert: bool,
}

impl ImageCopyCapture {
    pub fn new(
        conn: &mut WaylandConnection,
        output: &wl_output::WlOutput,
        include_cursor: bool,
        egl_modifiers: HashMap<u32, Vec<u64>>,
        allow_dmabuf: bool,
    ) -> Result<Self> {
        let source_manager = conn
            .state
            .image_capture_source_manager
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("ext_output_image_capture_source_manager_v1 not available")
            })?;
        let copy_manager = conn
            .state
            .image_copy_capture_manager
            .clone()
            .ok_or_else(|| anyhow::anyhow!("ext_image_copy_capture_manager_v1 not available"))?;

        let qh = conn.queue_handle();
        let state = Arc::new(Mutex::new(SessionState::default()));
        let source = source_manager.create_source(output, &qh, ());
        let options = if include_cursor {
            Options::PaintCursors
        } else {
            Options::empty()
        };
        let session =
            copy_manager.create_session(&source, options, &qh, SessionData(state.clone()));

        let capture = Self {
            session,
            source,
            copy_manager,
            options,
            state,
            shm: conn.state.shm.clone(),
            linux_dmabuf: conn.state.linux_dmabuf.clone(),
            egl_modifiers,
            allow_dmabuf,
            gbm: None,
            buffers: Vec::new(),
            alloc_generation: 0,
            next: 0,
            y_invert: false,
        };

        // Wait for the initial buffer constraints
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            {
                let st = capture.state.lock().unwrap();
                if st.stopped {
                    bail!("ext-image-copy-capture session stopped immediately");
                }
                if st.generation > 0 {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                bail!("ext-image-copy-capture: no buffer constraints received");
            }
            conn.dispatch_timeout(100)?;
        }

        // Allocate up front so we know (and report) dmabuf vs shm
        let mut capture = capture;
        capture.allocate(conn)?;
        capture.alloc_generation = capture.state.lock().unwrap().generation;

        Ok(capture)
    }

    /// Whether frames are currently delivered as dmabufs (vs. shm)
    pub fn uses_dmabuf(&self) -> bool {
        match self.buffers.first() {
            Some(b) => matches!(b, IcBuffer::Dmabuf(_)),
            None => self.allow_dmabuf,
        }
    }

    /// Switch to shm with a fresh session, so a static screen still gets a first frame
    pub fn force_shm(&mut self, conn: &WaylandConnection) {
        self.allow_dmabuf = false;
        self.buffers.clear();
        self.alloc_generation = 0;
        self.next = 0;

        self.session.destroy();
        self.state = Arc::new(Mutex::new(SessionState::default()));
        let qh = conn.queue_handle();
        self.session = self.copy_manager.create_session(
            &self.source,
            self.options,
            &qh,
            SessionData(self.state.clone()),
        );
    }

    fn allocate(&mut self, conn: &mut WaylandConnection) -> Result<()> {
        self.buffers.clear();
        self.next = 0;
        let constraints = self.state.lock().unwrap().current.clone();

        if self.allow_dmabuf {
            match self.allocate_dmabuf(conn, &constraints) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("Warning: dmabuf allocation failed ({e:#}), using shm buffers");
                }
            }
            self.buffers.clear();
        }

        self.allocate_shm(conn, &constraints)
    }

    fn allocate_shm(&mut self, conn: &WaylandConnection, c: &Constraints) -> Result<()> {
        let shm = self
            .shm
            .clone()
            .ok_or_else(|| anyhow::anyhow!("wl_shm not available"))?;
        let format = SUPPORTED_SHM_FORMATS
            .into_iter()
            .find(|f| c.shm_formats.contains(f))
            .ok_or_else(|| {
                let offered: Vec<_> = c.shm_formats.iter().map(|&f| fourcc_name(f)).collect();
                anyhow::anyhow!("No supported shm format offered (got {:?})", offered)
            })?;
        let qh = conn.queue_handle();
        for _ in 0..NUM_BUFFERS {
            self.buffers.push(IcBuffer::Shm(ShmBuffer::new(
                &shm,
                &qh,
                c.width,
                c.height,
                c.width * 4,
                format,
            )?));
        }
        Ok(())
    }

    /// Returns Ok(false) if dmabufs aren't possible with these constraints.
    fn allocate_dmabuf(&mut self, conn: &mut WaylandConnection, c: &Constraints) -> Result<bool> {
        let (Some(dev), Some(linux_dmabuf)) = (c.dmabuf_device, self.linux_dmabuf.clone()) else {
            return Ok(false);
        };

        let Some((format, modifiers)) = pick_dmabuf_format(c, &self.egl_modifiers) else {
            return Ok(false);
        };

        if self.gbm.as_ref().map(|(d, _)| *d) != Some(dev) {
            let node = open_render_node(dev)?;
            let device = gbm::Device::new(node).context("Failed to create GBM device")?;
            self.gbm = Some((dev, device));
        }
        let gbm = &self.gbm.as_ref().unwrap().1;
        let gbm_format = GbmFormat::try_from(format)
            .map_err(|_| anyhow::anyhow!("Unknown format {:#x}", format))?;

        for _ in 0..NUM_BUFFERS {
            let bo: BufferObject<()> = if modifiers.is_empty() {
                gbm.create_buffer_object(
                    c.width,
                    c.height,
                    gbm_format,
                    BufferObjectFlags::RENDERING,
                )
            } else {
                gbm.create_buffer_object_with_modifiers2(
                    c.width,
                    c.height,
                    gbm_format,
                    modifiers.iter().map(|&m| GbmModifier::from(m)),
                    BufferObjectFlags::RENDERING,
                )
                .or_else(|_| {
                    gbm.create_buffer_object_with_modifiers(
                        c.width,
                        c.height,
                        gbm_format,
                        modifiers.iter().map(|&m| GbmModifier::from(m)),
                    )
                })
            }
            .context("GBM buffer allocation failed")?;

            let modifier = if modifiers.is_empty() {
                DRM_FORMAT_MOD_INVALID
            } else {
                u64::from(bo.modifier()?)
            };

            let plane_count = bo.plane_count()? as i32;
            let mut fds = Vec::new();
            let mut planes = Vec::new();
            for i in 0..plane_count {
                let fd = bo.fd_for_plane(i).context("Failed to export GBM plane")?;
                planes.push(DmabufPlane {
                    fd: fd.as_raw_fd(),
                    offset: bo.offset(i)?,
                    stride: bo.stride_for_plane(i)?,
                });
                fds.push(fd);
            }

            let wl = create_dmabuf_wl_buffer(
                conn,
                &linux_dmabuf,
                &fds,
                &planes,
                c.width,
                c.height,
                format,
                modifier,
            )?;

            self.buffers.push(IcBuffer::Dmabuf(DmabufBuffer {
                wl,
                desc: CapturedFrame {
                    width: c.width,
                    height: c.height,
                    format,
                    modifier,
                    planes,
                    y_invert: false,
                    fds: Vec::new(), // owned by DmabufBuffer::_fds
                },
                _fds: fds,
                _bo: bo,
            }));
        }
        Ok(true)
    }

    pub fn next_frame(
        &mut self,
        conn: &mut WaylandConnection,
        running: &AtomicBool,
    ) -> Result<Option<Frame<'_>>> {
        let mut failures = 0;
        let idx = loop {
            if !running.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let generation = {
                let st = self.state.lock().unwrap();
                if st.stopped {
                    bail!("ext-image-copy-capture session stopped by the compositor");
                }
                st.generation
            };
            if generation == 0 {
                // Waiting for the (new) session's buffer constraints
                conn.dispatch_timeout(100)?;
                continue;
            }
            if generation != self.alloc_generation {
                self.allocate(conn)?;
                self.alloc_generation = generation;
            }

            let idx = self.next;
            let (width, height) = {
                let st = self.state.lock().unwrap();
                (st.current.width as i32, st.current.height as i32)
            };
            let fs = Arc::new(Mutex::new(IcFrameState::default()));
            let qh = conn.queue_handle();
            let frame = self.session.create_frame(&qh, IcFrameData(fs.clone()));
            frame.attach_buffer(self.buffers[idx].wl());
            // We alternate buffers, so the whole buffer is stale
            frame.damage_buffer(0, 0, width, height);
            frame.capture();

            loop {
                {
                    let f = fs.lock().unwrap();
                    if f.ready || f.failed.is_some() {
                        break;
                    }
                }
                if !running.load(Ordering::SeqCst) {
                    frame.destroy();
                    return Ok(None);
                }
                conn.dispatch_timeout(100)?;
            }
            frame.destroy();

            let failed = fs.lock().unwrap().failed;
            match failed {
                None => break idx,
                Some(WEnum::Value(FailureReason::BufferConstraints)) => {
                    // New constraints follow; wait for them, then reallocate
                    while self.state.lock().unwrap().generation == generation {
                        if !running.load(Ordering::SeqCst) {
                            return Ok(None);
                        }
                        conn.dispatch_timeout(100)?;
                    }
                }
                Some(WEnum::Value(FailureReason::Stopped)) => {
                    bail!("ext-image-copy-capture session stopped by the compositor");
                }
                Some(reason) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        bail!("ext-image-copy-capture keeps failing ({:?})", reason);
                    }
                }
            }
        };

        self.next = (idx + 1) % self.buffers.len();
        Ok(Some(match &self.buffers[idx] {
            IcBuffer::Dmabuf(b) => Frame::Dmabuf(&b.desc),
            IcBuffer::Shm(b) => Frame::Shm(ShmFrame {
                width: b.width,
                height: b.height,
                stride: b.stride,
                format: b.format,
                data: b.data(),
                y_invert: self.y_invert,
            }),
        }))
    }
}

impl Drop for ImageCopyCapture {
    fn drop(&mut self) {
        self.buffers.clear();
        self.session.destroy();
        self.source.destroy();
    }
}

/// Pick a format both sides support and its modifiers (empty = implicit modifier)
fn pick_dmabuf_format(
    c: &Constraints,
    egl_modifiers: &HashMap<u32, Vec<u64>>,
) -> Option<(u32, Vec<u64>)> {
    for format in SUPPORTED_SHM_FORMATS {
        let Some((_, offered)) = c.dmabuf_formats.iter().find(|(f, _)| *f == format) else {
            continue;
        };
        let explicit: Vec<u64> = offered
            .iter()
            .copied()
            .filter(|&m| m != DRM_FORMAT_MOD_INVALID)
            .collect();

        if explicit.is_empty() {
            if offered.contains(&DRM_FORMAT_MOD_INVALID) {
                return Some((format, Vec::new()));
            }
            continue;
        }

        let importable = egl_modifiers.get(&format);
        let common: Vec<u64> = match importable {
            Some(egl) if !egl.is_empty() => {
                explicit.into_iter().filter(|m| egl.contains(m)).collect()
            }
            // EGL can't tell us: linear is the safe choice
            _ => explicit
                .into_iter()
                .filter(|&m| m == DRM_FORMAT_MOD_LINEAR)
                .collect(),
        };
        if !common.is_empty() {
            return Some((format, common));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn create_dmabuf_wl_buffer(
    conn: &mut WaylandConnection,
    linux_dmabuf: &ZwpLinuxDmabufV1,
    fds: &[OwnedFd],
    planes: &[DmabufPlane],
    width: u32,
    height: u32,
    format: u32,
    modifier: u64,
) -> Result<WlBuffer> {
    let qh = conn.queue_handle();
    let result = Arc::new(Mutex::new(ParamsState::default()));
    let params = linux_dmabuf.create_params(&qh, ParamsData(result.clone()));
    for (i, (fd, plane)) in fds.iter().zip(planes).enumerate() {
        params.add(
            fd.as_fd(),
            i as u32,
            plane.offset,
            plane.stride,
            (modifier >> 32) as u32,
            (modifier & 0xffff_ffff) as u32,
        );
    }
    params.create(
        width as i32,
        height as i32,
        format,
        zwp_linux_buffer_params_v1::Flags::empty(),
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let buffer = loop {
        if let Some(r) = result.lock().unwrap().result.take() {
            break r;
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        conn.dispatch_timeout(100)?;
    };
    params.destroy();
    buffer.ok_or_else(|| {
        anyhow::anyhow!(
            "Compositor rejected dmabuf {} modifier {:#018x}",
            fourcc_name(format),
            modifier
        )
    })
}

/// dev_t arrays are sent in native byte order
fn parse_dev_t(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
        4 => Some(u32::from_ne_bytes(bytes.try_into().ok()?) as u64),
        _ => None,
    }
}

fn parse_modifiers(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes(*c))
        .collect()
}

/// Find the DRM render node for a dev_t (which may be a primary node)
fn open_render_node(dev: u64) -> Result<File> {
    let open = |path: &std::path::Path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open {}", path.display()))
    };

    for entry in fs::read_dir("/dev/dri").context("Failed to read /dev/dri")? {
        let path = entry?.path();
        let is_render = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("renderD"));
        if is_render && fs::metadata(&path).is_ok_and(|m| m.rdev() == dev) {
            return open(&path);
        }
    }

    // Primary node (cardN): find the render node of the same device via sysfs
    let (major, minor) = (nix::libc::major(dev), nix::libc::minor(dev));
    let sys = format!("/sys/dev/char/{}:{}/device/drm", major, minor);
    if let Ok(entries) = fs::read_dir(&sys) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("renderD") {
                return open(&std::path::Path::new("/dev/dri").join(name));
            }
        }
    }
    bail!("No DRM render node found for device {}:{}", major, minor)
}

impl Dispatch<ExtImageCopyCaptureSessionV1, SessionData> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        data: &SessionData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut st = data.0.lock().unwrap();
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                st.pending.width = width;
                st.pending.height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                let f = wl_shm_to_drm(wenum_raw(format));
                st.pending.shm_formats.push(f);
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                st.pending.dmabuf_device = parse_dev_t(&device);
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                st.pending
                    .dmabuf_formats
                    .push((format, parse_modifiers(&modifiers)));
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                st.current = std::mem::take(&mut st.pending);
                st.generation += 1;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => st.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, IcFrameData> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        data: &IcFrameData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut f = data.0.lock().unwrap();
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => f.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => f.failed = Some(reason),
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ParamsData> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        data: &ParamsData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut st = data.0.lock().unwrap();
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                st.result = Some(Some(buffer));
            }
            zwp_linux_buffer_params_v1::Event::Failed => st.result = Some(None),
            _ => {}
        }
    }

    event_created_child!(AppState, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::shm::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

    fn constraints(formats: Vec<(u32, Vec<u64>)>) -> Constraints {
        Constraints {
            width: 1920,
            height: 1080,
            dmabuf_formats: formats,
            ..Default::default()
        }
    }

    #[test]
    fn parses_dev_t_and_modifiers() {
        let dev: u64 = (226 << 8) | 128;
        assert_eq!(parse_dev_t(&dev.to_ne_bytes()), Some(dev));
        assert_eq!(parse_dev_t(&[1, 2, 3]), None);

        let mods = [0u64, 0x0100_0000_0000_0009];
        let bytes: Vec<u8> = mods.iter().flat_map(|m| m.to_ne_bytes()).collect();
        assert_eq!(parse_modifiers(&bytes), mods);
    }

    #[test]
    fn picks_modifiers_both_sides_support() {
        let tiled = 0x0100_0000_0000_0009; // I915_FORMAT_MOD_4_TILED
        let c = constraints(vec![(DRM_FORMAT_XRGB8888, vec![0, tiled, 0x42])]);
        let egl = HashMap::from([(DRM_FORMAT_XRGB8888, vec![tiled, 0])]);
        assert_eq!(
            pick_dmabuf_format(&c, &egl),
            Some((DRM_FORMAT_XRGB8888, vec![0, tiled]))
        );
    }

    #[test]
    fn falls_back_to_linear_without_egl_modifier_info() {
        let c = constraints(vec![(DRM_FORMAT_XRGB8888, vec![0x42, 0])]);
        assert_eq!(
            pick_dmabuf_format(&c, &HashMap::new()),
            Some((DRM_FORMAT_XRGB8888, vec![0]))
        );
    }

    #[test]
    fn implicit_modifier_and_no_overlap() {
        let c = constraints(vec![(DRM_FORMAT_ARGB8888, vec![DRM_FORMAT_MOD_INVALID])]);
        assert_eq!(
            pick_dmabuf_format(&c, &HashMap::new()),
            Some((DRM_FORMAT_ARGB8888, vec![]))
        );

        let c = constraints(vec![(DRM_FORMAT_XRGB8888, vec![0x42])]);
        let egl = HashMap::from([(DRM_FORMAT_XRGB8888, vec![0x43])]);
        assert_eq!(pick_dmabuf_format(&c, &egl), None);
    }
}
