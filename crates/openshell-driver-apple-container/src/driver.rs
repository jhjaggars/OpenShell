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
use openshell_core::{ComputeDriverError, sandbox_env};
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

        // Validate the effective gRPC endpoint is resolvable. When no
        // explicit grpc_endpoint is set, host_gateway_ip and gateway_port
        // must both be usable.
        if config.grpc_endpoint.is_empty() {
            if config.host_gateway_ip.trim().is_empty() {
                return Err(ComputeDriverError::Message(
                    "host_gateway_ip must not be empty when grpc_endpoint is unset. \
                     Set grpc_endpoint or host_gateway_ip in \
                     [openshell.drivers.apple-container]."
                        .to_string(),
                ));
            }
            if config.gateway_port == 0 {
                warn!(
                    "gateway_port is 0 and grpc_endpoint is unset — the \
                     auto-detected gRPC endpoint will use port 0. The server \
                     normally injects the gateway port; if you are constructing \
                     the driver manually, set grpc_endpoint explicitly."
                );
            }
        }

        if config.default_image.trim().is_empty() {
            return Err(ComputeDriverError::Message(
                "default_image must not be empty. Set default_image in \
                 [openshell.drivers.apple-container]."
                    .to_string(),
            ));
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
        if self.config.supervisor_bin.is_none() {
            return Err(ComputeDriverError::Precondition(
                "supervisor_bin is required for the Apple Container driver. \
                 Set supervisor_bin in [openshell.drivers.apple-container] \
                 to the path of a Linux arm64 openshell-sandbox binary."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Create and start a sandbox container.
    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), ComputeDriverError> {
        self.validate_sandbox_create(sandbox)?;

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
        let image = self.resolve_image(sandbox).to_string();
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
        self.ensure_image(&image).await?;

        // Write sandbox token file if JWT auth is configured.
        let token_host_path = write_sandbox_token_file(sandbox).await?;

        // Build container run arguments.
        let run_args = self.build_run_args(sandbox, token_host_path.as_deref());

        // Build the entrypoint command for the supervisor.
        let command = self.build_supervisor_command();

        // Run the container in detached mode.
        match self
            .cli
            .run_detached(&image, &name, &run_args, &command)
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

        // The supervisor needs CAP_NET_ADMIN and CAP_SYS_ADMIN for
        // network namespace creation and iptables-based policy enforcement.
        args.extend(["--cap-add".to_string(), "NET_ADMIN".to_string()]);
        args.extend(["--cap-add".to_string(), "SYS_ADMIN".to_string()]);

        // ── Supervisor environment variables ────────────────────────────
        // Use the canonical names from openshell_core::sandbox_env so the
        // supervisor discovers gateway connectivity, identity, and paths.
        let log_level =
            openshell_core::driver_utils::sandbox_log_level(sandbox, &self.config.log_level);
        args.extend([
            "-e".to_string(),
            format!("{}={log_level}", sandbox_env::LOG_LEVEL),
        ]);

        let grpc_endpoint = self.effective_grpc_endpoint();
        args.extend([
            "-e".to_string(),
            format!("{}={grpc_endpoint}", sandbox_env::ENDPOINT),
        ]);
        args.extend([
            "-e".to_string(),
            format!("{}={}", sandbox_env::SANDBOX_ID, sandbox.id),
        ]);
        args.extend([
            "-e".to_string(),
            format!("{}={}", sandbox_env::SANDBOX, sandbox.name),
        ]);
        // The supervisor runs `sleep infinity` as the user-visible process
        // while it manages SSH, policy, and the gateway relay.
        args.extend([
            "-e".to_string(),
            format!("{}=sleep infinity", sandbox_env::SANDBOX_COMMAND),
        ]);
        // SSH socket path for the in-sandbox SSH daemon that the supervisor
        // bridges via the ConnectSupervisor relay back to the gateway.
        args.extend([
            "-e".to_string(),
            format!("{}=/run/openshell/ssh.sock", sandbox_env::SSH_SOCKET_PATH),
        ]);
        args.extend([
            "-e".to_string(),
            format!(
                "{}={}",
                sandbox_env::TELEMETRY_ENABLED,
                openshell_core::telemetry::enabled_env_value()
            ),
        ]);

        // Template / spec environment variables.
        // Validate keys to prevent CLI flag injection via crafted env names.
        if let Some(spec) = sandbox.spec.as_ref() {
            for (key, value) in &spec.environment {
                if sandbox_env::is_valid_env_key(key) {
                    args.extend(["-e".to_string(), format!("{key}={value}")]);
                } else {
                    warn!(key = %key, "Dropping environment variable with invalid key");
                }
            }
            if let Some(template) = spec.template.as_ref() {
                for (key, value) in &template.environment {
                    if sandbox_env::is_valid_env_key(key) {
                        args.extend(["-e".to_string(), format!("{key}={value}")]);
                    } else {
                        warn!(key = %key, "Dropping environment variable with invalid key");
                    }
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

        // Mount TLS certificates and tell the supervisor where to find them.
        if let (Some(ca), Some(cert), Some(key)) = (
            self.config.guest_tls_ca.as_ref(),
            self.config.guest_tls_cert.as_ref(),
            self.config.guest_tls_key.as_ref(),
        ) {
            let ca_mount = "/etc/openshell/tls/client/ca.crt";
            let cert_mount = "/etc/openshell/tls/client/tls.crt";
            let key_mount = "/etc/openshell/tls/client/tls.key";

            args.extend(["-v".to_string(), format!("{}:{ca_mount}", ca.display())]);
            args.extend(["-v".to_string(), format!("{}:{cert_mount}", cert.display())]);
            args.extend(["-v".to_string(), format!("{}:{key_mount}", key.display())]);
            args.extend([
                "-e".to_string(),
                format!("{}={ca_mount}", sandbox_env::TLS_CA),
            ]);
            args.extend([
                "-e".to_string(),
                format!("{}={cert_mount}", sandbox_env::TLS_CERT),
            ]);
            args.extend([
                "-e".to_string(),
                format!("{}={key_mount}", sandbox_env::TLS_KEY),
            ]);
        }

        // Mount sandbox token and tell the supervisor where to find it.
        if let Some(token_path) = token_host_path {
            let token_mount = "/etc/openshell/auth/sandbox.jwt";
            args.extend([
                "-v".to_string(),
                format!("{}:{token_mount}", token_path.display()),
            ]);
            args.extend([
                "-e".to_string(),
                format!("{}={token_mount}", sandbox_env::SANDBOX_TOKEN_FILE),
            ]);
        }

        args
    }

    /// Build the supervisor entrypoint command.
    ///
    /// The supervisor reads all configuration from environment variables
    /// (set in `build_run_args`), so the command is just the binary path.
    ///
    /// Callers must ensure `supervisor_bin` is set before calling this
    /// (enforced by `validate_sandbox_create` which `create_sandbox` calls).
    fn build_supervisor_command(&self) -> Vec<String> {
        debug_assert!(
            self.config.supervisor_bin.is_some(),
            "build_supervisor_command called without supervisor_bin; \
             create_sandbox should call validate_sandbox_create first"
        );

        vec![SUPERVISOR_IMAGE_BINARY_PATH.to_string()]
    }

    fn effective_grpc_endpoint(&self) -> String {
        if !self.config.grpc_endpoint.is_empty() {
            return self.config.grpc_endpoint.clone();
        }
        // Apple containers reach the macOS host at the vmnet gateway IP.
        let scheme = if self.config.tls_enabled() {
            "https"
        } else {
            "http"
        };
        let host = &self.config.host_gateway_ip;
        let port = self.config.gateway_port;
        format!("{scheme}://{host}:{port}")
    }
}

// ── Conversion helpers ──────────────────────────────────────────────────────

/// Check if a container entry is managed by `OpenShell`.
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
    if labels
        .get(LABEL_MANAGED_BY)
        .is_none_or(|v| v != LABEL_MANAGED_BY_VALUE)
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
        .map_or("unknown", |s| s.state.as_str());

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

/// Build a test config with required fields populated.
#[cfg(test)]
fn test_config() -> AppleContainerComputeConfig {
    AppleContainerComputeConfig {
        default_image: "test-image:latest".to_string(),
        grpc_endpoint: "http://192.168.64.1:17670".to_string(),
        supervisor_bin: Some(std::path::PathBuf::from("/opt/openshell-sandbox")),
        ..AppleContainerComputeConfig::default()
    }
}

/// Build a minimal `DriverSandbox` for testing.
#[cfg(test)]
fn test_sandbox(id: &str, name: &str) -> DriverSandbox {
    use openshell_core::proto::compute::v1::{DriverSandboxSpec, DriverSandboxTemplate};
    DriverSandbox {
        id: id.to_string(),
        name: name.to_string(),
        namespace: "default".to_string(),
        spec: Some(DriverSandboxSpec {
            log_level: "info".to_string(),
            environment: Default::default(),
            template: Some(DriverSandboxTemplate {
                image: "ubuntu:latest".to_string(),
                ..Default::default()
            }),
            gpu: false,
            ..Default::default()
        }),
        status: None,
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
                labels: std::iter::once((
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                ))
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

    #[test]
    fn env_key_validation_uses_shared_function() {
        use openshell_core::sandbox_env::is_valid_env_key;

        // Valid POSIX keys
        assert!(is_valid_env_key("OPENSHELL_LOG_LEVEL"));
        assert!(is_valid_env_key("PATH"));
        assert!(is_valid_env_key("_INTERNAL"));
        assert!(is_valid_env_key("A1"));

        // Empty
        assert!(!is_valid_env_key(""));

        // CLI flag injection
        assert!(!is_valid_env_key("--privileged"));
        assert!(!is_valid_env_key("-e"));

        // Digit-leading (invalid per POSIX)
        assert!(!is_valid_env_key("1PASSWORD_TOKEN"));
        assert!(!is_valid_env_key("0BAD"));

        // Disallowed characters
        assert!(
            !is_valid_env_key("KEY=VALUE"),
            "= is not alphanumeric or underscore"
        );
        assert!(!is_valid_env_key("KEY WITH SPACES"));
        assert!(!is_valid_env_key("KEY\nINJECT"));
    }

    // ── build_run_args tests ────────────────────────────────────────────

    #[test]
    fn run_args_include_required_labels() {
        let config = test_config();
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let find = |prefix: &str| {
            args.windows(2)
                .find(|pair| pair[0] == "-l" && pair[1].starts_with(prefix))
                .map(|pair| pair[1].clone())
        };
        assert_eq!(
            find(LABEL_MANAGED_BY),
            Some(format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE}"))
        );
        assert_eq!(
            find(LABEL_SANDBOX_ID),
            Some(format!("{LABEL_SANDBOX_ID}=sb-1"))
        );
        assert_eq!(
            find(LABEL_SANDBOX_NAME),
            Some(format!("{LABEL_SANDBOX_NAME}=demo"))
        );
    }

    #[test]
    fn run_args_include_capabilities() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let caps: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "--cap-add")
            .map(|pair| pair[1].as_str())
            .collect();
        assert!(caps.contains(&"NET_ADMIN"), "missing NET_ADMIN");
        assert!(caps.contains(&"SYS_ADMIN"), "missing SYS_ADMIN");
    }

    #[test]
    fn run_args_include_supervisor_env_vars() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        // Use exact "KEY=" prefix matching to avoid SANDBOX matching SANDBOX_ID.
        let env = |key: &str| {
            let prefix = format!("{key}=");
            args.windows(2)
                .find(|pair| pair[0] == "-e" && pair[1].starts_with(&prefix))
                .map(|pair| pair[1].clone())
        };
        assert_eq!(
            env(sandbox_env::ENDPOINT),
            Some(format!(
                "{}=http://192.168.64.1:17670",
                sandbox_env::ENDPOINT
            ))
        );
        assert_eq!(
            env(sandbox_env::SANDBOX_ID),
            Some(format!("{}=sb-1", sandbox_env::SANDBOX_ID))
        );
        assert_eq!(
            env(sandbox_env::SANDBOX),
            Some(format!("{}=demo", sandbox_env::SANDBOX))
        );
        assert!(
            env(sandbox_env::SANDBOX_COMMAND).is_some(),
            "missing SANDBOX_COMMAND"
        );
        assert!(
            env(sandbox_env::SSH_SOCKET_PATH).is_some(),
            "missing SSH_SOCKET_PATH"
        );
    }

    #[test]
    fn run_args_mount_supervisor_binary() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let vol = args
            .windows(2)
            .find(|pair| pair[0] == "-v" && pair[1].contains(SUPERVISOR_IMAGE_BINARY_PATH))
            .map(|pair| pair[1].clone());
        assert_eq!(
            vol,
            Some(format!(
                "/opt/openshell-sandbox:{SUPERVISOR_IMAGE_BINARY_PATH}"
            ))
        );
    }

    #[test]
    fn run_args_apply_cpu_and_memory_limits() {
        let mut config = test_config();
        config.sandbox_cpus = Some(4);
        config.sandbox_memory = Some("2G".to_string());
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let cpu = args
            .windows(2)
            .find(|pair| pair[0] == "--cpus")
            .map(|pair| pair[1].clone());
        assert_eq!(cpu, Some("4".to_string()));

        let mem = args
            .windows(2)
            .find(|pair| pair[0] == "--memory")
            .map(|pair| pair[1].clone());
        assert_eq!(mem, Some("2G".to_string()));
    }

    #[test]
    fn run_args_omit_limits_when_not_configured() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        assert!(
            !args.contains(&"--cpus".to_string()),
            "--cpus should be absent"
        );
        assert!(
            !args.contains(&"--memory".to_string()),
            "--memory should be absent"
        );
    }

    #[test]
    fn run_args_include_tls_mounts_when_configured() {
        let mut config = test_config();
        config.guest_tls_ca = Some("/tls/ca.crt".into());
        config.guest_tls_cert = Some("/tls/tls.crt".into());
        config.guest_tls_key = Some("/tls/tls.key".into());
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let vols: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "-v" && pair[1].contains("/etc/openshell/tls/"))
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(vols.len(), 3, "should mount ca, cert, and key");

        let env = |key: &str| {
            args.windows(2)
                .any(|pair| pair[0] == "-e" && pair[1].starts_with(key))
        };
        assert!(env(sandbox_env::TLS_CA), "missing TLS_CA env");
        assert!(env(sandbox_env::TLS_CERT), "missing TLS_CERT env");
        assert!(env(sandbox_env::TLS_KEY), "missing TLS_KEY env");
    }

    #[test]
    fn run_args_omit_tls_when_not_configured() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let tls_vols = args
            .windows(2)
            .filter(|pair| pair[0] == "-v" && pair[1].contains("/etc/openshell/tls/"))
            .count();
        assert_eq!(tls_vols, 0, "no TLS mounts without config");
    }

    #[test]
    fn run_args_include_token_mount() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let token_path = std::path::Path::new("/tmp/sandbox.jwt");
        let args = driver.build_run_args(&sandbox, Some(token_path));

        let vol = args
            .windows(2)
            .find(|pair| pair[0] == "-v" && pair[1].contains("sandbox.jwt"));
        assert!(vol.is_some(), "should mount token file");

        let env = args
            .windows(2)
            .any(|pair| pair[0] == "-e" && pair[1].starts_with(sandbox_env::SANDBOX_TOKEN_FILE));
        assert!(env, "should set SANDBOX_TOKEN_FILE env");
    }

    #[test]
    fn run_args_omit_token_when_not_provided() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        let args = driver.build_run_args(&sandbox, None);

        let has_token = args
            .windows(2)
            .any(|pair| pair[0] == "-e" && pair[1].starts_with(sandbox_env::SANDBOX_TOKEN_FILE));
        assert!(!has_token, "should not set token env without path");
    }

    #[test]
    fn run_args_filter_invalid_env_keys() {
        use openshell_core::proto::compute::v1::{DriverSandboxSpec, DriverSandboxTemplate};

        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = DriverSandbox {
            id: "sb-1".to_string(),
            name: "demo".to_string(),
            namespace: "default".to_string(),
            spec: Some(DriverSandboxSpec {
                environment: [
                    ("VALID_KEY".to_string(), "good".to_string()),
                    ("--privileged".to_string(), "injected".to_string()),
                ]
                .into_iter()
                .collect(),
                template: Some(DriverSandboxTemplate {
                    image: "test:latest".to_string(),
                    environment: [("1BAD".to_string(), "nope".to_string())]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            status: None,
        };
        let args = driver.build_run_args(&sandbox, None);

        let env_vals: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "-e")
            .map(|pair| pair[1].as_str())
            .collect();
        assert!(
            env_vals.iter().any(|v| v.starts_with("VALID_KEY=")),
            "valid key should pass"
        );
        assert!(
            !env_vals.iter().any(|v| v.contains("privileged")),
            "--privileged should be filtered"
        );
        assert!(
            !env_vals.iter().any(|v| v.starts_with("1BAD=")),
            "digit-leading key should be filtered"
        );
    }

    // ── validate_sandbox_create tests ───────────────────────────────────

    #[test]
    fn validate_rejects_gpu_sandbox() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let mut sandbox = test_sandbox("sb-1", "demo");
        sandbox.spec.as_mut().unwrap().gpu = true;

        let err = driver.validate_sandbox_create(&sandbox).unwrap_err();
        assert!(
            matches!(err, ComputeDriverError::Precondition(_)),
            "GPU should be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_rejects_missing_supervisor_bin() {
        let mut config = test_config();
        config.supervisor_bin = None;
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        let sandbox = test_sandbox("sb-1", "demo");

        let err = driver.validate_sandbox_create(&sandbox).unwrap_err();
        assert!(
            matches!(err, ComputeDriverError::Precondition(ref msg) if msg.contains("supervisor_bin")),
            "missing supervisor_bin should be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_accepts_valid_sandbox() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let sandbox = test_sandbox("sb-1", "demo");
        driver.validate_sandbox_create(&sandbox).unwrap();
    }

    // ── build_supervisor_command tests ──────────────────────────────────

    #[test]
    fn supervisor_command_is_binary_path_only() {
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config: test_config(),
        };
        let cmd = driver.build_supervisor_command();
        assert_eq!(cmd, vec![SUPERVISOR_IMAGE_BINARY_PATH]);
    }

    // ── effective_grpc_endpoint tests ───────────────────────────────────

    #[test]
    fn explicit_grpc_endpoint_takes_precedence() {
        let mut config = test_config();
        config.grpc_endpoint = "https://custom:9090".to_string();
        config.host_gateway_ip = "10.0.0.1".to_string();
        config.gateway_port = 8080;
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        assert_eq!(driver.effective_grpc_endpoint(), "https://custom:9090");
    }

    #[test]
    fn auto_detected_endpoint_uses_host_gateway_ip() {
        let mut config = test_config();
        config.grpc_endpoint = String::new();
        config.host_gateway_ip = "10.0.0.1".to_string();
        config.gateway_port = 8080;
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        assert_eq!(driver.effective_grpc_endpoint(), "http://10.0.0.1:8080");
    }

    #[test]
    fn auto_detected_endpoint_uses_https_with_tls() {
        let mut config = test_config();
        config.grpc_endpoint = String::new();
        config.host_gateway_ip = "192.168.64.1".to_string();
        config.gateway_port = 17670;
        config.guest_tls_ca = Some("/tls/ca.crt".into());
        config.guest_tls_cert = Some("/tls/tls.crt".into());
        config.guest_tls_key = Some("/tls/tls.key".into());
        let driver = AppleContainerComputeDriver {
            cli: ContainerCli::new("false"),
            config,
        };
        assert_eq!(
            driver.effective_grpc_endpoint(),
            "https://192.168.64.1:17670"
        );
    }

    // ── driver_sandbox_from_entry edge cases ────────────────────────────

    #[test]
    fn driver_sandbox_from_entry_returns_none_for_missing_labels() {
        let entry = ContainerEntry {
            id: "test".to_string(),
            configuration: ContainerConfiguration {
                labels: std::iter::once((
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                ))
                .collect(),
                ..Default::default()
            },
            status: Some(ContainerStatus {
                state: "running".to_string(),
                ..Default::default()
            }),
        };
        // Missing SANDBOX_ID and SANDBOX_NAME → should return None
        assert!(driver_sandbox_from_entry(&entry).is_none());
    }

    #[test]
    fn driver_sandbox_defaults_namespace_when_label_absent() {
        let entry = ContainerEntry {
            id: "test".to_string(),
            configuration: ContainerConfiguration {
                labels: [
                    (
                        LABEL_MANAGED_BY.to_string(),
                        LABEL_MANAGED_BY_VALUE.to_string(),
                    ),
                    (LABEL_SANDBOX_ID.to_string(), "sb-1".to_string()),
                    (LABEL_SANDBOX_NAME.to_string(), "demo".to_string()),
                    // No LABEL_SANDBOX_NAMESPACE
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            status: None,
        };
        let sandbox = driver_sandbox_from_entry(&entry).unwrap();
        assert_eq!(sandbox.namespace, "default");
    }

    // ── error mapping ──────────────────────────────────────────────────

    #[test]
    fn cli_not_found_maps_to_driver_message() {
        let err: ComputeDriverError = ContainerCliError::NotFound("gone".into()).into();
        assert!(matches!(err, ComputeDriverError::Message(_)));
    }

    #[test]
    fn cli_already_exists_maps_to_already_exists() {
        let err: ComputeDriverError = ContainerCliError::AlreadyExists("dup".into()).into();
        assert!(matches!(err, ComputeDriverError::AlreadyExists));
    }
}
