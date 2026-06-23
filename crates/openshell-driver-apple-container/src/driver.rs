// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Apple Container compute driver.
//!
//! Manages sandbox containers using Apple's `container` CLI tool on macOS
//! with Apple Silicon. Each sandbox runs as a lightweight Linux VM via the
//! macOS Virtualization framework.

use crate::cli::{ContainerCli, ContainerCliError, ContainerEntry};
use crate::config::AppleContainerComputeConfig;
use crate::watcher::{self, WatchStream};
use openshell_core::driver_utils::{
    LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE, LABEL_SANDBOX_ID, LABEL_SANDBOX_NAME,
    LABEL_SANDBOX_NAMESPACE, SUPERVISOR_IMAGE_BINARY_PATH,
};
use openshell_core::proto::compute::v1::{
    DriverCondition, DriverSandbox, DriverSandboxStatus, GetCapabilitiesResponse,
};
use openshell_core::ComputeDriverError;
use std::path::PathBuf;
use tracing::{info, warn};

/// Container name prefix for managed sandboxes.
const CONTAINER_PREFIX: &str = "openshell-";

/// Construct the container name for a given sandbox name.
fn container_name(sandbox_name: &str) -> String {
    format!("{CONTAINER_PREFIX}{sandbox_name}")
}

impl From<ContainerCliError> for ComputeDriverError {
    fn from(err: ContainerCliError) -> Self {
        match err {
            ContainerCliError::NotFound(msg) => Self::Message(format!("not found: {msg}")),
            ContainerCliError::AlreadyExists(_) => Self::AlreadyExists,
            other => Self::Message(other.to_string()),
        }
    }
}

/// Apple Container compute driver managing sandbox containers via the
/// `container` CLI.
#[derive(Clone, Debug)]
pub struct AppleContainerComputeDriver {
    cli: ContainerCli,
    config: AppleContainerComputeConfig,
}

fn sandbox_token_host_path(sandbox_id: &str) -> Result<PathBuf, ComputeDriverError> {
    openshell_core::driver_utils::sandbox_token_path(
        "apple-container-sandbox-tokens",
        None,
        sandbox_id,
    )
    .map_err(|err| ComputeDriverError::Message(format!("resolve state dir failed: {err}")))
}

async fn write_sandbox_token_file(
    sandbox: &DriverSandbox,
) -> Result<Option<PathBuf>, ComputeDriverError> {
    let Some(spec) = sandbox.spec.as_ref() else {
        return Ok(None);
    };
    if spec.sandbox_token.is_empty() {
        return Ok(None);
    }
    let path = sandbox_token_host_path(&sandbox.id)?;
    if let Some(parent) = path.parent() {
        openshell_core::paths::create_dir_restricted(parent).map_err(|err| {
            ComputeDriverError::Message(format!(
                "create sandbox token directory {} failed: {err}",
                parent.display()
            ))
        })?;
    }
    tokio::fs::write(&path, format!("{}\n", spec.sandbox_token))
        .await
        .map_err(|err| {
            ComputeDriverError::Message(format!(
                "write sandbox token file {} failed: {err}",
                path.display()
            ))
        })?;
    openshell_core::paths::set_file_owner_only(&path).map_err(|err| {
        ComputeDriverError::Message(format!(
            "restrict sandbox token file {} failed: {err}",
            path.display()
        ))
    })?;
    Ok(Some(path))
}

fn cleanup_sandbox_token_file(sandbox_id: &str) {
    let Ok(path) = sandbox_token_host_path(sandbox_id) else {
        return;
    };
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            sandbox_id = %sandbox_id,
            path = %path.display(),
            error = %err,
            "Failed to remove Apple Container sandbox token file"
        );
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

impl AppleContainerComputeDriver {
    /// Create a new driver, verifying the `container` CLI is reachable and
    /// the system service is running.
    pub async fn new(config: AppleContainerComputeConfig) -> Result<Self, ComputeDriverError> {
        let cli = ContainerCli::new(&config.container_bin);

        match cli.verify().await {
            Ok(_) => {
                info!(
                    bin = %config.container_bin,
                    "Connected to Apple Container runtime"
                );
            }
            Err(ContainerCliError::Exec(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ComputeDriverError::Message(format!(
                    "Apple `container` CLI not found at '{}'. \
                     Install it from https://github.com/apple/container/releases \
                     and ensure `container system start` has been run.",
                    config.container_bin
                )));
            }
            Err(e) => {
                return Err(ComputeDriverError::Message(format!(
                    "Failed to connect to Apple Container runtime: {e}. \
                     Ensure `container system start` has been run."
                )));
            }
        }

        Ok(Self { cli, config })
    }

