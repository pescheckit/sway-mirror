mod wayland;
mod capture;
mod render;
mod sway;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use nix::libc;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::fs;
use std::io::Write;
use std::process;

use wayland::outputs::request_xdg_outputs;
use capture::DmabufCapture;
use render::{EglContext, MirrorSurface, ScaleMode};
use sway::WorkspaceState;

fn get_pid_file_path() -> String {
    // Use XDG_RUNTIME_DIR for security (per-user, proper permissions)
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{}/sway-mirror.pid", dir);
    }
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return format!("{}/sway-mirror.pid", dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return format!("{}/.local/state/sway-mirror.pid", home);
    }
    "/run/user/1000/sway-mirror.pid".to_string()
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum ScaleModeArg {
    /// Preserve aspect ratio, fit within target (letterbox/pillarbox)
    #[default]
    Fit,
    /// Preserve aspect ratio, fill target completely (crops edges)
    Fill,
    /// Stretch to fill target, ignoring aspect ratio
    Stretch,
    /// Display at 1:1 pixel ratio, centered (no scaling)
    Center,
}

impl From<ScaleModeArg> for ScaleMode {
    fn from(arg: ScaleModeArg) -> Self {
        match arg {
            ScaleModeArg::Fit => ScaleMode::Fit,
            ScaleModeArg::Fill => ScaleMode::Fill,
            ScaleModeArg::Stretch => ScaleMode::Stretch,
            ScaleModeArg::Center => ScaleMode::Center,
        }
    }
}

#[derive(Parser)]
#[command(name = "sway-mirror")]
#[command(about = "Fast zero-copy screen mirroring for Sway")]
struct Cli {
    /// Source output to mirror (e.g., eDP-1, DP-7)
    source: Option<String>,

    /// Target outputs (if not specified, mirrors to all other outputs)
    #[arg(short, long)]
    to: Vec<String>,

    /// List available outputs and exit
    #[arg(short, long)]
    list: bool,

    /// Include cursor in mirror
    #[arg(long, default_value = "true")]
    cursor: bool,

    /// Scaling mode for the mirrored content
    #[arg(short, long, value_enum, default_value = "fit")]
    scale: ScaleModeArg,

    /// Move all workspaces to source output while mirroring (restores on exit)
    #[arg(short, long, default_value = "true")]
    workspaces: bool,

    /// Stop a running sway-mirror instance
    #[arg(long)]
    stop: bool,
}

fn write_pid_file() -> Result<()> {
    let pid_file = get_pid_file_path();
    let mut file = fs::File::create(&pid_file)?;
    writeln!(file, "{}", process::id())?;
    Ok(())
}

fn remove_pid_file() {
    let _ = fs::remove_file(get_pid_file_path());
}

fn is_sway_mirror_process(pid: i32) -> bool {
    // Verify the process is actually sway-mirror by checking /proc/<pid>/comm
    let comm_path = format!("/proc/{}/comm", pid);
    if let Ok(comm) = fs::read_to_string(&comm_path) {
        return comm.trim() == "sway-mirror";
    }
    false
}

