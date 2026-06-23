// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration for the Apple Container compute driver.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Gateway-local configuration for the Apple Container compute driver.
///
/// Corresponds to `[openshell.drivers.apple-container]` in the gateway TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppleContainerComputeConfig {
    /// Default OCI image for sandboxes.
    pub default_image: String,

    /// Image pull policy for sandbox images.
    ///
    /// Supported values: `""` (default, pull if not present), `"always"`, `"never"`.
    pub image_pull_policy: String,

    /// Namespace label applied to Apple Container sandboxes.
    pub sandbox_namespace: String,

    /// Gateway gRPC endpoint the sandbox connects back to.
    ///
    /// When empty, the driver auto-detects using the container's gateway IP
    /// and the configured port.
    pub grpc_endpoint: String,

    /// Gateway listen port used to construct the auto-detected gRPC endpoint.
    pub gateway_port: u16,

    /// Optional override for the `openshell-sandbox` supervisor binary
    /// mounted into containers.
    pub supervisor_bin: Option<PathBuf>,

    /// Supervisor image containing the Linux `openshell-sandbox` binary.
    /// Mounted into sandbox containers using `--volume` or `--mount`.
    pub supervisor_image: String,

    /// Host-side CA certificate for sandbox mTLS.
    pub guest_tls_ca: Option<PathBuf>,

    /// Host-side client certificate for sandbox mTLS.
    pub guest_tls_cert: Option<PathBuf>,

    /// Host-side private key for sandbox mTLS.
    pub guest_tls_key: Option<PathBuf>,

    /// Number of CPUs to allocate to each sandbox container.
    pub sandbox_cpus: Option<u32>,

    /// Memory limit per sandbox container (e.g. "2G", "512M").
    pub sandbox_memory: Option<String>,

    /// Stop timeout in seconds before force-killing a container.
    pub stop_timeout_secs: u32,

    /// Path to the `container` CLI binary.
    ///
    /// Defaults to searching PATH for `container`.
    pub container_bin: String,

    /// Log level for sandbox supervisor processes.
    pub log_level: String,
}

impl Default for AppleContainerComputeConfig {
    fn default() -> Self {
        Self {
            default_image: openshell_core::image::default_sandbox_image(),
            image_pull_policy: String::new(),
            sandbox_namespace: "default".to_string(),
            grpc_endpoint: String::new(),
            gateway_port: 0,
            supervisor_bin: None,
            supervisor_image: default_supervisor_image(),
            guest_tls_ca: None,
            guest_tls_cert: None,
            guest_tls_key: None,
            sandbox_cpus: None,
            sandbox_memory: None,
            stop_timeout_secs: 10,
            container_bin: "container".to_string(),
            log_level: String::new(),
        }
    }
}

impl AppleContainerComputeConfig {
    /// Whether TLS client certs are fully configured for guest mTLS.
    #[must_use]
    pub fn tls_enabled(&self) -> bool {
        self.guest_tls_ca.is_some()
            && self.guest_tls_cert.is_some()
            && self.guest_tls_key.is_some()
    }
}

fn default_supervisor_image() -> String {
    format!(
        "ghcr.io/nvidia/openshell/supervisor:{}",
        openshell_core::VERSION
    )
}
