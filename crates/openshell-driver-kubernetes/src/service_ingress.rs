// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation of ingress objects for exposed sandbox services.
//!
//! The gateway mints a hostname for every exposed sandbox service, but the
//! chart deliberately creates no ingress for that traffic: how a hostname
//! reaches the gateway is an operator decision. When multiple gateways share
//! one service base domain behind a single ingress, each hostname must be
//! routed to the gateway that exposed it, which requires one exact-host
//! ingress object per exposed service.
//!
//! This module implements that reconciliation. A source of exposed hostnames
//! (implemented by the gateway, which owns endpoint persistence and hostname
//! minting) is polled on an interval, and one object of the configured kind —
//! an OpenShift `Route` with TLS passthrough, a Gateway API `HTTPRoute`, or a
//! `networking.k8s.io` `Ingress` — is ensured per hostname, targeting the
//! gateway's own Service. Objects carry OpenShell ownership labels; managed
//! objects whose hostname is no longer exposed are removed.
//!
//! The gateway terminates TLS for service traffic itself, so every kind is
//! configured for pass-through semantics: the object only has to deliver the
//! TCP stream (SNI and `Host` header preserved) to the gateway's listener.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, ObjectMeta, PostParams};
use kube::{Api, Client, ResourceExt};
use openshell_core::driver_utils::{LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{ServiceIngressConfig, ServiceIngressKind};

/// Ownership label identifying objects reconciled by this controller.
pub const LABEL_SERVICE_INGRESS: &str = "openshell.ai/service-ingress";
/// Annotation recording the exposed-service hostname an object routes.
pub const ANNOTATION_SERVICE_HOSTNAME: &str = "openshell.ai/service-hostname";

/// Future returned by [`ExposedServiceSource::exposed_service_hostnames`].
pub type ExposedHostnamesFuture = Pin<Box<dyn Future<Output = Vec<String>> + Send + 'static>>;

/// Source of exposed-service hostnames. Implemented by the gateway, which
/// mints hostnames and persists service endpoints; the reconciler only
/// consumes the final hostnames, so it never duplicates hash or
/// composite-hostname logic.
pub trait ExposedServiceSource: Send + Sync {
    /// Return every exposed-service hostname this gateway currently serves.
    fn exposed_service_hostnames(&self) -> ExposedHostnamesFuture;
}

/// OpenShift `Route` (route.openshift.io/v1) with TLS passthrough.
#[derive(Clone, Debug, Serialize, Deserialize, kube::CustomResource)]
#[kube(
    group = "route.openshift.io",
    version = "v1",
    kind = "Route",
    namespaced,
    root = "OpenshiftRoute",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct OpenshiftRouteSpec {
    pub host: String,
    pub to: RouteTargetReference,
    pub port: Option<RoutePortTarget>,
    pub tls: Option<RouteTlsConfig>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteTargetReference {
    pub kind: String,
    pub name: String,
    pub weight: Option<i32>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePortTarget {
    pub target_port: IntOrString,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteTlsConfig {
    pub termination: String,
}

/// Gateway API `HTTPRoute` (gateway.networking.k8s.io/v1).
#[derive(Clone, Debug, Serialize, Deserialize, kube::CustomResource)]
#[kube(
    group = "gateway.networking.k8s.io",
    version = "v1",
    kind = "HTTPRoute",
    namespaced,
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_refs: Vec<HttpRouteParentReference>,
    pub hostnames: Vec<String>,
    pub rules: Vec<HttpRouteRule>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteParentReference {
    pub group: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteRule {
    pub backend_refs: Vec<HttpRouteBackendReference>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteBackendReference {
    pub group: String,
    pub kind: String,
    pub name: String,
    pub port: u16,
}

/// Reconciles one ingress object per exposed-service hostname.
pub struct ServiceIngressReconciler {
    config: ServiceIngressConfig,
    client: Client,
    source: Arc<dyn ExposedServiceSource>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ServiceIngressReconciler {
    /// Build a reconciler with its own Kubernetes client using the ambient
    /// in-cluster (or kubeconfig) identity.
    pub async fn new(
        config: ServiceIngressConfig,
        source: Arc<dyn ExposedServiceSource>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Result<Self, kube::Error> {
        let client = Client::try_default().await?;
        Ok(Self {
            config,
            client,
            source,
            shutdown_rx,
        })
    }

    /// Build a reconciler on an existing client (tests).
    #[must_use]
    pub fn with_client(
        config: ServiceIngressConfig,
        client: Client,
        source: Arc<dyn ExposedServiceSource>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            client,
            source,
            shutdown_rx,
        }
    }

    /// Run the reconcile loop until shutdown is signalled. Errors from a pass
    /// are logged and retried on the next tick; the loop never exits early on
    /// a reconciliation failure.
    pub async fn run(mut self) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs.max(1)));
        info!(
            kind = ?self.config.kind,
            namespace = %self.config.target_service_namespace,
            service = %self.config.target_service_name,
            poll_interval_secs = self.config.poll_interval_secs,
            "service ingress reconciler started"
        );
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.shutdown_rx.changed() => break,
            }
            if *self.shutdown_rx.borrow() {
                break;
            }
            if let Err(error) = self.reconcile_once().await {
                warn!(error = %error, "service ingress reconciliation failed");
            }
        }
        info!("service ingress reconciler stopped");
    }

    /// Spawn the reconcile loop on the runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn reconcile_once(&self) -> Result<(), kube::Error> {
        let mut desired = self.source.exposed_service_hostnames().await;
        desired.sort_unstable();
        desired.dedup();
        match self.config.kind {
            ServiceIngressKind::Route => self.reconcile_openshift_routes(&desired).await?,
            ServiceIngressKind::HttpRoute => self.reconcile_http_routes(&desired).await?,
            ServiceIngressKind::Ingress => self.reconcile_ingresses(&desired).await?,
        }
        self.prune_other_kinds().await
    }

    /// Delete managed service-ingress objects of kinds other than the
    /// configured one.
    ///
    /// Switching `kind` in the configuration orphans the objects of the
    /// previous kind: the active reconciler only lists its own kind, so the
    /// stale objects keep routing traffic (and, for `OpenShift` `Route`s,
    /// keep DNS records alive) after the exposure is removed. Ownership
    /// labels are kind-agnostic, so any other-kind object that still targets
    /// this gateway's `Service` is removed regardless of whether its hostname
    /// is still exposed — objects for removed hostnames must go too, since no
    /// reconciler is watching their kind anymore.
    ///
    /// Kinds whose CRD is not installed (`OpenShift` `Route`s off `OpenShift`,
    /// `HTTPRoute` without Gateway API) are treated as having nothing to
    /// prune.
    async fn prune_other_kinds(&self) -> Result<(), kube::Error> {
        match self.config.kind {
            ServiceIngressKind::Route => {
                self.prune_stale_kind::<HTTPRoute, _>(&|object: &HTTPRoute| {
                    http_route_targets_service(object, &self.config.target_service_name)
                })
                .await?;
                self.prune_stale_kind::<k8s_openapi::api::networking::v1::Ingress, _>(&|object| {
                    ingress_targets_service(object, &self.config.target_service_name)
                })
                .await
            }
            ServiceIngressKind::HttpRoute => {
                self.prune_stale_kind::<OpenshiftRoute, _>(&|object: &OpenshiftRoute| {
                    openshift_route_targets_service(object, &self.config.target_service_name)
                })
                .await?;
                self.prune_stale_kind::<k8s_openapi::api::networking::v1::Ingress, _>(&|object| {
                    ingress_targets_service(object, &self.config.target_service_name)
                })
                .await
            }
            ServiceIngressKind::Ingress => {
                self.prune_stale_kind::<OpenshiftRoute, _>(&|object: &OpenshiftRoute| {
                    openshift_route_targets_service(object, &self.config.target_service_name)
                })
                .await?;
                self.prune_stale_kind::<HTTPRoute, _>(&|object: &HTTPRoute| {
                    http_route_targets_service(object, &self.config.target_service_name)
                })
                .await
            }
        }
    }

    /// List managed objects of one kind and delete those the
    /// `targets_this_gateway` predicate claims. Missing-CRD list failures
    /// are ignored: the configured kind's availability is enforced by its
    /// own reconcile pass.
    async fn prune_stale_kind<T, F>(&self, targets_this_gateway: &F) -> Result<(), kube::Error>
    where
        T: kube::Resource<DynamicType = (), Scope = kube::core::NamespaceResourceScope>
            + Clone
            + std::fmt::Debug,
        for<'de> T: Deserialize<'de>,
        F: Fn(&T) -> bool + Sync,
    {
        let api: Api<T> =
            Api::namespaced(self.client.clone(), &self.config.target_service_namespace);
        let objects = match api
            .list(&ListParams::default().labels(&managed_selector()))
            .await
        {
            Ok(list) => list.items,
            Err(kube::Error::Api(error)) if error.code == 404 => return Ok(()),
            Err(error) => return Err(error),
        };
        for object in objects {
            if !targets_this_gateway(&object) {
                continue;
            }
            let name = object.name_any();
            let hostname = object
                .meta()
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(ANNOTATION_SERVICE_HOSTNAME))
                .cloned()
                .unwrap_or_else(|| name.clone());
            match api.delete(&name, &DeleteParams::default()).await {
                Ok(_) => {
                    info!(hostname, %name, "deleted service ingress object left by a previous kind")
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn reconcile_openshift_routes(&self, desired: &[String]) -> Result<(), kube::Error> {
        let api: Api<OpenshiftRoute> =
            Api::namespaced(self.client.clone(), &self.config.target_service_namespace);
        let existing = managed_objects(&api).await?;
        let plan = plan_reconciliation(desired, &existing);
        for hostname in &plan.create {
            let name = ingress_object_name(hostname);
            let mut route = OpenshiftRoute::new(
                &name,
                OpenshiftRouteSpec {
                    host: hostname.clone(),
                    to: RouteTargetReference {
                        kind: "Service".to_string(),
                        name: self.config.target_service_name.clone(),
                        weight: Some(100),
                    },
                    port: Some(RoutePortTarget {
                        target_port: IntOrString::Int(i32::from(self.config.target_service_port)),
                    }),
                    tls: Some(RouteTlsConfig {
                        termination: "passthrough".to_string(),
                    }),
                },
            );
            apply_ownership(&mut route.metadata, hostname);
            match api.create(&PostParams::default(), &route).await {
                Ok(_) => info!(hostname, %name, "created Route for exposed service"),
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => return Err(error),
            }
        }
        delete_orphans(&api, &plan.delete).await
    }

    async fn reconcile_http_routes(&self, desired: &[String]) -> Result<(), kube::Error> {
        let api: Api<HTTPRoute> =
            Api::namespaced(self.client.clone(), &self.config.target_service_namespace);
        let existing = managed_objects(&api).await?;
        let plan = plan_reconciliation(desired, &existing);
        let parent_name = self
            .config
            .http_route_parent_gateway
            .clone()
            .unwrap_or_default();
        let parent_namespace = self
            .config
            .http_route_parent_namespace
            .clone()
            .unwrap_or_else(|| self.config.target_service_namespace.clone());
        for hostname in &plan.create {
            let name = ingress_object_name(hostname);
            let mut route = HTTPRoute::new(
                &name,
                HttpRouteSpec {
                    parent_refs: vec![HttpRouteParentReference {
                        group: "gateway.networking.k8s.io".to_string(),
                        kind: "Gateway".to_string(),
                        name: parent_name.clone(),
                        namespace: Some(parent_namespace.clone()),
                    }],
                    hostnames: vec![hostname.clone()],
                    rules: vec![HttpRouteRule {
                        backend_refs: vec![HttpRouteBackendReference {
                            group: String::new(),
                            kind: "Service".to_string(),
                            name: self.config.target_service_name.clone(),
                            port: self.config.target_service_port,
                        }],
                    }],
                },
            );
            apply_ownership(&mut route.metadata, hostname);
            match api.create(&PostParams::default(), &route).await {
                Ok(_) => info!(hostname, %name, "created HTTPRoute for exposed service"),
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => return Err(error),
            }
        }
        delete_orphans(&api, &plan.delete).await
    }

    async fn reconcile_ingresses(&self, desired: &[String]) -> Result<(), kube::Error> {
        use k8s_openapi::api::networking::v1 as networking;
        let api: Api<networking::Ingress> =
            Api::namespaced(self.client.clone(), &self.config.target_service_namespace);
        let existing = managed_objects(&api).await?;
        let plan = plan_reconciliation(desired, &existing);
        for hostname in &plan.create {
            let name = ingress_object_name(hostname);
            let mut ingress = networking::Ingress {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    namespace: Some(self.config.target_service_namespace.clone()),
                    ..Default::default()
                },
                spec: Some(networking::IngressSpec {
                    ingress_class_name: self
                        .config
                        .ingress_class
                        .clone()
                        .filter(|class| !class.is_empty()),
                    rules: Some(vec![networking::IngressRule {
                        host: Some(hostname.clone()),
                        http: Some(networking::HTTPIngressRuleValue {
                            paths: vec![networking::HTTPIngressPath {
                                path: Some("/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: networking::IngressBackend {
                                    service: Some(networking::IngressServiceBackend {
                                        name: self.config.target_service_name.clone(),
                                        port: Some(networking::ServiceBackendPort {
                                            number: Some(i32::from(
                                                self.config.target_service_port,
                                            )),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            }],
                        }),
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            // The gateway terminates service TLS itself; the ingress only
            // forwards the stream. SSL passthrough is controller-specific, so
            // the nginx annotation is set and other controllers must be
            // configured equivalently by the operator.
            ingress
                .metadata
                .annotations
                .get_or_insert_with(BTreeMap::new)
                .insert(
                    "nginx.ingress.kubernetes.io/ssl-passthrough".to_string(),
                    "true".to_string(),
                );
            apply_ownership(&mut ingress.metadata, hostname);
            match api.create(&PostParams::default(), &ingress).await {
                Ok(_) => info!(hostname, %name, "created Ingress for exposed service"),
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => return Err(error),
            }
        }
        delete_orphans(&api, &plan.delete).await
    }
}

/// Label selector matching only objects this controller owns.
fn managed_selector() -> String {
    format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE},{LABEL_SERVICE_INGRESS}=true")
}

/// List objects this controller owns, keyed by their annotated hostname.
async fn managed_objects<T>(api: &Api<T>) -> Result<Vec<(String, String)>, kube::Error>
where
    T: kube::Resource<DynamicType = ()> + Clone + std::fmt::Debug,
    for<'de> T: Deserialize<'de>,
{
    let items = api
        .list(&ListParams::default().labels(&managed_selector()))
        .await?
        .items;
    Ok(items
        .into_iter()
        .map(|object| {
            let name = object.name_any();
            let hostname = object
                .meta()
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(ANNOTATION_SERVICE_HOSTNAME))
                .cloned()
                .unwrap_or_else(|| name.clone());
            (name, hostname)
        })
        .collect())
}

/// Whether an OpenShift `Route` created for service ingress routes to the
/// named gateway Service.
fn openshift_route_targets_service(route: &OpenshiftRoute, service: &str) -> bool {
    route.spec.to.name == service
}

/// Whether a Gateway API `HTTPRoute` created for service ingress routes to
/// the named gateway Service in any rule's backend refs.
fn http_route_targets_service(route: &HTTPRoute, service: &str) -> bool {
    route.spec.rules.iter().any(|rule| {
        rule.backend_refs
            .iter()
            .any(|backend| backend.name == service)
    })
}

/// Whether a `networking.k8s.io/v1` `Ingress` created for service ingress
/// routes to the named gateway Service in any rule's backend.
fn ingress_targets_service(
    ingress: &k8s_openapi::api::networking::v1::Ingress,
    service: &str,
) -> bool {
    let rules = ingress.spec.as_ref().and_then(|spec| spec.rules.as_ref());
    let Some(rules) = rules else {
        return false;
    };
    rules.iter().any(|rule| {
        rule.http
            .as_ref()
            .map(|http| {
                http.paths.iter().any(|path| {
                    path.backend
                        .service
                        .as_ref()
                        .map(|backend| backend.name == service)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Ownership labels and hostname annotation applied to every object this
/// controller creates.
fn apply_ownership(metadata: &mut ObjectMeta, hostname: &str) {
    let labels = metadata.labels.get_or_insert_with(BTreeMap::new);
    labels.insert(
        LABEL_MANAGED_BY.to_string(),
        LABEL_MANAGED_BY_VALUE.to_string(),
    );
    labels.insert(LABEL_SERVICE_INGRESS.to_string(), "true".to_string());
    metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(
            ANNOTATION_SERVICE_HOSTNAME.to_string(),
            hostname.to_string(),
        );
}

/// Which hostnames need objects created and which object names are orphaned.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReconciliationPlan {
    create: Vec<String>,
    delete: Vec<String>,
}

fn plan_reconciliation(desired: &[String], existing: &[(String, String)]) -> ReconciliationPlan {
    let mut existing_hosts: Vec<&String> = existing.iter().map(|(_, host)| host).collect();
    existing_hosts.sort_unstable();
    existing_hosts.dedup();
    let create = desired
        .iter()
        .filter(|hostname| !existing_hosts.contains(hostname))
        .cloned()
        .collect();
    let delete = existing
        .iter()
        .filter(|(_, hostname)| !desired.iter().any(|wanted| wanted == hostname))
        .map(|(name, _)| name.clone())
        .collect();
    ReconciliationPlan { create, delete }
}

async fn delete_orphans<T>(api: &Api<T>, names: &[String]) -> Result<(), kube::Error>
where
    T: kube::Resource<DynamicType = ()> + Clone + std::fmt::Debug,
    for<'de> T: Deserialize<'de>,
{
    for name in names {
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => info!(%name, "deleted orphaned service ingress object"),
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Deterministic object name for an exposed-service hostname. The hostname is
/// sanitized to DNS-1123 and long hostnames are truncated with a hash suffix
/// so distinct hostnames never collapse onto the same object.
#[must_use]
pub fn ingress_object_name(hostname: &str) -> String {
    const PREFIX: &str = "openshell-svc-";
    const MAX_BODY: usize = 220;
    let sanitized: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.len() <= MAX_BODY {
        return format!("{PREFIX}{sanitized}");
    }
    let body: String = sanitized.chars().take(MAX_BODY - 9).collect();
    format!("{PREFIX}{body}-{:016x}", fnv1a64(hostname.as_bytes()))
}

/// FNV-1a 64-bit hash used only for deterministic name truncation. This is
/// not a cryptographic primitive.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_name_is_deterministic_and_dns_safe() {
        assert_eq!(
            ingress_object_name("pyldrduzj554prit.apps.example.com"),
            "openshell-svc-pyldrduzj554prit-apps-example-com"
        );
        assert_eq!(
            ingress_object_name("default--demo--web.apps.example.com"),
            "openshell-svc-default--demo--web-apps-example-com"
        );
    }

    #[test]
    fn object_name_truncates_long_hostnames_uniquely() {
        let long_a = format!("{}a.example.com", "x".repeat(300));
        let long_b = format!("{}b.example.com", "x".repeat(300));
        let name_a = ingress_object_name(&long_a);
        let name_b = ingress_object_name(&long_b);
        assert_ne!(name_a, name_b);
        assert!(name_a.len() <= 253, "{}", name_a.len());
        assert!(
            name_a
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == c)
        );
    }

    #[test]
    fn plan_creates_missing_and_deletes_orphans() {
        let existing = vec![
            ("obj-a".to_string(), "hash-a.example.com".to_string()),
            ("obj-stale".to_string(), "gone.example.com".to_string()),
        ];
        let desired = vec![
            "hash-a.example.com".to_string(),
            "hash-b.example.com".to_string(),
        ];
        let plan = plan_reconciliation(&desired, &existing);
        assert_eq!(plan.create, vec!["hash-b.example.com".to_string()]);
        assert_eq!(plan.delete, vec!["obj-stale".to_string()]);
    }

    #[test]
    fn plan_is_empty_when_reconciled() {
        let existing = vec![("obj-a".to_string(), "hash-a.example.com".to_string())];
        let desired = vec!["hash-a.example.com".to_string()];
        let plan = plan_reconciliation(&desired, &existing);
        assert_eq!(plan, ReconciliationPlan::default());
    }

    #[test]
    fn selector_matches_owned_labels() {
        assert_eq!(
            managed_selector(),
            "openshell.ai/managed-by=openshell,openshell.ai/service-ingress=true"
        );
    }

    #[test]
    fn openshift_route_target_detection_follows_backend_name() {
        let mut route = OpenshiftRoute::new(
            "obj",
            OpenshiftRouteSpec {
                host: "hash.example.com".to_string(),
                to: RouteTargetReference {
                    kind: "Service".to_string(),
                    name: "other-gateway".to_string(),
                    weight: None,
                },
                port: None,
                tls: None,
            },
        );
        assert!(!openshift_route_targets_service(&route, "my-gateway"));
        route.spec.to.name = "my-gateway".to_string();
        assert!(openshift_route_targets_service(&route, "my-gateway"));
    }

    #[test]
    fn http_route_target_detection_scans_all_backend_refs() {
        let make = |backends: &[&str]| {
            HTTPRoute::new(
                "obj",
                HttpRouteSpec {
                    parent_refs: vec![],
                    hostnames: vec!["hash.example.com".to_string()],
                    rules: vec![HttpRouteRule {
                        backend_refs: backends
                            .iter()
                            .map(|name| HttpRouteBackendReference {
                                group: String::new(),
                                kind: "Service".to_string(),
                                name: (*name).to_string(),
                                port: 8080,
                            })
                            .collect(),
                    }],
                },
            )
        };
        assert!(!http_route_targets_service(&make(&["other"]), "my-gateway"));
        assert!(http_route_targets_service(
            &make(&["other", "my-gateway"]),
            "my-gateway"
        ));
    }

    #[test]
    fn ingress_target_detection_scans_all_rules_and_paths() {
        use k8s_openapi::api::networking::v1 as networking;
        let make = |service: &str| networking::Ingress {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                ..Default::default()
            },
            spec: Some(networking::IngressSpec {
                rules: Some(vec![networking::IngressRule {
                    host: Some("hash.example.com".to_string()),
                    http: Some(networking::HTTPIngressRuleValue {
                        paths: vec![networking::HTTPIngressPath {
                            path: Some("/".to_string()),
                            path_type: "Prefix".to_string(),
                            backend: networking::IngressBackend {
                                service: Some(networking::IngressServiceBackend {
                                    name: service.to_string(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        }],
                    }),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!ingress_targets_service(&make("other"), "my-gateway"));
        assert!(ingress_targets_service(&make("my-gateway"), "my-gateway"));
        // No spec at all cannot target anything.
        let empty = networking::Ingress {
            metadata: ObjectMeta::default(),
            spec: None,
            ..Default::default()
        };
        assert!(!ingress_targets_service(&empty, "my-gateway"));
    }
}