fn stop_running_instance() -> Result<()> {
    let pid_file = get_pid_file_path();
    let pid_str = fs::read_to_string(&pid_file)
        .map_err(|_| anyhow::anyhow!("No running sway-mirror instance found (no PID file)"))?;

    let pid: i32 = pid_str.trim().parse()
        .map_err(|_| anyhow::anyhow!("Invalid PID in file"))?;

    // Verify it's actually sway-mirror before killing
    if !is_sway_mirror_process(pid) {
        remove_pid_file();
        bail!("PID {} is not a sway-mirror process (stale PID file removed)", pid);
    }

    // Send SIGTERM
    unsafe {
        if libc::kill(pid, libc::SIGTERM) == 0 {
            println!("Sent stop signal to sway-mirror (PID {})", pid);

            // Wait a moment for process to exit
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Restore workspaces from saved state file
            if let Err(e) = WorkspaceState::restore_from_file() {
                eprintln!("Warning: Failed to restore workspaces: {}", e);
            } else {
                println!("Restored workspaces to original outputs");
            }

            // Clean up PID file if still there
            remove_pid_file();

            Ok(())
        } else {
            // Process might not exist anymore, clean up PID file
            remove_pid_file();
            bail!("Process {} not found (stale PID file removed)", pid)
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle --stop
    if cli.stop {
        return stop_running_instance();
    }

    // Check if already running
    let pid_file = get_pid_file_path();
    if fs::metadata(&pid_file).is_ok() {
        if let Ok(pid_str) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                // Verify it's actually sway-mirror and still running
                if is_sway_mirror_process(pid) {
                    unsafe {
                        if libc::kill(pid, 0) == 0 {
                            bail!("sway-mirror is already running (PID {}). Use --stop to stop it.", pid);
                        }
                    }
                }
            }
        }
        // Stale PID file, remove it
        remove_pid_file();
    }

    // Connect to Wayland
    let mut conn = wayland::WaylandConnection::connect()?;

    // Request xdg_output info
    {
        let qh = conn.queue_handle();
        request_xdg_outputs(&conn.state, &qh);
    }
    conn.roundtrip()?;

    // Handle --list
    if cli.list {
        println!("Available outputs:");
        for output in conn.state.output_manager.list() {
            println!("  {} - {} ({}x{})",
                output.name,
                output.description,
                output.width,
                output.height
            );
        }
        return Ok(());
    }

    // Require source
    let source_name = cli.source.ok_or_else(|| anyhow::anyhow!("Source output required. Use --list to see available outputs."))?;

    // Find source output
    let source_output = {
        let source = conn.state.output_manager.get_by_name(&source_name)
            .ok_or_else(|| anyhow::anyhow!("Source output '{}' not found", source_name))?;
        source.wl_output.clone()
    };

    // Determine target outputs
    let target_outputs: Vec<_> = {
        if cli.to.is_empty() {
            // All outputs except source
            conn.state.output_manager.list()
                .into_iter()
                .filter(|o| o.name != source_name)
                .map(|o| (o.name.clone(), o.wl_output.clone(), o.width as u32, o.height as u32))
                .collect()
        } else {
            // Specified targets
            cli.to.iter()
                .filter_map(|name| {
                    conn.state.output_manager.get_by_name(name)
                        .map(|o| (o.name.clone(), o.wl_output.clone(), o.width as u32, o.height as u32))
                })
                .collect()
        }
    };

    if target_outputs.is_empty() {
        bail!("No target outputs found");
    }

    println!("Mirroring {} to: (scale: {:?})", source_name, cli.scale);
    for (name, _, w, h) in &target_outputs {
        println!("  {} ({}x{})", name, w, h);
    }

    // Move all workspaces to source output
    let workspace_state = if cli.workspaces {
        match WorkspaceState::capture_and_move_to_source(&source_name) {
            Ok(state) => {
                println!("Moved all workspaces to {}", source_name);
                Some(state)
            }
            Err(e) => {
                eprintln!("Warning: Could not move workspaces: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Set up Ctrl+C handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl+C handler");

    // Initialize EGL
    let wayland_display = conn.connection.backend().display_ptr() as *mut c_void;
    let mut egl_ctx = EglContext::new(wayland_display)?;

    // Initialize GL (need a surfaceless context first)
    egl_ctx.make_current_surfaceless()?;
    egl_ctx.init_gl()?;

    // Create mirror surfaces for each target
    let mut surfaces: Vec<MirrorSurface> = Vec::new();
    {
        let compositor = conn.state.compositor.as_ref()
            .ok_or_else(|| anyhow::anyhow!("wl_compositor not available"))?;
        let layer_shell = conn.state.layer_shell.as_ref()
            .ok_or_else(|| anyhow::anyhow!("zwlr_layer_shell_v1 not available"))?;

        let qh = conn.queue_handle();

        for (name, wl_output, width, height) in &target_outputs {
            let surface = MirrorSurface::new(
                compositor,
                layer_shell,
                wl_output,
                &egl_ctx,
                &qh,
                *width,
                *height,
            ).map_err(|e| anyhow::anyhow!("Failed to create surface for {}: {}", name, e))?;
            surfaces.push(surface);
        }
    }

    // Wait for surfaces to be configured
    while surfaces.iter().any(|s| !s.is_configured()) {
        conn.roundtrip()?;
    }

    // Commit initial frames
    for surface in &surfaces {
        surface.commit();
    }
    conn.roundtrip()?;

    // Setup dmabuf capture
    let capture = DmabufCapture::new();

    // Write PID file
    write_pid_file()?;

    println!("Mirror active. Press Ctrl+C or use --stop to stop.");

    // Main loop
    while running.load(Ordering::SeqCst) {
        // Check for resize
        for surface in &mut surfaces {
            surface.resize_if_needed();
        }

        // Request frame capture
        {
            let dmabuf_manager = conn.state.dmabuf_manager.as_ref()
                .ok_or_else(|| anyhow::anyhow!("zwlr_export_dmabuf_manager_v1 not available"))?;
            let qh = conn.queue_handle();
            capture.request_frame(dmabuf_manager, &source_output, &qh, cli.cursor);
        }

        // Wait for frame
        while !capture.is_done() && running.load(Ordering::SeqCst) {
            conn.roundtrip()?;
        }

        // Render to all targets
        if let Some(frame) = capture.take_frame() {
            let scale_mode: ScaleMode = cli.scale.into();
            for surface in &surfaces {
                egl_ctx.render_frame(
                    &frame,
                    surface.egl_window_surface,
                    surface.width as i32,
                    surface.height as i32,
                    scale_mode,
                )?;
                surface.commit();
            }
        }

        conn.dispatch()?;
    }

    // Cleanup on exit
    println!("\nStopping mirror...");

    // Explicitly drop surfaces to destroy layer surfaces
    drop(surfaces);
    // Flush destroy commands to compositor
    let _ = conn.roundtrip();

    // Remove PID file
    remove_pid_file();

    // Restore workspaces (use in-memory state if available, otherwise cleanup state file)
    if let Some(state) = workspace_state {
        if let Err(e) = state.restore() {
            eprintln!("Warning: Failed to restore workspaces: {}", e);
        } else {
            println!("Restored workspaces to original outputs");
        }
        // Remove state file since we restored from memory
        WorkspaceState::remove_state_file();
    }

    Ok(())
}
