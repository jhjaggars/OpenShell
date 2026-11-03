// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Managed-mode RBAC for the `NetworkPolicy` guard.
//!
//! The gateway never holds `list` or `watch` on `NetworkPolicy` objects
//! cluster-wide. In managed mode the chart ships one fixed `ClusterRole`
//! with those verbs, and the gateway binds it with a `RoleBinding` in each
//! namespace it owns. Creating that binding needs only `bind` on the one
//! role, which the chart limits by `resourceNames`.

use super::{
    KUBE_API_TIMEOUT, KubernetesComputeDriver, LABEL_GATEWAY_ID, LABEL_MANAGED_BY,
    LABEL_MANAGED_BY_VALUE, is_namespace_owned_by_gateway, managed_namespace_prefix,
};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use kube::Error as KubeError;
use kube::api::{Api, PostParams};
use kube::core::ObjectMeta;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Name of the guard `RoleBinding` in each managed namespace.
pub const GUARD_ROLE_BINDING_NAME: &str = "openshell-netpol-guard";
const CLUSTER_ROLE_ENV: &str = "OPENSHELL_NETPOL_GUARD_CLUSTER_ROLE";
const SERVICE_ACCOUNT_ENV: &str = "OPENSHELL_SERVICE_ACCOUNT_NAME";
const POD_NAMESPACE_ENV: &str = "OPENSHELL_POD_NAMESPACE";

/// The chart-provided role and the gateway's own `ServiceAccount`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardBinding {
    pub cluster_role: String,
    pub service_account: String,
    pub service_account_namespace: String,
}

impl GuardBinding {
    /// Read from the environment the chart renders into the gateway Pod.
    /// `None` when any part is missing, for example with `rbac.create=false`;
    /// the guard then relies on whatever RBAC the operator granted.
    pub fn from_env() -> Option<Self> {
        let read = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
        Some(Self {
            cluster_role: read(CLUSTER_ROLE_ENV)?,
            service_account: read(SERVICE_ACCOUNT_ENV)?,
            service_account_namespace: read(POD_NAMESPACE_ENV)?,
        })
    }

    pub fn role_binding(&self, namespace: &str) -> RoleBinding {
        RoleBinding {
            metadata: ObjectMeta {
                name: Some(GUARD_ROLE_BINDING_NAME.to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(BTreeMap::from([(
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                )])),
                ..Default::default()
            },
            role_ref: RoleRef {
                api_group: "rbac.authorization.k8s.io".to_string(),
                kind: "ClusterRole".to_string(),
                name: self.cluster_role.clone(),
            },
            subjects: Some(vec![Subject {
                kind: "ServiceAccount".to_string(),
                name: self.service_account.clone(),
                namespace: Some(self.service_account_namespace.clone()),
                ..Default::default()
            }]),
        }
    }

    /// Whether an existing binding grants the guard role to this gateway.
    pub fn matches(&self, existing: &RoleBinding) -> bool {
        let expected = self.role_binding("");
        existing.role_ref == expected.role_ref
            && existing.subjects.as_ref().is_some_and(|subjects| {
                subjects
                    .iter()
                    .any(|s| Some(s) == expected.subjects.as_ref().and_then(|e| e.first()))
            })
    }
}

/// Whether `namespace` is a managed namespace this gateway owns: both the
/// ownership labels and the deterministic name prefix must match, so a
/// namespace someone merely labelled can't make the gateway bind its role.
pub fn is_guarded_managed_namespace(namespace: &Namespace, gateway_id: &str) -> bool {
    namespace
        .metadata
        .name
        .as_deref()
        .is_some_and(|name| name.starts_with(&managed_namespace_prefix(gateway_id)))
        && is_namespace_owned_by_gateway(namespace.metadata.labels.as_ref(), gateway_id)
        && namespace.metadata.deletion_timestamp.is_none()
}

impl KubernetesComputeDriver {
    /// Ensure the guard `RoleBinding` in a managed namespace. Failures are
    /// logged, never returned: the periodic pass keeps checking the
    /// namespace's policies without the guard.
    pub(crate) async fn ensure_netpol_guard_role_binding(&self, namespace: &str) -> bool {
        let Some(binding) = &self.netpol_guard_binding else {
            return false;
        };
        let api: Api<RoleBinding> = Api::namespaced(self.client.clone(), namespace);
        let existing = match tokio::time::timeout(
            KUBE_API_TIMEOUT,
            api.get_opt(GUARD_ROLE_BINDING_NAME),
        )
        .await
        {
            Ok(Ok(existing)) => existing,
            Ok(Err(error)) => {
                warn!(namespace, %error, "could not read the NetworkPolicy guard RoleBinding");
                return false;
            }
            Err(_) => return false,
        };
        if let Some(existing) = existing {
            if binding.matches(&existing) {
                return true;
            }
            // roleRef is immutable and repairing would need delete.
            warn!(
                namespace,
                binding = GUARD_ROLE_BINDING_NAME,
                "the NetworkPolicy guard RoleBinding does not grant the guard role to this gateway; NetworkPolicy drift is checked only by periodic reconciliation here"
            );
            return false;
        }
        match tokio::time::timeout(
            KUBE_API_TIMEOUT,
            api.create(&PostParams::default(), &binding.role_binding(namespace)),
        )
        .await
        {
            Ok(Ok(_)) => {
                info!(namespace, "bound the NetworkPolicy guard role");
                true
            }
            Ok(Err(KubeError::Api(error))) if error.code == 409 => true,
            Ok(Err(error)) => {
                warn!(namespace, %error, "could not bind the NetworkPolicy guard role");
                false
            }
            Err(_) => false,
        }
    }

