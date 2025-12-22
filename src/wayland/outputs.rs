use std::collections::HashMap;
use wayland_client::{protocol::wl_output, Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1;

use super::connection::AppState;

#[derive(Debug, Clone)]
pub struct Output {
    pub name: String,        // e.g., "DP-7", "eDP-1"
    pub description: String, // e.g., "Philips PHL 276E8V"
    pub width: i32,
    pub height: i32,
    pub refresh: i32, // mHz
    pub x: i32,
    pub y: i32,
    pub scale: i32,
    pub wl_output: wl_output::WlOutput,
    #[allow(dead_code)]
    pub global_name: u32,
}

impl Output {
    pub fn new(global_name: u32, wl_output: wl_output::WlOutput) -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            width: 0,
            height: 0,
            refresh: 0,
            x: 0,
            y: 0,
            scale: 1,
            wl_output,
            global_name,
        }
    }
}

pub struct OutputManager {
    pub outputs: HashMap<u32, Output>,
}

impl OutputManager {
    pub fn new() -> Self {
        Self {
            outputs: HashMap::new(),
        }
    }

    pub fn add_output(&mut self, global_name: u32, wl_output: wl_output::WlOutput) {
        self.outputs
            .insert(global_name, Output::new(global_name, wl_output));
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Output> {
        self.outputs.values().find(|o| o.name == name)
    }

    pub fn list(&self) -> Vec<&Output> {
        self.outputs.values().collect()
    }
}

// Handle wl_output events
impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let Some(output) = state.output_manager.outputs.get_mut(global_name) {
            match event {
                wl_output::Event::Mode {
                    flags: wayland_client::WEnum::Value(mode_flags),
                    width,
                    height,
                    refresh,
                } if mode_flags.contains(wl_output::Mode::Current) => {
                    output.width = width;
                    output.height = height;
                    output.refresh = refresh;
                }
                wl_output::Event::Scale { factor } => {
                    output.scale = factor;
                }
                wl_output::Event::Name { name } => {
                    output.name = name;
                }
                wl_output::Event::Description { description } => {
                    output.description = description;
                }
                _ => {}
            }
        }
    }
}

// Handle xdg_output events (for position info)
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let Some(output) = state.output_manager.outputs.get_mut(global_name) {
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    output.x = x;
                    output.y = y;
                }
                zxdg_output_v1::Event::Name { name } => {
                    // xdg_output name takes precedence
                    if !name.is_empty() {
                        output.name = name;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Request xdg_output for all outputs to get their names
pub fn request_xdg_outputs(state: &AppState, qh: &QueueHandle<AppState>) {
    if let Some(ref manager) = state.xdg_output_manager {
        for (global_name, output) in &state.output_manager.outputs {
            manager.get_xdg_output(&output.wl_output, qh, *global_name);
        }
    }
}
