// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! I/O shell of the `NetworkPolicy` guard: resolve the expected object from
//! trusted reads, repair the stored one with preconditioned writes, then run
//! the shared per-sandbox reconcile for every affected sandbox so the
//! fail-closed suspension decision stays in one place.

use super::netpol_guard::{
    BoundState, Drift, ExpectedPolicy, GuardedKind, NoAction, PolicyKey, RenderInputs,
    RepairAction, SharedTarget, classify, plan_repair, resolve_kind,
};
use super::{
    ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_GENERATION, KUBE_API_TIMEOUT,
    KubernetesComputeDriver, ReconcileOutcome, SkipReason, sandbox_id_from_object,
    sandbox_runtime_bootstrap_in_progress, sandbox_runtime_sandbox_owner_reference,
    sandbox_runtime_should_run,
};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Error as KubeError;
use kube::api::{Api, ListParams, PostParams};
use kube::core::DynamicObject;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// At most this many repairs of one policy within [`REPAIR_BUDGET_WINDOW`].
/// A webhook, policy engine or `GitOps` controller that keeps rewriting a
/// policy would otherwise fight the guard forever; past the budget the
/// guard stops writing that policy and the periodic pass keeps failing
/// closed.
pub const REPAIR_BUDGET: usize = 5;
pub const REPAIR_BUDGET_WINDOW: Duration = Duration::from_mins(10);
/// Retry delay for a busy lifecycle gate or a raced write.
pub const REQUEUE_DELAY: Duration = Duration::from_secs(1);

/// Per-replica guard state shared by every clone of the driver.
#[derive(Default)]
pub struct GuardState {
    /// Serializes repairs within one namespace, so two keys can't race the
    /// namespace fences inside this replica.
    namespace_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Recent repair times per key, for the repair budget.
    repairs: Mutex<HashMap<PolicyKey, VecDeque<Instant>>>,
}

impl GuardState {
    fn namespace_lock(&self, namespace: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.namespace_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(namespace.to_string())
            .or_default()
            .clone()
    }

    /// Whether `key` may be repaired now. Logs once when the budget runs out.
    fn budget_allows(&self, key: &PolicyKey, now: Instant) -> bool {
        let mut repairs = self
            .repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recent = repairs.entry(key.clone()).or_default();
        while recent
            .front()
            .is_some_and(|at| now.duration_since(*at) > REPAIR_BUDGET_WINDOW)
        {
            recent.pop_front();
        }
        if recent.len() < REPAIR_BUDGET {
            return true;
        }
        if recent.len() == REPAIR_BUDGET {
            // Record a sentinel so the warning is logged once per window.
            recent.push_back(now);
            warn!(
                namespace = %key.namespace,
                policy = %key.name,
                budget = REPAIR_BUDGET,
                window_secs = REPAIR_BUDGET_WINDOW.as_secs(),
                "NetworkPolicy repair budget exhausted; another controller keeps changing this policy. The guard stops repairing it and periodic reconciliation keeps affected sandboxes suspended"
            );
        }
        false
    }

    fn record_repair(&self, key: &PolicyKey, now: Instant) {
        self.repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_default()
            .push_back(now);
    }
}

/// Result of a repair write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairWrite {
    None(NoAction),
    Created,
    Replaced,
    /// A precondition failed (409): someone else wrote first. Re-read.
    Raced,
    /// The object disappeared before the replace (404).
    Vanished,
    Failed(String),
    /// The repair budget for this key is exhausted.
    BudgetExhausted,
}

impl RepairWrite {
    pub const fn written(&self) -> bool {
        matches!(self, Self::Created | Self::Replaced)
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None(_) => "none",
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::Raced => "raced",
            Self::Vanished => "vanished",
            Self::Failed(_) => "failed",
            Self::BudgetExhausted => "budget-exhausted",
        }
    }
}

/// What the guard did for one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Nothing to do, with why.
    Skipped(&'static str),
    Checked {
        kind: GuardedKind,
        drift: Drift,
        write: RepairWrite,
        /// Per-sandbox reconcile outcomes, keyed by sandbox ID.
        reconciled: Vec<(String, ReconcileOutcome)>,
    },
}

impl GuardOutcome {
    /// Whether the key should be looked at again shortly.
    pub fn needs_requeue(&self) -> bool {
        match self {
            Self::Skipped(reason) => *reason == "lifecycle-busy",
            Self::Checked {
                write, reconciled, ..
            } => {
                matches!(write, RepairWrite::Raced | RepairWrite::Vanished)
                    || reconciled.iter().any(|(_, outcome)| {
                        *outcome == ReconcileOutcome::Skipped(SkipReason::GateBusy)
                    })
            }
        }
    }
}