    /// Managed mode's namespace set: every namespace this gateway owns,
    /// discovered with a label-selected Namespace watch. Each new namespace,
    /// including ones created before this gateway started, gets the guard
    /// `RoleBinding` before its watch starts. A namespace that is deleted or
    /// starts terminating leaves the set.
    pub(crate) fn managed_namespace_events(&self) -> Option<mpsc::Receiver<BTreeSet<String>>> {
        if self.config.workspace_mode != super::WorkspaceMode::Managed {
            return None;
        }
        let (tx, rx) = mpsc::channel(8);
        let driver = self.clone();
        let gateway_id = self.config.gateway_id.clone();
        let api: Api<Namespace> = Api::all(self.watch_client.clone());
        let selector =
            format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE},{LABEL_GATEWAY_ID}={gateway_id}");
        tokio::spawn(async move {
            let mut stream = watcher::watcher(api, watcher::Config::default().labels(&selector))
                .default_backoff()
                .boxed();
            let mut known = BTreeMap::<String, bool>::new();
            let mut listing = BTreeMap::<String, bool>::new();
            while let Some(item) = stream.next().await {
                let changed = match item {
                    Ok(Event::Init) => {
                        listing.clear();
                        false
                    }
                    Ok(Event::InitApply(namespace)) => {
                        if let Some(name) = namespace.metadata.name.clone() {
                            listing.insert(
                                name,
                                is_guarded_managed_namespace(&namespace, &gateway_id),
                            );
                        }
                        false
                    }
                    Ok(Event::InitDone) => {
                        known = std::mem::take(&mut listing);
                        true
                    }
                    Ok(Event::Apply(namespace)) => {
                        namespace.metadata.name.clone().is_some_and(|name| {
                            let guarded = is_guarded_managed_namespace(&namespace, &gateway_id);
                            known.insert(name, guarded) != Some(guarded)
                        })
                    }
                    Ok(Event::Delete(namespace)) => namespace
                        .metadata
                        .name
                        .as_deref()
                        .is_some_and(|name| known.remove(name).is_some()),
                    Err(error) => {
                        debug!(%error, "managed namespace watch error; waiting for kube-runtime recovery");
                        false
                    }
                };
                if !changed {
                    continue;
                }
                let mut desired = BTreeSet::new();
                for (name, guarded) in &known {
                    if *guarded {
                        // Bind before watching; a 403 right after binding is
                        // retried by the watch while the grant propagates.
                        driver.ensure_netpol_guard_role_binding(name).await;
                        desired.insert(name.clone());
                    }
                }
                if tx.send(desired).await.is_err() {
                    return;
                }
            }
        });
        Some(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> GuardBinding {
        GuardBinding {
            cluster_role: "openshell-netpol-guard-openshell".into(),
            service_account: "openshell".into(),
            service_account_namespace: "openshell".into(),
        }
    }

    fn namespace(name: &str, labels: &[(&str, &str)]) -> Namespace {
        Namespace {
            metadata: ObjectMeta {
                name: Some(name.into()),
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| ((*k).into(), (*v).into()))
                        .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn binding_grants_only_the_guard_role_to_the_gateway() {
        let rb = binding().role_binding("openshell-gw-team-a");
        assert_eq!(rb.role_ref.kind, "ClusterRole");
        assert_eq!(rb.role_ref.name, "openshell-netpol-guard-openshell");
        assert_eq!(rb.subjects.as_ref().unwrap().len(), 1);
        assert!(binding().matches(&rb));
        let mut other = rb;
        other.role_ref.name = "cluster-admin".into();
        assert!(!binding().matches(&other));
    }

    #[test]
    fn only_owned_managed_namespaces_are_guarded() {
        let owned = [
            (LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE),
            (LABEL_GATEWAY_ID, "gw"),
        ];
        let prefix = managed_namespace_prefix("gw");
        assert!(is_guarded_managed_namespace(
            &namespace(&format!("{prefix}team"), &owned),
            "gw"
        ));
        // Labelled but not named like a managed namespace.
        assert!(!is_guarded_managed_namespace(
            &namespace("kube-system", &owned),
            "gw"
        ));
        // Another gateway's namespace.
        assert!(!is_guarded_managed_namespace(
            &namespace(&format!("{prefix}team"), &owned),
            "other"
        ));
        let mut terminating = namespace(&format!("{prefix}team"), &owned);
        terminating.metadata.deletion_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::chrono::Utc::now()),
        );
        assert!(!is_guarded_managed_namespace(&terminating, "gw"));
    }
}
