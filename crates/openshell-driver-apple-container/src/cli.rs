// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Wrapper around the Apple `container` CLI binary.
//!
//! All interactions with the container runtime go through this module,
//! which shells out to the `container` CLI and parses its JSON output.
//!
//! Apple's `container` CLI uses a different JSON schema from Docker/Podman:
//! - `id` is the user-assigned container name (not a hash)
//! - Labels live under `configuration.labels`
//! - State is at `status.state`
//! - Both `list` and `inspect` return the same full schema

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

// ── Apple Container JSON schema ─────────────────────────────────────────────
//
// Both `container list --format json` and `container inspect` return the same
// top-level array of `ContainerEntry` objects.

/// Top-level container object returned by `list` and `inspect`.
#[derive(Debug, Clone, Deserialize)]
pub struct ContainerEntry {
    /// Container ID — in Apple's `container` this is the user-assigned name.
    pub id: String,
    /// Full configuration snapshot.
    #[serde(default)]
    pub configuration: ContainerConfiguration,
    /// Runtime status (present when the container has been started).
    #[serde(default)]
    pub status: Option<ContainerStatus>,
}

/// Container configuration from `container inspect` / `container list`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerConfiguration {
    /// User-assigned labels.
    pub labels: HashMap<String, String>,
    /// Image reference and descriptor.
    pub image: Option<ContainerImage>,
    /// Resource allocation.
    pub resources: Option<ContainerResources>,
    /// Published TCP ports.
    pub published_ports: Vec<PublishedPort>,
    /// Published unix sockets.
    pub published_sockets: Vec<PublishedSocket>,
}

/// Image metadata embedded in the configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContainerImage {
    /// Image reference string (e.g. `docker.io/library/alpine:latest`).
    pub reference: String,
}

/// Resource allocation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerResources {
    pub cpus: Option<u32>,
    pub memory_in_bytes: Option<u64>,
}

/// Published port mapping.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PublishedPort {
    pub host_port: Option<u16>,
    pub container_port: Option<u16>,
    pub protocol: Option<String>,
}

/// Published socket mapping.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PublishedSocket {
    pub host_path: Option<String>,
    pub container_path: Option<String>,
}

/// Runtime status.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerStatus {
    /// Container state: `running`, `stopped`, etc.
    pub state: String,
    /// Network attachments.
    pub networks: Vec<ContainerNetwork>,
    /// When the container started.
    pub started_date: Option<String>,
}

/// Network attachment info.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerNetwork {
    pub network: String,
    pub hostname: Option<String>,
    pub ipv4_address: Option<String>,
    pub ipv4_gateway: Option<String>,
    pub mac_address: Option<String>,
}

// ── CLI client ──────────────────────────────────────────────────────────────

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
        let output = self.run(&["list", "--format", "json"]).await?;
        Ok(output)
    }

    /// Run a container (create + start) in detached mode.
    ///
    /// Returns the container ID (name).
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

    /// Remove a container. Stops it first if necessary.
    pub async fn rm(&self, container_id: &str) -> Result<(), ContainerCliError> {
        // Stop first (ignore errors — may already be stopped).
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

    /// List all containers (including stopped).
    pub async fn list_all(&self) -> Result<Vec<ContainerEntry>, ContainerCliError> {
        let output = self.run(&["list", "--all", "--format", "json"]).await?;
        parse_container_entries(&output)
    }

    /// Inspect a container by ID (name).
    pub async fn inspect(&self, container_id: &str) -> Result<ContainerEntry, ContainerCliError> {
        let output = match self.run(&["inspect", container_id]).await {
            Ok(out) => out,
            Err(ContainerCliError::NonZero { stderr, .. })
                if stderr.contains("not found") || stderr.contains("no such") =>
            {
                return Err(ContainerCliError::NotFound(container_id.to_string()));
            }
            Err(e) => return Err(e),
        };

        let entries = parse_container_entries(&output)?;
        entries
            .into_iter()
            .next()
            .ok_or_else(|| ContainerCliError::NotFound(container_id.to_string()))
    }

    /// Pull an image from a registry.
    pub async fn pull(&self, image: &str) -> Result<(), ContainerCliError> {
        self.run(&["image", "pull", image]).await?;
        Ok(())
    }

    /// Check if an image exists locally.
    pub async fn image_exists(&self, image: &str) -> bool {
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

            if stderr.contains("already exists") || stderr.contains("already in use") {
                return Err(ContainerCliError::AlreadyExists(stderr.trim().to_string()));
            }

            return Err(ContainerCliError::NonZero {
                code,
                stderr: stderr.trim().to_string(),
            });
        }

        Ok(stdout)
    }
}

/// Parse the JSON output from `container list` or `container inspect`.
///
/// Both commands return an array of [`ContainerEntry`] objects.
fn parse_container_entries(output: &str) -> Result<Vec<ContainerEntry>, ContainerCliError> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|e| {
        ContainerCliError::Parse(format!(
            "{e} (input starts with: {})",
            &trimmed[..trimmed.len().min(200)]
        ))
    })
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
    fn parse_empty_list() {
        assert!(parse_container_entries("").unwrap().is_empty());
        assert!(parse_container_entries("[]").unwrap().is_empty());
        assert!(parse_container_entries("null").unwrap().is_empty());
    }

    #[test]
    fn parse_real_list_output() {
        let json = r#"[{
            "id": "test-sandbox",
            "configuration": {
                "labels": {
                    "openshell.ai/managed-by": "openshell",
                    "openshell.ai/sandbox-id": "sb-123"
                },
                "image": { "reference": "docker.io/library/alpine:latest" },
                "publishedPorts": [],
                "publishedSockets": []
            },
            "status": {
                "state": "running",
                "networks": [{
                    "ipv4Address": "192.168.64.3/24",
                    "ipv4Gateway": "192.168.64.1"
                }]
            }
        }]"#;

        let entries = parse_container_entries(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "test-sandbox");
        assert_eq!(
            entries[0]
                .configuration
                .labels
                .get("openshell.ai/sandbox-id"),
            Some(&"sb-123".to_string())
        );
        assert_eq!(entries[0].status.as_ref().unwrap().state, "running");
    }

    #[test]
    fn parse_inspect_with_resources() {
        let json = r#"[{
            "id": "my-container",
            "configuration": {
                "labels": {},
                "resources": { "cpus": 4, "memoryInBytes": 1073741824 },
                "publishedPorts": [],
                "publishedSockets": []
            },
            "status": { "state": "running", "networks": [] }
        }]"#;

        let entries = parse_container_entries(json).unwrap();
        let res = entries[0].configuration.resources.as_ref().unwrap();
        assert_eq!(res.cpus, Some(4));
        assert_eq!(res.memory_in_bytes, Some(1_073_741_824));
    }
}