/// Perform a planned repair. Every write carries a precondition: a create
/// fails on an existing object and a replace on a changed resourceVersion.
pub async fn execute_repair(api: &Api<NetworkPolicy>, action: RepairAction) -> RepairWrite {
    match action {
        RepairAction::None(reason) => RepairWrite::None(reason),
        RepairAction::Create(body) => {
            match tokio::time::timeout(KUBE_API_TIMEOUT, api.create(&PostParams::default(), &body))
                .await
            {
                Ok(Ok(_)) => RepairWrite::Created,
                Ok(Err(KubeError::Api(error))) if error.code == 409 => RepairWrite::Raced,
                Ok(Err(error)) => RepairWrite::Failed(error.to_string()),
                Err(_) => RepairWrite::Failed("timed out".to_string()),
            }
        }
        RepairAction::Replace {
            mut body,
            resource_version,
        } => {
            let name = body.metadata.name.clone().unwrap_or_default();
            body.metadata.resource_version = Some(resource_version);
            match tokio::time::timeout(
                KUBE_API_TIMEOUT,
                api.replace(&name, &PostParams::default(), &body),
            )
            .await
            {
                Ok(Ok(_)) => RepairWrite::Replaced,
                Ok(Err(KubeError::Api(error))) if error.code == 409 => RepairWrite::Raced,
                Ok(Err(KubeError::Api(error))) if error.code == 404 => RepairWrite::Vanished,
                Ok(Err(error)) => RepairWrite::Failed(error.to_string()),
                Err(_) => RepairWrite::Failed("timed out".to_string()),
            }
        }
    }
}

/// A Sandbox CR from the driver's own list, with the facts the guard needs.
struct TrustedSandbox {
    sandbox_id: String,
    object: DynamicObject,
}

impl TrustedSandbox {
    fn bound_state(&self) -> BoundState {
        BoundState {
            may_run: sandbox_runtime_should_run(&self.object)
                || sandbox_runtime_bootstrap_in_progress(&self.object),
            bound_generation: self
                .object
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| {
                    annotations.get(ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_GENERATION)
                })
                .and_then(|generation| generation.parse().ok()),
        }
    }
}

