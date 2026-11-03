// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Watch side of the `NetworkPolicy` guard.
//!
//! One label-selected watch (`openshell.ai/managed-by=openshell`) runs per
//! sandbox namespace. Its events only name keys: the worker always GETs the
//! policy by name, which ignores labels, and never trusts the event body.
//!
//! A single selector is enough for a label strip followed by a widening edit.
//! The strip moves the object out of the selection, so the API server sends
//! `DELETED`. The queued GET-by-name then sees whatever the object is by the
//! time it runs. If it sees only the strip, the label repair is guarded by
//! the observed resourceVersion: a widening that lands first makes it fail
//! with 409 and the key is re-read, and one that lands after it happens on an
//! object that is back in the selection, so it produces an event. A relist
//! after a watch gap (`InitDone`) re-queues every expected name, which
//! catches a change whose event was missed.

use super::netpol_guard::{PolicyKey, guarded_name};
use super::netpol_guard_shell::{GuardOutcome, REQUEUE_DELAY, guard_signals};
use super::{KubernetesComputeDriver, WorkspaceMode};
use super::{KubernetesDriverError, WatchSandboxesEvent};
use futures::StreamExt;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::Api;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use openshell_core::driver_utils::{LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Collapse bursts of events for one key into a single check.
pub const DEBOUNCE: Duration = Duration::from_millis(200);
/// Re-queue every expected key this often, in case a watch wedges while its
/// stream stays open.
pub const RESYNC_INTERVAL: Duration = Duration::from_mins(10);
/// How often operator mode re-reads its namespace allowlist.
const NAMESPACE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Work for the guard worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardMessage {
    /// Check one policy.
    Key(PolicyKey),
    /// Check every policy the driver expects in a namespace.
    Namespace(String),
}

/// Translate one watch event into guard work. Apply and Delete name a key;
/// a Delete is also what a label strip looks like through the selector.
/// `InitDone` (after start or a relist) queues the whole namespace.
pub fn messages_for_event(namespace: &str, event: &Event<NetworkPolicy>) -> Vec<GuardMessage> {
    match event {
        Event::Apply(policy) | Event::Delete(policy) => policy
            .metadata
            .name
            .as_deref()
            .filter(|name| guarded_name(name))
            .map(|name| vec![GuardMessage::Key(PolicyKey::new(namespace, name))])
            .unwrap_or_default(),
        Event::Init | Event::InitApply(_) => Vec::new(),
        Event::InitDone => vec![GuardMessage::Namespace(namespace.to_string())],
    }
}

/// A keyed queue that runs each key at most once per due time.
#[derive(Debug, Default)]
pub struct GuardQueue {
    due: HashMap<PolicyKey, Instant>,
}

impl GuardQueue {
    /// Queue `key` at `at`, keeping an earlier pending time.
    pub fn push(&mut self, key: PolicyKey, at: Instant) {
        self.due
            .entry(key)
            .and_modify(|due| *due = (*due).min(at))
            .or_insert(at);
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.due.values().min().copied()
    }

    /// Remove and return every key due by `now`, in key order.
    pub fn pop_due(&mut self, now: Instant) -> Vec<PolicyKey> {
        let mut due: Vec<PolicyKey> = self
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        due.sort();
        for key in &due {
            self.due.remove(key);
        }
        due
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.due.len()
    }
}

fn is_forbidden(error: &watcher::Error) -> bool {
    matches!(
        error,
        watcher::Error::InitialListFailed(kube::Error::Api(response))
            | watcher::Error::WatchStartFailed(kube::Error::Api(response))
            | watcher::Error::WatchFailed(kube::Error::Api(response))
            if response.code == 403
    ) || matches!(error, watcher::Error::WatchError(response) if response.code == 403)
}

/// Run one namespace's label-selected watch until `stop` fires.
fn spawn_namespace_watch(
    driver: &KubernetesComputeDriver,
    namespace: String,
    work: mpsc::UnboundedSender<GuardMessage>,
) -> tokio::task::AbortHandle {
    let api: Api<NetworkPolicy> = Api::namespaced(driver.watch_client.clone(), &namespace);
    let config =
        watcher::Config::default().labels(&format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE}"));
    tokio::spawn(async move {
        let mut stream = watcher::watcher(api, config).default_backoff().boxed();
        let mut forbidden_logged = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    forbidden_logged = false;
                    for message in messages_for_event(&namespace, &event) {
                        if work.send(message).is_err() {
                            return;
                        }
                    }
                }
                Err(error) if is_forbidden(&error) => {
                    // Without list/watch the guard can't run here; the
                    // periodic pass keeps its checks. Retry quietly: in
                    // managed mode a fresh RoleBinding may still propagate.
                    if !forbidden_logged {
                        warn!(
                            namespace,
                            "NetworkPolicy drift watch forbidden; relying on periodic reconciliation"
                        );
                        forbidden_logged = true;
                    }
                }
                Err(error) => {
                    warn!(
                        namespace,
                        %error,
                        "NetworkPolicy drift watch error; waiting for kube-runtime recovery"
                    );
                }
            }
        }
    })
    .abort_handle()
}

