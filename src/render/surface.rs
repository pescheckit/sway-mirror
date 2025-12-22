use anyhow::{Context, Result};
use khronos_egl as egl;
use std::sync::{Arc, Mutex};
use wayland_client::{
    protocol::{wl_compositor, wl_output, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_egl::WlEglSurface;
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use crate::render::EglContext;
use crate::wayland::AppState;

/// Newtype wrapper for surface data
pub struct SurfaceData {
    pub configured: Arc<Mutex<bool>>,
    pub pending_size: Arc<Mutex<(u32, u32)>>,
}

pub struct MirrorSurface {
    pub wl_surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub egl_surface: WlEglSurface,
    pub egl_window_surface: egl::Surface,
    pub width: u32,
    pub height: u32,
    pub configured: Arc<Mutex<bool>>,
    pub pending_size: Arc<Mutex<(u32, u32)>>,
}

impl MirrorSurface {
    pub fn new(
        compositor: &wl_compositor::WlCompositor,
        layer_shell: &ZwlrLayerShellV1,
        output: &wl_output::WlOutput,
        egl_ctx: &EglContext,
        qh: &QueueHandle<AppState>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let configured = Arc::new(Mutex::new(false));
        let pending_size = Arc::new(Mutex::new((width, height)));

        // Create Wayland surface
        let wl_surface = compositor.create_surface(qh, ());

        // Create layer surface
        let layer_surface = layer_shell.get_layer_surface(
            &wl_surface,
            Some(output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "sway-mirror".to_string(),
            qh,
            SurfaceData {
                configured: configured.clone(),
                pending_size: pending_size.clone(),
            },
        );

        // Configure as fullscreen
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_exclusive_zone(-1); // Don't reserve space
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

        // Commit to get configure event
        wl_surface.commit();

        // Create EGL surface using the object id
        let egl_surface = WlEglSurface::new(wl_surface.id(), width as i32, height as i32)
            .context("Failed to create WlEglSurface")?;

        let egl_window_surface =
            egl_ctx.create_window_surface(egl_surface.ptr() as egl::NativeWindowType)?;

        Ok(Self {
            wl_surface,
            layer_surface,
            egl_surface,
            egl_window_surface,
            width,
            height,
            configured,
            pending_size,
        })
    }

    pub fn is_configured(&self) -> bool {
        *self.configured.lock().unwrap()
    }

    pub fn resize_if_needed(&mut self) -> bool {
        let pending = *self.pending_size.lock().unwrap();
        if pending.0 != self.width || pending.1 != self.height {
            self.width = pending.0;
            self.height = pending.1;
            self.egl_surface
                .resize(self.width as i32, self.height as i32, 0, 0);
            true
        } else {
            false
        }
    }

    pub fn commit(&self) {
        self.wl_surface.commit();
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, SurfaceData> for AppState {
    fn event(
        _state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        data: &SurfaceData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                if width > 0 && height > 0 {
                    *data.pending_size.lock().unwrap() = (width, height);
                }
                *data.configured.lock().unwrap() = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // Surface was closed
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Drop for MirrorSurface {
    fn drop(&mut self) {
        // Destroy layer surface first, then the wl_surface
        self.layer_surface.destroy();
        self.wl_surface.destroy();
    }
}