impl KubernetesComputeDriver {
    /// Check one guarded policy and repair it when it drifted. Never creates
    /// a per-sandbox policy, never deletes anything, and never acts in a
    /// namespace that is terminating, gone, outside this driver's scope or
    /// without `OpenShell` sandboxes.
    pub(crate) async fn guard_policy_key(&self, key: &PolicyKey) -> GuardOutcome {
        if !self.accepts_auth_namespace(&key.namespace) {
            return GuardOutcome::Skipped("namespace-not-accepted");
        }
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        match tokio::time::timeout(KUBE_API_TIMEOUT, namespaces.get_opt(&key.namespace)).await {
            Ok(Ok(Some(namespace))) if namespace.metadata.deletion_timestamp.is_none() => {}
            Ok(Ok(_)) => return GuardOutcome::Skipped("namespace-gone"),
            Ok(Err(error)) => {
                debug!(namespace = %key.namespace, %error, "NetworkPolicy guard could not read namespace");
                return GuardOutcome::Skipped("namespace-unreadable");
            }
            Err(_) => return GuardOutcome::Skipped("namespace-unreadable"),
        }

        let Ok(lookup_api) = self
            .supported_sandbox_api_for_lookup(self.client.clone())
            .await
        else {
            return GuardOutcome::Skipped("sandbox-api-unavailable");
        };
        let sandbox_api = Self::agent_sandbox_api(
            self.client.clone(),
            &lookup_api.resource.version,
            &key.namespace,
        );
        let listed = match tokio::time::timeout(
            KUBE_API_TIMEOUT,
            sandbox_api
                .api
                .list(&ListParams::default().labels(&self.openshell_sandbox_selector())),
        )
        .await
        {
            Ok(Ok(list)) => list.items,
            _ => return GuardOutcome::Skipped("sandbox-list-failed"),
        };
        let sandboxes: Vec<TrustedSandbox> = listed
            .into_iter()
            .filter_map(|object| {
                Some(TrustedSandbox {
                    sandbox_id: sandbox_id_from_object(&object).ok()?,
                    object,
                })
            })
            .collect();
        if sandboxes.is_empty() {
            return GuardOutcome::Skipped("no-sandboxes");
        }

        let Some(kind) = resolve_kind(
            &key.name,
            sandboxes.iter().map(|sandbox| sandbox.sandbox_id.as_str()),
            self.guards_managed_ssh(),
        ) else {
            return GuardOutcome::Skipped("not-guarded");
        };
        let owner_sandbox = kind
            .sandbox_id()
            .and_then(|id| sandboxes.iter().find(|sandbox| sandbox.sandbox_id == id));
        if owner_sandbox.is_some_and(|sandbox| sandbox.object.metadata.deletion_timestamp.is_some())
        {
            return GuardOutcome::Skipped("sandbox-deleting");
        }
        let owner = owner_sandbox.and_then(|sandbox| {
            Some(sandbox_runtime_sandbox_owner_reference(
                sandbox.object.metadata.name.as_deref()?,
                sandbox.object.metadata.uid.as_deref()?,
                &sandbox_api.resource.api_version,
                false,
            ))
        });
        let shared_target = if matches!(kind, GuardedKind::SharedControl { .. }) {
            match self.shared_supervisor_target().await {
                Ok((_, target)) => Some(SharedTarget {
                    namespace: target.namespace,
                    selector: target.selector,
                    port: target.address.port(),
                }),
                Err(error) => {
                    debug!(%error, "NetworkPolicy guard could not resolve the shared supervisor");
                    return GuardOutcome::Skipped("shared-target-unavailable");
                }
            }
        } else {
            None
        };
        let Some(expected) = ExpectedPolicy::build(
            key.clone(),
            kind.clone(),
            &RenderInputs {
                namespace: &key.namespace,
                config: &self.config,
                owner,
                shared_target: shared_target.as_ref(),
            },
        ) else {
            return GuardOutcome::Skipped("render-unavailable");
        };

        let Some((drift, write)) = ({
            // A per-sandbox repair must not interleave with that sandbox's
            // start, which creates and binds the same object.
            let _lifecycle = match owner_sandbox {
                Some(sandbox) => match self
                    .lifecycle_gates
                    .gate_for(&sandbox.sandbox_id)
                    .try_lock_owned()
                {
                    Ok(guard) => Some(guard),
                    Err(_) => return GuardOutcome::Skipped("lifecycle-busy"),
                },
                None => None,
            };
            let lock = self.netpol_guard.namespace_lock(&key.namespace);
            let _namespace = lock.lock().await;
            self.repair_policy(&expected, owner_sandbox.map(TrustedSandbox::bound_state))
                .await
        }) else {
            return GuardOutcome::Skipped("policy-unreadable");
        };

        // A raced or failed write is re-read on requeue before deciding.
        if drift == Drift::InSync
            || matches!(
                write,
                RepairWrite::Raced | RepairWrite::Vanished | RepairWrite::Failed(_)
            )
        {
            return GuardOutcome::Checked {
                kind,
                drift,
                write,
                reconciled: Vec::new(),
            };
        }
        // The repaired object has a new UID or generation, so any generation
        // that ran across the drift no longer matches what it bound. The
        // shared reconcile decides suspension exactly as the periodic pass.
        let affected: Vec<&TrustedSandbox> = match &kind {
            GuardedKind::ManagedSsh => Vec::new(),
            GuardedKind::WorkloadFence | GuardedKind::SupervisorEgress => {
                sandboxes.iter().collect()
            }
            GuardedKind::Control { .. } | GuardedKind::SharedControl { .. } => {
                owner_sandbox.into_iter().collect()
            }
        };
        let mut reconciled = Vec::with_capacity(affected.len());
        for sandbox in affected {
            let outcome =
                Box::pin(self.reconcile_one_sandbox_runtime(&lookup_api, &sandbox.object)).await;
            reconciled.push((sandbox.sandbox_id.clone(), outcome));
        }
        GuardOutcome::Checked {
            kind,
            drift,
            write,
            reconciled,
        }
    }

    /// GET by name (labels are ignored, so a label-stripped object is still
    /// found), classify, and write the planned repair.
    async fn repair_policy(
        &self,
        expected: &ExpectedPolicy,
        bound: Option<BoundState>,
    ) -> Option<(Drift, RepairWrite)> {
        let api: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), &expected.key.namespace);
        let observed = match tokio::time::timeout(KUBE_API_TIMEOUT, api.get_opt(&expected.key.name))
            .await
        {
            Ok(Ok(observed)) => observed,
            Ok(Err(error)) => {
                debug!(policy = %expected.key.name, %error, "NetworkPolicy guard could not read policy");
                return None;
            }
            Err(_) => return None,
        };
        let drift = classify(observed.as_ref(), expected);
        let action = plan_repair(
            drift,
            observed.as_ref(),
            expected,
            bound.unwrap_or_default(),
            openshell_core::VERSION,
        );
        if let RepairAction::None(reason) = action {
            return Some((drift, RepairWrite::None(reason)));
        }
        let now = Instant::now();
        if !self.netpol_guard.budget_allows(&expected.key, now) {
            return Some((drift, RepairWrite::BudgetExhausted));
        }
        let write = execute_repair(&api, action).await;
        if write.written() {
            self.netpol_guard.record_repair(&expected.key, now);
        }
        Some((drift, write))
    }

    /// The SSH policy is guarded once its own commit lands.
    #[allow(clippy::unused_self)]
    const fn guards_managed_ssh(&self) -> bool {
        false
    }
}