/// Running per-namespace watches.
#[derive(Default)]
pub struct NamespaceWatches {
    running: BTreeMap<String, tokio::task::AbortHandle>,
}

impl NamespaceWatches {
    /// Start watches for namespaces in `desired` and stop the rest.
    pub fn reconcile(
        &mut self,
        driver: &KubernetesComputeDriver,
        desired: &std::collections::BTreeSet<String>,
        work: &mpsc::UnboundedSender<GuardMessage>,
    ) {
        self.running.retain(|namespace, handle| {
            let keep = desired.contains(namespace);
            if !keep {
                info!(namespace, "stopping NetworkPolicy drift watch");
                handle.abort();
            }
            keep
        });
        for namespace in desired {
            if !self.running.contains_key(namespace) {
                info!(namespace, "starting NetworkPolicy drift watch");
                let handle = spawn_namespace_watch(driver, namespace.clone(), work.clone());
                self.running.insert(namespace.clone(), handle);
            }
        }
    }

    pub fn namespaces(&self) -> impl Iterator<Item = &String> {
        self.running.keys()
    }
}

impl Drop for NamespaceWatches {
    fn drop(&mut self) {
        for handle in self.running.values() {
            handle.abort();
        }
    }
}

impl KubernetesComputeDriver {
    /// Managed mode's namespace set, discovered from the namespaces this
    /// gateway owns. Added with managed-mode RBAC binding.
    #[allow(clippy::unused_self)]
    const fn managed_namespace_events(
        &self,
    ) -> Option<mpsc::Receiver<std::collections::BTreeSet<String>>> {
        None
    }

    /// Namespaces whose guarded policies this replica watches, when the
    /// set is static or polled. Managed mode discovers its own namespaces.
    fn polled_guard_namespaces(&self) -> std::collections::BTreeSet<String> {
        match self.config.workspace_mode {
            WorkspaceMode::Shared => std::iter::once(self.config.namespace.clone()).collect(),
            WorkspaceMode::Operator => self
                .operator_allowlist
                .as_ref()
                .map(|allowlist| allowlist.read().clone())
                .unwrap_or_default(),
            WorkspaceMode::Managed => std::collections::BTreeSet::new(),
        }
    }

