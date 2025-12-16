use anyhow::{Context, Result};
use wayland_client::{
    protocol::{wl_compositor, wl_registry, wl_output},
    Connection, Dispatch, QueueHandle, EventQueue,
};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;
use std::ops::{Deref, DerefMut};

use super::outputs::OutputManager;

/// Global state for Wayland connection
pub struct WaylandState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    pub dmabuf_manager: Option<zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1>,
    pub xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    pub output_manager: OutputManager,
}

impl WaylandState {
    pub fn new() -> Self {
        Self {
            compositor: None,
            layer_shell: None,
            dmabuf_manager: None,
            xdg_output_manager: None,
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
        let connection = Connection::connect_to_env()
            .context("Failed to connect to Wayland display")?;

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
        if let wl_registry::Event::Global { name, interface, version } = event {
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
    ) {}
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
        _event: zwlr_export_dmabuf_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _event: zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}
