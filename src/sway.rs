use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct SwayWorkspace {
    name: String,
    output: String,
    focused: bool,
}

/// Stores original workspace-to-output mapping for restoration
#[derive(Debug)]
pub struct WorkspaceState {
    /// Maps workspace name -> original output name
    original_mapping: HashMap<String, String>,
    source_output: String,
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

        let state = Self {
            original_mapping,
            source_output: source_output.to_string(),
        };

        // Move all workspaces from other outputs to source
        for ws in &workspaces {
            if ws.output != source_output {
                state.move_workspace(&ws.name, source_output)?;
            }
        }

        Ok(state)
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

        // Find which workspace was originally focused on source (to return to it)
        let mut focused_ws = None;

        for ws in &current_workspaces {
            if let Some(original_output) = self.original_mapping.get(&ws.name) {
                if original_output != &self.source_output && ws.output == self.source_output {
                    // This workspace was moved, restore it
                    self.move_workspace(&ws.name, original_output)?;
                }
                if ws.focused {
                    focused_ws = Some(ws.name.clone());
                }
            }
        }

        // Return focus to the originally focused workspace if known
        if let Some(ws) = focused_ws {
            let _ = Command::new("swaymsg")
                .arg(format!("workspace {}", ws))
                .output();
        }

        Ok(())
    }
}