    /// Start the `NetworkPolicy` guard for the lifetime of one sandbox watch
    /// stream. It stops when the gateway closes that stream or shuts down;
    /// shutdown is checked first so a terminating replica makes no repairs
    /// during its grace period.
    pub(crate) fn spawn_netpol_guard(
        &self,
        tx: mpsc::Sender<Result<WatchSandboxesEvent, KubernetesDriverError>>,
    ) {
        let driver = self.clone();
        tokio::spawn(async move {
            let (work, mut inbox) = mpsc::unbounded_channel();
            let mut watches = NamespaceWatches::default();
            let mut queue = GuardQueue::default();
            let mut shutdown = driver.shutdown.clone();
            let mut poll = tokio::time::interval(NAMESPACE_POLL_INTERVAL);
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut resync = tokio::time::interval(RESYNC_INTERVAL);
            resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            resync.tick().await;
            let mut managed = driver.managed_namespace_events();
            loop {
                let next = queue.next_due();
                tokio::select! {
                    biased;
                    () = wait_for_shutdown(shutdown.as_mut()) => {
                        debug!("NetworkPolicy guard stopping for gateway shutdown");
                        return;
                    }
                    () = tx.closed() => return,
                    _ = poll.tick(), if driver.config.workspace_mode != WorkspaceMode::Managed => {
                        watches.reconcile(&driver, &driver.polled_guard_namespaces(), &work);
                    }
                    Some(desired) = recv_managed(managed.as_mut()) => {
                        watches.reconcile(&driver, &desired, &work);
                    }
                    _ = resync.tick() => {
                        for namespace in watches.namespaces() {
                            let _ = work.send(GuardMessage::Namespace(namespace.clone()));
                        }
                    }
                    Some(message) = inbox.recv() => match message {
                        GuardMessage::Key(key) => queue.push(key, Instant::now() + DEBOUNCE),
                        GuardMessage::Namespace(namespace) => {
                            let now = Instant::now();
                            for key in driver.expected_guard_keys(&namespace).await {
                                queue.push(key, now);
                            }
                        }
                    },
                    () = sleep_until_due(next) => {
                        for key in queue.pop_due(Instant::now()) {
                            let outcome = driver.guard_policy_key(&key).await;
                            log_guard_outcome(&key, &outcome);
                            for event in guard_signals(&key, &outcome) {
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                            if outcome.needs_requeue() {
                                queue.push(key, Instant::now() + REQUEUE_DELAY);
                            }
                        }
                    }
                }
            }
        });
    }
}

async fn wait_for_shutdown(shutdown: Option<&mut tokio::sync::watch::Receiver<bool>>) {
    let Some(shutdown) = shutdown else {
        return std::future::pending::<()>().await;
    };
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

async fn recv_managed(
    managed: Option<&mut mpsc::Receiver<std::collections::BTreeSet<String>>>,
) -> Option<std::collections::BTreeSet<String>> {
    match managed {
        Some(managed) => managed.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_due(next: Option<Instant>) {
    match next {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn log_guard_outcome(key: &PolicyKey, outcome: &GuardOutcome) {
    match outcome {
        GuardOutcome::Skipped(reason) => {
            debug!(namespace = %key.namespace, policy = %key.name, reason, "NetworkPolicy guard skipped");
        }
        GuardOutcome::Checked { drift, write, .. } => {
            debug!(
                namespace = %key.namespace,
                policy = %key.name,
                drift = drift.as_str(),
                action = write.as_str(),
                "NetworkPolicy guard checked"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::ObjectMeta;

    fn policy(name: &str) -> NetworkPolicy {
        NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn events_name_guarded_keys_only() {
        let key = |name: &str| vec![GuardMessage::Key(PolicyKey::new("ns", name))];
        assert_eq!(
            messages_for_event("ns", &Event::Delete(policy("openshell-sandbox-workloads"))),
            key("openshell-sandbox-workloads")
        );
        assert_eq!(
            messages_for_event("ns", &Event::Apply(policy("os-control-abc"))),
            key("os-control-abc")
        );
        assert!(messages_for_event("ns", &Event::Apply(policy("allow-dns"))).is_empty());
        assert!(
            messages_for_event(
                "ns",
                &Event::InitApply(policy("openshell-sandbox-workloads"))
            )
            .is_empty()
        );
        assert_eq!(
            messages_for_event("ns", &Event::InitDone),
            vec![GuardMessage::Namespace("ns".into())]
        );
    }

    #[test]
    fn queue_collapses_duplicate_keys_and_keeps_the_earliest_time() {
        let mut queue = GuardQueue::default();
        let now = Instant::now();
        let key = PolicyKey::new("ns", "openshell-sandbox-workloads");
        queue.push(key.clone(), now + DEBOUNCE);
        queue.push(key.clone(), now + DEBOUNCE);
        queue.push(key.clone(), now);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.next_due(), Some(now));
        assert_eq!(queue.pop_due(now), vec![key]);
        assert_eq!(queue.len(), 0);
        queue.push(PolicyKey::new("ns", "later"), now + DEBOUNCE);
        assert!(queue.pop_due(now).is_empty());
    }

    #[test]
    fn forbidden_watch_errors_are_recognized() {
        let forbidden = kube::core::ErrorResponse {
            status: "Failure".into(),
            message: "forbidden".into(),
            reason: "Forbidden".into(),
            code: 403,
        };
        assert!(is_forbidden(&watcher::Error::InitialListFailed(
            kube::Error::Api(forbidden.clone())
        )));
        assert!(is_forbidden(&watcher::Error::WatchError(forbidden.clone())));
        let mut gone = forbidden;
        gone.code = 410;
        assert!(!is_forbidden(&watcher::Error::WatchError(gone)));
    }
}
