// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Sandbox watch loop for the Apple Container driver.
//!
//! Apple's `container` CLI does not provide a streaming events API, so this
//! module polls `container list --all --format json` at a regular interval and
//! emits snapshot diffs as `WatchSandboxesEvent` messages.

use crate::cli::{ContainerCli, ContainerCliError};
use crate::driver::driver_sandbox_from_entry;
use openshell_core::driver_utils::{LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE};
use openshell_core::proto::compute::v1::{
    DriverSandbox, WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesSandboxEvent,
    watch_sandboxes_event,
};
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

/// Stream type returned by the watcher.
pub type WatchStream =
    Pin<Box<dyn futures::Stream<Item = Result<WatchSandboxesEvent, tonic::Status>> + Send>>;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const POLL_MAX_BACKOFF: Duration = Duration::from_secs(30);
const WATCH_BUFFER: usize = 128;

/// Start the watch loop, returning a stream of sandbox events.
pub async fn start_watch(cli: ContainerCli) -> Result<WatchStream, ContainerCliError> {
    let (tx, rx) = mpsc::channel(WATCH_BUFFER);

    // Seed with current state.
    let initial = match poll_managed_sandboxes(&cli).await {
        Ok(sandboxes) => sandboxes,
        Err(err) => {
            warn!(error = %err, "Failed to seed initial sandbox state");
            HashMap::new()
        }
    };
    for sandbox in initial.values() {
        let _ = tx
            .send(Ok(WatchSandboxesEvent {
                payload: Some(watch_sandboxes_event::Payload::Sandbox(
                    WatchSandboxesSandboxEvent {
                        sandbox: Some(sandbox.clone()),
                    },
                )),
            }))
            .await;
    }

    tokio::spawn(async move {
        let mut previous = initial;
        let mut backoff = POLL_INTERVAL;

        loop {
            tokio::time::sleep(backoff).await;

            match poll_managed_sandboxes(&cli).await {
                Ok(current) => {
                    // Emit updates for new or changed sandboxes.
                    for (id, sandbox) in &current {
                        let changed = previous
                            .get(id)
                            .is_none_or(|prev| !sandbox_status_eq(prev, sandbox));
                        if changed
                            && tx
                                .send(Ok(WatchSandboxesEvent {
                                    payload: Some(watch_sandboxes_event::Payload::Sandbox(
                                        WatchSandboxesSandboxEvent {
                                            sandbox: Some(sandbox.clone()),
                                        },
                                    )),
                                }))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }

                    // Emit deletions for sandboxes that disappeared.
                    for id in previous.keys() {
                        if !current.contains_key(id)
                            && tx
                                .send(Ok(WatchSandboxesEvent {
                                    payload: Some(watch_sandboxes_event::Payload::Deleted(
                                        WatchSandboxesDeletedEvent {
                                            sandbox_id: id.clone(),
                                        },
                                    )),
                                }))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }

                    previous = current;
                    backoff = POLL_INTERVAL;
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        backoff_secs = backoff.as_secs(),
                        "Failed to poll Apple Container sandboxes"
                    );
                    backoff = (backoff * 2).min(POLL_MAX_BACKOFF);
                }
            }
        }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
}

/// Poll the CLI for all managed sandboxes and return them keyed by sandbox ID.
async fn poll_managed_sandboxes(
    cli: &ContainerCli,
) -> Result<HashMap<String, DriverSandbox>, ContainerCliError> {
    let entries = cli.list_all().await?;
    let mut result = HashMap::new();

    for entry in &entries {
        let managed = entry
            .configuration
            .labels
            .get(LABEL_MANAGED_BY)
            .is_some_and(|v| v == LABEL_MANAGED_BY_VALUE);
        if !managed {
            continue;
        }

        if let Some(sandbox) = driver_sandbox_from_entry(entry) {
            result.insert(sandbox.id.clone(), sandbox);
        }
    }

    Ok(result)
}

/// Rough equality check on sandbox status for change detection.
fn sandbox_status_eq(a: &DriverSandbox, b: &DriverSandbox) -> bool {
    let a_status = a.status.as_ref();
    let b_status = b.status.as_ref();
    match (a_status, b_status) {
        (Some(a_s), Some(b_s)) => {
            a_s.instance_id == b_s.instance_id
                && a_s.deleting == b_s.deleting
                && a_s.conditions.len() == b_s.conditions.len()
                && a_s
                    .conditions
                    .iter()
                    .zip(b_s.conditions.iter())
                    .all(|(ac, bc)| {
                        ac.r#type == bc.r#type && ac.status == bc.status && ac.reason == bc.reason
                    })
        }
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::proto::compute::v1::{DriverCondition, DriverSandboxStatus};

    fn make_sandbox(id: &str, reason: &str) -> DriverSandbox {
        DriverSandbox {
            id: id.to_string(),
            name: format!("name-{id}"),
            namespace: "default".to_string(),
            spec: None,
            status: Some(DriverSandboxStatus {
                sandbox_name: format!("name-{id}"),
                instance_id: "inst-1".to_string(),
                agent_fd: String::new(),
                sandbox_fd: String::new(),
                conditions: vec![DriverCondition {
                    r#type: "Ready".to_string(),
                    status: "False".to_string(),
                    reason: reason.to_string(),
                    message: String::new(),
                    last_transition_time: String::new(),
                }],
                deleting: false,
            }),
        }
    }

    #[test]
    fn sandbox_status_eq_same() {
        let a = make_sandbox("sb-1", "Starting");
        let b = make_sandbox("sb-1", "Starting");
        assert!(sandbox_status_eq(&a, &b));
    }

    #[test]
    fn sandbox_status_eq_different_reason() {
        let a = make_sandbox("sb-1", "Starting");
        let b = make_sandbox("sb-1", "DependenciesNotReady");
        assert!(!sandbox_status_eq(&a, &b));
    }
}
