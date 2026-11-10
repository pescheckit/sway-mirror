use anyhow::{Context, Result};
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;
use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

use super::outputs::OutputManager;

/// Global state for Wayland connection
pub struct WaylandState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    pub dmabuf_manager: Option<zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1>,
    pub xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    pub shm: Option<wl_shm::WlShm>,
    pub linux_dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    pub screencopy_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    pub image_capture_source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    pub image_copy_capture_manager:
        Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    pub output_manager: OutputManager,
}

impl WaylandState {
    pub fn new() -> Self {
        Self {
            compositor: None,
            layer_shell: None,
            dmabuf_manager: None,
            xdg_output_manager: None,
            shm: None,
            linux_dmabuf: None,
            screencopy_manager: None,
            image_capture_source_manager: None,
            image_copy_capture_manager: None,
            output_manager: OutputManager::new(),
        }
    }
}

/// Newtype wrapper to satisfy orphan rules
pub struct AppState(pub WaylandState);

impl Deref for AppState {
    type Target = WaylandState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for AppState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct WaylandConnection {
    pub connection: Connection,
    pub state: AppState,
    pub queue: EventQueue<AppState>,
}

impl WaylandConnection {
    pub fn connect() -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("Failed to connect to Wayland display")?;

        let mut state = AppState(WaylandState::new());
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();

        let display = connection.display();
        display.get_registry(&qh, ());

        // Initial roundtrip to get globals
        queue.roundtrip(&mut state)?;

        // Second roundtrip to get xdg_output info
        queue.roundtrip(&mut state)?;

        Ok(Self {
            connection,
            state,
            queue,
        })
    }

    pub fn roundtrip(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state)?;
        Ok(())
    }

    pub fn dispatch(&mut self) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.queue.flush()?;
        Ok(())
    }

    /// Dispatch events, waiting up to `timeout_ms` so idle waits don't spin or block Ctrl+C
    pub fn dispatch_timeout(&mut self, timeout_ms: i32) -> Result<()> {
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(());
        }
        self.queue.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let mut pfd = nix::libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { nix::libc::poll(&mut pfd, 1, timeout_ms) };
            if ret > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn queue_handle(&self) -> QueueHandle<AppState> {
        self.queue.handle()
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(5), qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "zwlr_export_dmabuf_manager_v1" => {
                    state.dmabuf_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    // create_immed needs v2; v4 avoids the legacy format spam
                    if version >= 2 {
                        state.linux_dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.image_capture_source_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.image_copy_capture_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_output_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, name);
                    state.output_manager.add_output(name, output);
                }
                _ => {}
            }
        }
    }
}

// Empty dispatchers for globals we just store
impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
        _event: zwlr_export_dmabuf_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _event: zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Dispatchers for objects whose events we don't care about
macro_rules! ignore_events {
    ($($iface:ty),* $(,)?) => {
        $(
            impl Dispatch<$iface, ()> for AppState {
                fn event(
                    _state: &mut Self,
                    _proxy: &$iface,
                    _event: <$iface as wayland_client::Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qh: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
);
