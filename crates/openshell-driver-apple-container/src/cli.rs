// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Wrapper around the Apple `container` CLI binary.
//!
//! All interactions with the container runtime go through this module,
//! which shells out to the `container` CLI and parses its JSON output.

use serde::Deserialize;
use std::collections::HashMap;
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;
use tracing::debug;

/// Errors from the `container` CLI wrapper.
#[derive(Debug, Error)]
pub enum ContainerCliError {
    #[error("container CLI execution failed: {0}")]
    Exec(#[from] std::io::Error),

    #[error("container CLI returned non-zero exit ({code}): {stderr}")]
    NonZero { code: i32, stderr: String },

    #[error("failed to parse container CLI output: {0}")]
    Parse(String),

    #[error("container not found: {0}")]
    NotFound(String),

    #[error("container already exists: {0}")]
    AlreadyExists(String),
}

/// Parsed output of `container inspect`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerInspect {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: ContainerState,
    #[serde(default)]
    pub config: ContainerConfig,
    #[serde(default)]
    pub network: Option<ContainerNetwork>,
}

/// Container state from inspect output.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub exit_code: Option<i64>,
}

/// Container config from inspect output.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfig {
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// Container network info from inspect output.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerNetwork {
    #[serde(default)]
    pub ip_address: Option<String>,
}

/// Parsed entry from `container ls --format json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerListEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
}

/// Client that shells out to the `container` CLI.
#[derive(Debug, Clone)]
pub struct ContainerCli {
    bin: String,
}

impl ContainerCli {
    /// Create a new CLI wrapper using the given binary path.
    pub fn new(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Check that the container CLI is available and the system service is
    /// running.
    pub async fn verify(&self) -> Result<String, ContainerCliError> {
        // `container system info` would be ideal but may not exist.
        // Use `container ls` as a connectivity check.
        let output = self.run(&["list", "--format", "json"]).await?;
        Ok(output)
    }

    /// Create a container without starting it.
    ///
    /// Returns the container ID.
    pub async fn create(
        &self,
        image: &str,
        name: &str,
        args: &[String],
    ) -> Result<String, ContainerCliError> {
        let mut cmd_args = vec![
            "create".to_string(),
            "--name".to_string(),
            name.to_string(),
        ];
        cmd_args.extend(args.iter().cloned());
        cmd_args.push(image.to_string());

        let output = self.run_args(&cmd_args).await?;
        let container_id = output.trim().to_string();
        if container_id.is_empty() {
            return Err(ContainerCliError::Parse(
                "container create returned empty ID".to_string(),
            ));
        }
        Ok(container_id)
    }

    /// Run a container (create + start) in detached mode.
    ///
    /// Returns the container ID.
    pub async fn run_detached(
        &self,
        image: &str,
        name: &str,
        args: &[String],
        command: &[String],
    ) -> Result<String, ContainerCliError> {
        let mut cmd_args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            name.to_string(),
        ];
        cmd_args.extend(args.iter().cloned());
        cmd_args.push(image.to_string());
        cmd_args.extend(command.iter().cloned());

        let output = self.run_args(&cmd_args).await?;
        let container_id = output.trim().to_string();
        if container_id.is_empty() {
            return Err(ContainerCliError::Parse(
                "container run returned empty ID".to_string(),
            ));
        }
        Ok(container_id)
    }

    /// Start a stopped container.
    pub async fn start(&self, container_id: &str) -> Result<(), ContainerCliError> {
        self.run(&["start", container_id]).await?;
        Ok(())
    }

    /// Stop a running container.
    pub async fn stop(
        &self,
        container_id: &str,
        timeout_secs: u32,
    ) -> Result<(), ContainerCliError> {
        let timeout_str = timeout_secs.to_string();
        match self
            .run(&["stop", "--time", &timeout_str, container_id])
            .await
        {
            Ok(_) => Ok(()),
            Err(ContainerCliError::NonZero { stderr, .. })
                if stderr.contains("not found") || stderr.contains("no such") =>
            {
                Err(ContainerCliError::NotFound(container_id.to_string()))
            }
            Err(e) => Err(e),
        }
    }

