use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Command;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct SwayWorkspace {
    name: String,
    output: String,
    focused: bool,
}

/// Stores original workspace-to-output mapping for restoration
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Maps workspace name -> original output name
    original_mapping: HashMap<String, String>,
    /// The workspace that was focused before mirroring started
    original_focused: Option<String>,
    source_output: String,
}

fn get_state_file_path() -> PathBuf {
    // Use XDG_RUNTIME_DIR (per-user, proper permissions)
    // Falls back to XDG_STATE_HOME or ~/.local/state
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(format!("{}/sway-mirror-state.json", dir));
    }
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(format!("{}/sway-mirror-state.json", dir));
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(format!("{}/.local/state/sway-mirror-state.json", home));
    }
    PathBuf::from("/run/user/1000/sway-mirror-state.json")
}

impl WorkspaceState {
    /// Query sway for current workspace layout and move all to source output
    pub fn capture_and_move_to_source(source_output: &str) -> Result<Self> {
        // Get current workspace state
        let output = Command::new("swaymsg")
            .args(["-t", "get_workspaces"])
            .output()
            .context("Failed to run swaymsg")?;

        if !output.status.success() {
            anyhow::bail!("swaymsg failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let workspaces: Vec<SwayWorkspace> = serde_json::from_slice(&output.stdout)
            .context("Failed to parse swaymsg output")?;

        // Store original mapping
        let original_mapping: HashMap<String, String> = workspaces
            .iter()
            .map(|ws| (ws.name.clone(), ws.output.clone()))
            .collect();

        // Remember which workspace was originally focused
        let original_focused = workspaces
            .iter()
            .find(|ws| ws.focused)
            .map(|ws| ws.name.clone());

        let state = Self {
            original_mapping,
            original_focused,
            source_output: source_output.to_string(),
        };

        // Move all workspaces from other outputs to source
        for ws in &workspaces {
            if ws.output != source_output {
                state.move_workspace(&ws.name, source_output)?;
            }
        }

        // Refocus the originally focused workspace (moving changes focus)
        if let Some(ref focused) = state.original_focused {
            let _ = Command::new("swaymsg")
                .arg(format!("workspace {}", focused))
                .output();
        }

        // Save state to disk so it can be restored if process is killed
        state.save_to_file()?;

        Ok(state)
    }

    /// Save workspace state to disk
    fn save_to_file(&self) -> Result<()> {
        let path = get_state_file_path();
        let json = serde_json::to_string(self)
            .context("Failed to serialize workspace state")?;
        fs::write(&path, json)
            .context("Failed to write workspace state file")?;
        Ok(())
    }

    /// Load workspace state from disk
    pub fn load_from_file() -> Result<Option<Self>> {
        let path = get_state_file_path();
        if !path.exists() {
            return Ok(None);
        }
        let json = fs::read_to_string(&path)
            .context("Failed to read workspace state file")?;
        let state: Self = serde_json::from_str(&json)
            .context("Failed to parse workspace state file")?;
        Ok(Some(state))
    }

    /// Remove the state file
    pub fn remove_state_file() {
        let _ = fs::remove_file(get_state_file_path());
    }

    /// Restore workspaces from saved state file (used by --stop)
    pub fn restore_from_file() -> Result<()> {
        if let Some(state) = Self::load_from_file()? {
            state.restore()?;
            Self::remove_state_file();
        }
        Ok(())
    }

    /// Move a workspace to an output
    fn move_workspace(&self, workspace: &str, output: &str) -> Result<()> {
        // First focus the workspace, then move it
        let cmd = format!(
            "workspace {}; move workspace to output {}",
            workspace, output
        );

        let result = Command::new("swaymsg")
            .arg(&cmd)
            .output()
            .context("Failed to run swaymsg")?;

        if !result.status.success() {
            eprintln!("Warning: Failed to move workspace {} to {}: {}",
                workspace, output, String::from_utf8_lossy(&result.stderr));
        }

        Ok(())
    }

    /// Restore all workspaces to their original outputs
    pub fn restore(&self) -> Result<()> {
        // Get current workspace state to know what exists
        let output = Command::new("swaymsg")
            .args(["-t", "get_workspaces"])
            .output()
            .context("Failed to run swaymsg")?;

        let current_workspaces: Vec<SwayWorkspace> = serde_json::from_slice(&output.stdout)
            .unwrap_or_default();

        // Move workspaces back to their original outputs
        for ws in &current_workspaces {
            if let Some(original_output) = self.original_mapping.get(&ws.name) {
                if original_output != &self.source_output && ws.output == self.source_output {
                    // This workspace was moved, restore it
                    self.move_workspace(&ws.name, original_output)?;
                }
            }
        }

        // Return focus to the originally focused workspace (from before mirroring started)
        if let Some(ref ws) = self.original_focused {
            let _ = Command::new("swaymsg")
                .arg(format!("workspace {}", ws))
                .output();
        }

        Ok(())
    }
}