    /// Report driver capabilities.
    pub fn capabilities(&self) -> GetCapabilitiesResponse {
        openshell_core::driver_utils::build_capabilities_response(
            "apple-container",
            openshell_core::VERSION,
            &self.config.default_image,
            false, // Apple Container does not support GPU passthrough
        )
    }

    /// Validate a sandbox before creation.
    pub fn validate_sandbox_create(
        &self,
        sandbox: &DriverSandbox,
    ) -> Result<(), ComputeDriverError> {
        if sandbox.spec.as_ref().is_some_and(|s| s.gpu) {
            return Err(ComputeDriverError::Precondition(
                "Apple Container driver does not support GPU sandboxes. \
                 GPU passthrough is not available through the macOS \
                 Virtualization framework."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Create and start a sandbox container.
    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), ComputeDriverError> {
        if sandbox.name.is_empty() {
            return Err(ComputeDriverError::Precondition(
                "sandbox name is required".into(),
            ));
        }
        if sandbox.id.is_empty() {
            return Err(ComputeDriverError::Precondition(
                "sandbox id is required".into(),
            ));
        }

        let name = container_name(&sandbox.name);
        let image = self.resolve_image(sandbox);
        if image.is_empty() {
            return Err(ComputeDriverError::Precondition(
                "no sandbox image configured: set default_image in \
                 [openshell.drivers.apple-container] or provide an image \
                 in the sandbox template"
                    .to_string(),
            ));
        }

        info!(
            sandbox_id = %sandbox.id,
            sandbox_name = %sandbox.name,
            container = %name,
            image = %image,
            "Creating Apple Container sandbox"
        );

        // Ensure the image is available locally.
        self.ensure_image(image).await?;

        // Write sandbox token file if JWT auth is configured.
        let token_host_path = write_sandbox_token_file(sandbox).await?;

        // Build container run arguments.
        let run_args = self.build_run_args(sandbox, token_host_path.as_deref());

        // Build the entrypoint command for the supervisor.
        let command = self.build_supervisor_command(sandbox);

        // Run the container in detached mode.
        match self
            .cli
            .run_detached(image, &name, &run_args, &command)
            .await
        {
            Ok(container_id) => {
                info!(
                    sandbox_id = %sandbox.id,
                    sandbox_name = %sandbox.name,
                    container_id = %container_id,
                    "Apple Container sandbox started"
                );
                Ok(())
            }
            Err(ContainerCliError::AlreadyExists(_)) => {
                cleanup_sandbox_token_file(&sandbox.id);
                Err(ComputeDriverError::AlreadyExists)
            }
            Err(e) => {
                cleanup_sandbox_token_file(&sandbox.id);
                Err(ComputeDriverError::from(e))
            }
        }
    }

    /// Stop a sandbox container without deleting it.
    pub async fn stop_sandbox(&self, sandbox_name: &str) -> Result<(), ComputeDriverError> {
        let name = container_name(sandbox_name);
        info!(sandbox_name = %sandbox_name, container = %name, "Stopping Apple Container sandbox");
        self.cli
            .stop(&name, self.config.stop_timeout_secs)
            .await
            .map_err(ComputeDriverError::from)
    }

    /// Delete a sandbox container.
    pub async fn delete_sandbox(
        &self,
        sandbox_id: &str,
        sandbox_name: &str,
    ) -> Result<bool, ComputeDriverError> {
        if sandbox_id.is_empty() {
            return Err(ComputeDriverError::Precondition(
                "sandbox id is required".into(),
            ));
        }
        let name = container_name(sandbox_name);
        info!(
            sandbox_id = %sandbox_id,
            sandbox_name = %sandbox_name,
            container = %name,
            "Deleting Apple Container sandbox"
        );

        match self.cli.rm(&name).await {
            Ok(()) => {
                cleanup_sandbox_token_file(sandbox_id);
                Ok(true)
            }
            Err(ContainerCliError::NotFound(_)) => {
                cleanup_sandbox_token_file(sandbox_id);
                Ok(false)
            }
            Err(e) => Err(ComputeDriverError::from(e)),
        }
    }

    /// Fetch a single sandbox by name.
    pub async fn get_sandbox(
        &self,
        sandbox_name: &str,
    ) -> Result<Option<DriverSandbox>, ComputeDriverError> {
        let name = container_name(sandbox_name);
        match self.cli.inspect(&name).await {
            Ok(entry) => Ok(driver_sandbox_from_entry(&entry)),
            Err(ContainerCliError::NotFound(_)) => Ok(None),
            Err(e) => Err(ComputeDriverError::from(e)),
        }
    }

    /// List all managed sandboxes.
    pub async fn list_sandboxes(&self) -> Result<Vec<DriverSandbox>, ComputeDriverError> {
        let entries = self.cli.list_all().await?;

        let mut sandboxes = Vec::new();
        for entry in &entries {
            if !is_managed_entry(entry) {
                continue;
            }
            if let Some(sandbox) = driver_sandbox_from_entry(entry) {
                sandboxes.push(sandbox);
            }
        }

        sandboxes.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(sandboxes)
    }

    /// Start watching all managed sandbox containers.
    pub async fn watch_sandboxes(&self) -> Result<WatchStream, ComputeDriverError> {
        watcher::start_watch(self.cli.clone())
            .await
            .map_err(ComputeDriverError::from)
    }

    // ── internal helpers ────────────────────────────────────────────────

    fn resolve_image<'a>(&'a self, sandbox: &'a DriverSandbox) -> &'a str {
        sandbox
            .spec
            .as_ref()
            .and_then(|spec| spec.template.as_ref())
            .map(|t| t.image.as_str())
            .filter(|img| !img.trim().is_empty())
            .unwrap_or(&self.config.default_image)
    }

    async fn ensure_image(&self, image: &str) -> Result<(), ComputeDriverError> {
        let policy = self.config.image_pull_policy.trim().to_ascii_lowercase();
        match policy.as_str() {
            "" | "ifnotpresent" => {
                if self.cli.image_exists(image).await {
                    return Ok(());
                }
                self.pull_image(image).await
            }
            "always" => self.pull_image(image).await,
            "never" => {
                if self.cli.image_exists(image).await {
                    Ok(())
                } else {
                    Err(ComputeDriverError::Precondition(format!(
                        "image '{image}' not present locally and image_pull_policy=Never"
                    )))
                }
            }
            other => Err(ComputeDriverError::Precondition(format!(
                "unsupported image_pull_policy '{other}'; expected Always, IfNotPresent, or Never"
            ))),
        }
    }

    async fn pull_image(&self, image: &str) -> Result<(), ComputeDriverError> {
        info!(image = %image, "Pulling image");
        self.cli.pull(image).await.map_err(ComputeDriverError::from)
    }

    fn build_run_args(
        &self,
        sandbox: &DriverSandbox,
        token_host_path: Option<&std::path::Path>,
    ) -> Vec<String> {
        let mut args = Vec::new();

        // Labels for management.
        args.extend([
            "-l".to_string(),
            format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE}"),
        ]);
        args.extend([
            "-l".to_string(),
            format!("{LABEL_SANDBOX_ID}={}", sandbox.id),
        ]);
        args.extend([
            "-l".to_string(),
            format!("{LABEL_SANDBOX_NAME}={}", sandbox.name),
        ]);
        args.extend([
            "-l".to_string(),
            format!(
                "{LABEL_SANDBOX_NAMESPACE}={}",
                self.config.sandbox_namespace
            ),
        ]);

        // Resource limits.
        if let Some(cpus) = self.config.sandbox_cpus {
            args.extend(["--cpus".to_string(), cpus.to_string()]);
        }
        if let Some(ref memory) = self.config.sandbox_memory {
            args.extend(["--memory".to_string(), memory.clone()]);
        }

        // Environment variables.
        let log_level =
            openshell_core::driver_utils::sandbox_log_level(sandbox, &self.config.log_level);
        args.extend([
            "-e".to_string(),
            format!("OPENSHELL_LOG_LEVEL={log_level}"),
        ]);

        let grpc_endpoint = self.effective_grpc_endpoint();
        args.extend([
            "-e".to_string(),
            format!("OPENSHELL_GRPC_ENDPOINT={grpc_endpoint}"),
        ]);
        args.extend([
            "-e".to_string(),
            format!("OPENSHELL_SANDBOX_ID={}", sandbox.id),
        ]);
        args.extend([
            "-e".to_string(),
            format!("OPENSHELL_SANDBOX_NAME={}", sandbox.name),
        ]);

        // Template environment variables.
        if let Some(spec) = sandbox.spec.as_ref() {
            for (key, value) in &spec.environment {
                args.extend(["-e".to_string(), format!("{key}={value}")]);
            }
            if let Some(template) = spec.template.as_ref() {
                for (key, value) in &template.environment {
                    args.extend(["-e".to_string(), format!("{key}={value}")]);
                }
            }
        }

        // Mount supervisor binary if configured.
        if let Some(ref supervisor_bin) = self.config.supervisor_bin {
            args.extend([
                "-v".to_string(),
                format!(
                    "{}:{}",
                    supervisor_bin.display(),
                    SUPERVISOR_IMAGE_BINARY_PATH
                ),
            ]);
        }

        // Mount TLS certificates if configured.
        if let Some(ref ca) = self.config.guest_tls_ca {
            args.extend([
                "-v".to_string(),
                format!("{}:/etc/openshell/tls/client/ca.crt", ca.display()),
            ]);
        }
        if let Some(ref cert) = self.config.guest_tls_cert {
            args.extend([
                "-v".to_string(),
                format!("{}:/etc/openshell/tls/client/tls.crt", cert.display()),
            ]);
        }
        if let Some(ref key) = self.config.guest_tls_key {
            args.extend([
                "-v".to_string(),
                format!("{}:/etc/openshell/tls/client/tls.key", key.display()),
            ]);
        }

        // Mount sandbox token if available.
        if let Some(token_path) = token_host_path {
            args.extend([
                "-v".to_string(),
                format!(
                    "{}:/etc/openshell/auth/sandbox.jwt",
                    token_path.display()
                ),
            ]);
        }

        args
    }

    fn build_supervisor_command(&self, sandbox: &DriverSandbox) -> Vec<String> {
        let mut cmd = vec![SUPERVISOR_IMAGE_BINARY_PATH.to_string()];

        let grpc_endpoint = self.effective_grpc_endpoint();
        cmd.extend(["--grpc-endpoint".to_string(), grpc_endpoint]);
        cmd.extend(["--sandbox-id".to_string(), sandbox.id.clone()]);
        cmd.extend(["--sandbox-name".to_string(), sandbox.name.clone()]);

        cmd
    }

    fn effective_grpc_endpoint(&self) -> String {
        if !self.config.grpc_endpoint.is_empty() {
            return self.config.grpc_endpoint.clone();
        }
        // Apple containers can reach the host at the vmnet gateway IP
        // (typically 192.168.64.1).
        let scheme = if self.config.tls_enabled() {
            "https"
        } else {
            "http"
        };
        let port = self.config.gateway_port;
        format!("{scheme}://192.168.64.1:{port}")
    }
}

// ── Conversion helpers ──────────────────────────────────────────────────────

/// Check if a container entry is managed by OpenShell.
fn is_managed_entry(entry: &ContainerEntry) -> bool {
    entry
        .configuration
        .labels
        .get(LABEL_MANAGED_BY)
        .is_some_and(|v| v == LABEL_MANAGED_BY_VALUE)
}

/// Convert an Apple Container entry (from list or inspect) into a `DriverSandbox`.
pub fn driver_sandbox_from_entry(entry: &ContainerEntry) -> Option<DriverSandbox> {
    let labels = &entry.configuration.labels;
    if !labels
        .get(LABEL_MANAGED_BY)
        .is_some_and(|v| v == LABEL_MANAGED_BY_VALUE)
    {
        return None;
    }

    let sandbox_id = labels.get(LABEL_SANDBOX_ID)?.clone();
    let sandbox_name = labels.get(LABEL_SANDBOX_NAME)?.clone();
    let namespace = labels
        .get(LABEL_SANDBOX_NAMESPACE)
        .cloned()
        .unwrap_or_else(|| "default".to_string());

    let state = entry
        .status
        .as_ref()
        .map(|s| s.state.as_str())
        .unwrap_or("unknown");

    let condition = condition_from_state(state);

    Some(DriverSandbox {
        id: sandbox_id,
        name: sandbox_name,
        namespace,
        spec: None,
        status: Some(DriverSandboxStatus {
            sandbox_name: entry.id.clone(),
            instance_id: entry.id.clone(),
            agent_fd: String::new(),
            sandbox_fd: String::new(),
            conditions: vec![condition],
            deleting: false,
        }),
    })
}

/// Derive a `DriverCondition` from Apple Container state string.
fn condition_from_state(state: &str) -> DriverCondition {
    let lower = state.to_ascii_lowercase();
    if lower == "running" {
        DriverCondition {
            r#type: "Ready".to_string(),
            status: "False".to_string(),
            reason: "DependenciesNotReady".to_string(),
            message: "Container is running, waiting for supervisor".to_string(),
            last_transition_time: String::new(),
        }
    } else if lower == "created" || lower == "starting" {
        DriverCondition {
            r#type: "Ready".to_string(),
            status: "False".to_string(),
            reason: "Starting".to_string(),
            message: format!("Container state: {state}"),
            last_transition_time: String::new(),
        }
    } else if lower == "exited" || lower == "stopped" || lower == "dead" {
        DriverCondition {
            r#type: "Ready".to_string(),
            status: "False".to_string(),
            reason: "ContainerExited".to_string(),
            message: format!("Container state: {state}"),
            last_transition_time: String::new(),
        }
    } else {
        DriverCondition {
            r#type: "Ready".to_string(),
            status: "False".to_string(),
            reason: "Unknown".to_string(),
            message: format!("Container state: {state}"),
            last_transition_time: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ContainerConfiguration, ContainerStatus};

    #[test]
    fn container_name_prefix() {
        assert_eq!(container_name("my-sandbox"), "openshell-my-sandbox");
    }

    #[test]
    fn managed_entry_detection() {
        let managed = ContainerEntry {
            id: "test".to_string(),
            configuration: ContainerConfiguration {
                labels: [(
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            status: None,
        };
        assert!(is_managed_entry(&managed));

        let unmanaged = ContainerEntry {
            id: "other".to_string(),
            configuration: ContainerConfiguration::default(),
            status: None,
        };
        assert!(!is_managed_entry(&unmanaged));
    }

    #[test]
    fn driver_sandbox_from_entry_extracts_labels() {
        let entry = ContainerEntry {
            id: "openshell-demo".to_string(),
            configuration: ContainerConfiguration {
                labels: [
                    (
                        LABEL_MANAGED_BY.to_string(),
                        LABEL_MANAGED_BY_VALUE.to_string(),
                    ),
                    (LABEL_SANDBOX_ID.to_string(), "sb-123".to_string()),
                    (LABEL_SANDBOX_NAME.to_string(), "demo".to_string()),
                    (LABEL_SANDBOX_NAMESPACE.to_string(), "default".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            status: Some(ContainerStatus {
                state: "running".to_string(),
                ..Default::default()
            }),
        };

        let sandbox = driver_sandbox_from_entry(&entry).unwrap();
        assert_eq!(sandbox.id, "sb-123");
        assert_eq!(sandbox.name, "demo");
        assert_eq!(sandbox.namespace, "default");
        let cond = &sandbox.status.unwrap().conditions[0];
        assert_eq!(cond.reason, "DependenciesNotReady");
    }

    #[test]
    fn condition_from_running() {
        let cond = condition_from_state("running");
        assert_eq!(cond.reason, "DependenciesNotReady");
        assert_eq!(cond.status, "False");
    }

    #[test]
    fn condition_from_stopped() {
        let cond = condition_from_state("stopped");
        assert_eq!(cond.reason, "ContainerExited");
    }

    #[test]
    fn condition_from_unknown() {
        let cond = condition_from_state("weird");
        assert_eq!(cond.reason, "Unknown");
    }
}