    /// Remove a container (force).
    pub async fn rm(&self, container_id: &str) -> Result<(), ContainerCliError> {
        // Apple container doesn't have --force on rm, so stop first then rm.
        // Ignore stop errors (may already be stopped).
        let _ = self.stop(container_id, 5).await;
        match self.run(&["rm", container_id]).await {
            Ok(_) => Ok(()),
            Err(ContainerCliError::NonZero { stderr, .. })
                if stderr.contains("not found") || stderr.contains("no such") =>
            {
                Err(ContainerCliError::NotFound(container_id.to_string()))
            }
            Err(e) => Err(e),
        }
    }

    /// List containers matching a label filter.
    pub async fn list_all(&self) -> Result<Vec<ContainerListEntry>, ContainerCliError> {
        let output = self
            .run(&["list", "--all", "--format", "json"])
            .await?;
        if output.trim().is_empty() || output.trim() == "[]" || output.trim() == "null" {
            return Ok(Vec::new());
        }
        let entries: Vec<ContainerListEntry> =
            serde_json::from_str(&output).map_err(|e| ContainerCliError::Parse(e.to_string()))?;
        Ok(entries)
    }

    /// Inspect a container by ID or name.
    pub async fn inspect(&self, container_id: &str) -> Result<ContainerInspect, ContainerCliError> {
        let output = match self.run(&["inspect", container_id]).await {
            Ok(out) => out,
            Err(ContainerCliError::NonZero { stderr, .. })
                if stderr.contains("not found") || stderr.contains("no such") =>
            {
                return Err(ContainerCliError::NotFound(container_id.to_string()));
            }
            Err(e) => return Err(e),
        };

        // `container inspect` may return a single object or an array.
        let trimmed = output.trim();
        if trimmed.starts_with('[') {
            let entries: Vec<ContainerInspect> = serde_json::from_str(trimmed)
                .map_err(|e| ContainerCliError::Parse(e.to_string()))?;
            entries
                .into_iter()
                .next()
                .ok_or_else(|| ContainerCliError::NotFound(container_id.to_string()))
        } else {
            serde_json::from_str(trimmed).map_err(|e| ContainerCliError::Parse(e.to_string()))
        }
    }

    /// Execute a command inside a running container.
    pub async fn exec(
        &self,
        container_id: &str,
        command: &[&str],
    ) -> Result<String, ContainerCliError> {
        let mut args = vec!["exec", container_id];
        args.extend(command);
        self.run(&args).await
    }

    /// Pull an image from a registry.
    pub async fn pull(&self, image: &str) -> Result<(), ContainerCliError> {
        self.run(&["pull", image]).await?;
        Ok(())
    }

    /// Check if an image exists locally.
    pub async fn image_exists(&self, image: &str) -> bool {
        // `container image inspect` should return 0 if image exists.
        self.run(&["image", "inspect", image]).await.is_ok()
    }

    // ── internal helpers ────────────────────────────────────────────────

    async fn run(&self, args: &[&str]) -> Result<String, ContainerCliError> {
        let string_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.run_args(&string_args).await
    }

    async fn run_args(&self, args: &[String]) -> Result<String, ContainerCliError> {
        debug!(bin = %self.bin, args = ?args, "Executing container CLI");

        let output = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            debug!(
                bin = %self.bin,
                args = ?args,
                code,
                stderr = %stderr.trim(),
                "Container CLI failed"
            );

            // Check for common error patterns.
            if stderr.contains("already exists") || stderr.contains("already in use") {
                return Err(ContainerCliError::AlreadyExists(
                    stderr.trim().to_string(),
                ));
            }

            return Err(ContainerCliError::NonZero {
                code,
                stderr: stderr.trim().to_string(),
            });
        }

        Ok(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_cli_new() {
        let cli = ContainerCli::new("/usr/local/bin/container");
        assert_eq!(cli.bin, "/usr/local/bin/container");
    }

    #[test]
    fn container_cli_default_path() {
        let cli = ContainerCli::new("container");
        assert_eq!(cli.bin, "container");
    }
}
