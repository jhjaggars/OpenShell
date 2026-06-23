// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Apple Container compute driver for OpenShell.
//!
//! This driver manages sandbox containers using Apple's `container` CLI tool,
//! which runs Linux containers as lightweight VMs on macOS with Apple Silicon.
//! Each container gets its own VM with full isolation via the macOS
//! Virtualization framework.
//!
//! The driver shells out to the `container` CLI for lifecycle operations
//! (create, start, stop, rm) and parses JSON output from `container ls`
//! and `container inspect` to observe sandbox state.

pub mod cli;
pub mod config;
pub mod driver;
pub mod grpc;
pub mod watcher;

pub use config::AppleContainerComputeConfig;
pub use driver::AppleContainerComputeDriver;
pub use grpc::ComputeDriverService;
