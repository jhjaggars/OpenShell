// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod lifecycle_reconciliation {
    use super::*;

    // Availability reads run concurrently. Match pending responses by path
    // rather than assuming tokio::join! polls its branches in a fixed order.
    fn read_only_driver(steps: Vec<KubeTestStep>) -> ScriptedDriver {
        let steps = Arc::new(std::sync::Mutex::new(VecDeque::from(steps)));
        let pending = steps.clone();
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let pending = pending.clone();
            async move {
                assert_eq!(
                    request.method(),
                    http::Method::GET,
                    "reconciliation must preserve the replacement"
                );
                let mut pending = pending.lock().unwrap();
                let index = pending
                    .iter()
                    .position(|(method, path, _)| {
                        request.method() == method && request.uri().path() == *path
                    })
                    .unwrap_or_else(|| panic!("unexpected read {}", request.uri().path()));
                let (_, _, response) = pending.remove(index).unwrap();
                Ok::<_, std::convert::Infallible>(response)
            }
        });
        let client = Client::new(service, "openshell");
        let driver = KubernetesComputeDriver::with_test_client(client, KubernetesComputeConfig::default());
        (driver, steps, Arc::default())
    }

    fn snapshot(version: &str, phase: Option<SandboxRuntimeBootstrapPhase>) -> serde_json::Value {
        let mut object = serde_json::json!({
            "apiVersion": format!("{SANDBOX_GROUP}/{version}"), "kind": "Sandbox",
            "metadata": {
                "name": "sandbox-cr", "namespace": "openshell", "uid": "cr-uid",
                "resourceVersion": "42",
                "labels": {LABEL_SANDBOX_ID: "sandbox-1", LABEL_SANDBOX_WORKSPACE: "team-a"},
                "annotations": {
                    crate::resource_admission::CONFIG_USED: "false",
                    crate::resource_admission::IDENTITIES: "{}",
                    ANNOTATION_SANDBOX_RUNTIME_READINESS: "unavailable"
                }
            },
            "spec": {"podTemplate": {"spec": {
                "automountServiceAccountToken": false,
                "volumes": [{"name": SANDBOX_BOOTSTRAP_VOLUME_NAME,
                    "secret": {"secretName": "os-sandbox-sandbox-1-gen2"}}]
            }}}
        });
        object["spec"][if version == SANDBOX_VERSION_V1ALPHA1 {
            "replicas"
        } else {
            "operatingMode"
        }] = if version == SANDBOX_VERSION_V1ALPHA1 {
            serde_json::json!(0)
        } else {
            serde_json::json!("Suspended")
        };
        if let Some(phase) = phase {
            object["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAPPING] =
                serde_json::json!("true");
            object["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_OPERATION] =
                serde_json::json!("stop");
            object["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_PHASE] =
                serde_json::json!(phase.as_str());
            object["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_SUPERVISOR_UID] =
                serde_json::json!("old-supervisor-uid");
        }
        object
    }

    fn restarted_snapshot(version: &str) -> serde_json::Value {
        let mut object = snapshot(version, Some(SandboxRuntimeBootstrapPhase::Preparing));
        object["metadata"]["resourceVersion"] = serde_json::json!("43");
        let annotations = &mut object["metadata"]["annotations"];
        annotations[ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_OPERATION] = serde_json::json!("restart");
        annotations[ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_STARTED_AT] =
            serde_json::json!(openshell_core::time::now_ms().to_string());
        annotations[ANNOTATION_SANDBOX_RUNTIME_SUPERVISOR_UID] =
            serde_json::json!("new-supervisor-uid");
        annotations[ANNOTATION_SANDBOX_RUNTIME_NETWORK_POLICY_UID] =
            serde_json::json!("sandbox-workload-fence-uid");
        annotations[ANNOTATION_SANDBOX_RUNTIME_NETWORK_POLICY_GENERATION] = serde_json::json!("1");
        object["spec"][if version == SANDBOX_VERSION_V1ALPHA1 {
            "replicas"
        } else {
            "operatingMode"
        }] = if version == SANDBOX_VERSION_V1ALPHA1 {
            serde_json::json!(1)
        } else {
            serde_json::json!("Running")
        };
        object
    }

    fn listed(version: &str, object: serde_json::Value) -> KubeTestStep {
        (
            http::Method::GET,
            if version == SANDBOX_VERSION_V1ALPHA1 {
                "/apis/agents.x-k8s.io/v1alpha1/namespaces/openshell/sandboxes"
            } else {
                "/apis/agents.x-k8s.io/v1beta1/namespaces/openshell/sandboxes"
            },
            kube_test_response(
                http::StatusCode::OK,
                serde_json::json!({
                    "apiVersion": format!("{SANDBOX_GROUP}/{version}"),
                    "kind": "SandboxList", "items": [object]
                }),
            ),
        )
    }

    fn refreshed(version: &str, object: serde_json::Value) -> KubeTestStep {
        (
            http::Method::GET,
            if version == SANDBOX_VERSION_V1ALPHA1 {
                "/apis/agents.x-k8s.io/v1alpha1/namespaces/openshell/sandboxes/sandbox-cr"
            } else {
                "/apis/agents.x-k8s.io/v1beta1/namespaces/openshell/sandboxes/sandbox-cr"
            },
            kube_test_response(http::StatusCode::OK, object),
        )
    }

    fn preparing_dependencies() -> Vec<KubeTestStep> {
        let mut fence = workload_fence("openshell", &SandboxRuntimeNames::new("sandbox-1"), 5500);
        for (policy, component) in [
            (&mut fence.workload_policy, "sandbox-workload-fence"),
            (&mut fence.supervisor_policy, "sandbox-supervisor-egress"),
        ] {
            policy.metadata.labels.get_or_insert_default().extend([
                (
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                ),
                ("openshell.ai/component".to_string(), component.to_string()),
            ]);
            policy.metadata.uid = Some(format!("{component}-uid"));
            policy.metadata.generation = Some(1);
        }
        let workload = || {
            (
                http::Method::GET,
                "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/openshell-sandbox-workloads",
                kube_test_response(
                    http::StatusCode::OK,
                    serde_json::to_value(&fence.workload_policy).unwrap(),
                ),
            )
        };
        let supervisor = || {
            (
                http::Method::GET,
                "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/openshell-sandbox-supervisors",
                kube_test_response(
                    http::StatusCode::OK,
                    serde_json::to_value(&fence.supervisor_policy).unwrap(),
                ),
            )
        };
        vec![
            workload(),
            supervisor(),
            workload(),
            // The recorded mode (none: supervisor-initiated) must still match
            // the role the boundary Service selects.
            (
                http::Method::GET,
                "/api/v1/namespaces/openshell/services/os-boundary-sandbox-1",
                kube_test_response(
                    http::StatusCode::OK,
                    serde_json::json!({
                        "apiVersion": "v1", "kind": "Service",
                        "metadata": {"name": "os-boundary-sandbox-1"},
                        "spec": {"selector": {
                            BOUNDARY_PAIR_LABEL: "sandbox-1",
                            BOUNDARY_ROLE_LABEL: "workload"
                        }}
                    }),
                ),
            ),
            (
                http::Method::GET,
                "/api/v1/namespaces/openshell/pods/os-supervisor-sandbox-1",
                kube_test_response(
                    http::StatusCode::OK,
                    serde_json::json!({
                        "apiVersion": "v1", "kind": "Pod",
                        "metadata": {"name": "os-supervisor-sandbox-1", "uid": "new-supervisor-uid"},
                        "spec": {"containers": []}, "status": {"phase": "Pending"}
                    }),
                ),
            ),
            (
                http::Method::GET,
                "/api/v1/namespaces/openshell/services/os-boundary-sandbox-1",
                kube_test_response(
                    http::StatusCode::OK,
                    serde_json::json!({
                        "apiVersion": "v1", "kind": "Service", "metadata": {"name": "os-boundary-sandbox-1"}
                    }),
                ),
            ),
            workload(),
            supervisor(),
        ]
    }

    #[tokio::test]
    async fn stale_stop_snapshots_cannot_delete_a_restarted_supervisor() {
        for version in [SANDBOX_VERSION_V1ALPHA1, SANDBOX_VERSION_V1BETA1] {
            for phase in [
                None,
                Some(SandboxRuntimeBootstrapPhase::Releasing),
                Some(SandboxRuntimeBootstrapPhase::Suspending),
            ] {
                // Restart completed its critical section after LIST. The old
                // implementation either deleted the replacement or attempted
                // cleanup using the old stop transition. Every allowed request
                // here is a read; unexpected DELETE/PATCH/Secret access fails.
                let mut steps = vec![
                    listed(version, snapshot(version, phase)),
                    refreshed(version, restarted_snapshot(version)),
                ];
                steps.extend(preparing_dependencies());
                let (driver, steps, _) = read_only_driver(steps);
                driver.sandbox_api_version.set(version).unwrap();
                driver.reconcile_sandbox_runtime_resources().await;
                assert!(steps.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn reconciliation_skips_a_busy_restart_and_retries_after_release() {
        let version = SANDBOX_VERSION_V1BETA1;
        let mut steps = vec![
            listed(version, snapshot(version, None)),
            listed(version, snapshot(version, None)),
            refreshed(version, restarted_snapshot(version)),
        ];
        steps.extend(preparing_dependencies());
        let (driver, steps, _) = read_only_driver(steps);
        driver.sandbox_api_version.set(version).unwrap();
        let clone = driver.clone();
        let guard = clone
            .lifecycle_gates
            .gate_for("sandbox-1")
            .lock_owned()
            .await;
        // Restart may already have created its Pod while the CR is still
        // stopped. Reconciliation must skip even reading that companion.
        driver.reconcile_sandbox_runtime_resources().await;
        drop(guard);
        driver.reconcile_sandbox_runtime_resources().await;
        assert!(steps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_cannot_adopt_a_replaced_sandbox_cr() {
        let version = SANDBOX_VERSION_V1BETA1;
        let mut replacement = restarted_snapshot(version);
        replacement["metadata"]["uid"] = serde_json::json!("another-cr-uid");
        let (driver, steps, _) = read_only_driver(vec![
            listed(version, snapshot(version, None)),
            refreshed(version, replacement),
        ]);
        driver.sandbox_api_version.set(version).unwrap();
        driver.reconcile_sandbox_runtime_resources().await;
        assert!(steps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_requests_share_gates_across_clones_without_blocking_other_sandboxes() {
        let driver = KubernetesComputeDriver::new_for_test(KubernetesComputeConfig::default());
        let clone = driver.clone();
        let guard = clone
            .lifecycle_gates
            .gate_for("sandbox-1")
            .lock_owned()
            .await;
        let sandbox = Sandbox {
            id: "sandbox-1".to_string(),
            ..Default::default()
        };
        let mut create = Box::pin(driver.create_sandbox(&sandbox));
        let mut stop = Box::pin(driver.stop_sandbox("sandbox-1"));
        let mut delete = Box::pin(driver.delete_sandbox("sandbox-1"));
        let mut start = Box::pin(driver.start_sandbox("sandbox-1", "", &[], ""));
        assert!(futures::poll!(&mut create).is_pending());
        assert!(futures::poll!(&mut stop).is_pending());
        assert!(futures::poll!(&mut delete).is_pending());
        assert!(futures::poll!(&mut start).is_pending());
        driver
            .start_sandbox("sandbox-2", "", &[], "")
            .await
            .unwrap_err();
        // Cancel queued mutations, then let start obtain the gate. Its normal
        // generation validation proves release did not leave it deadlocked.
        drop(create);
        drop(stop);
        drop(delete);
        drop(guard);
        assert!(matches!(
            start.await,
            Err(KubernetesDriverError::InvalidArgument(_))
        ));
    }
}

/// Golden request sequences for each branch of the per-sandbox runtime
/// reconcile. These pin the observable Kubernetes API traffic so the body can
/// be refactored and shared with the `NetworkPolicy` guard without changing what
/// the 30-second pass does.
mod reconcile_request_sequences {
    use super::*;

    pub(super) const NS: &str = "openshell";
    pub(super) const CR_PATH: &str = "/apis/agents.x-k8s.io/v1beta1/namespaces/openshell/sandboxes/sandbox-cr";
    pub(super) const LIST_PATH: &str = "/apis/agents.x-k8s.io/v1beta1/namespaces/openshell/sandboxes";
    pub(super) const WORKLOAD_FENCE_PATH: &str =
        "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/openshell-sandbox-workloads";
    pub(super) const SUPERVISOR_FENCE_PATH: &str =
        "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/openshell-sandbox-supervisors";
    pub(super) const POLICIES_PATH: &str =
        "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies";

    pub(super) type Recorded = (http::Method, String, Option<serde_json::Value>);
    type Routes = std::collections::HashMap<(http::Method, String), VecDeque<(u16, serde_json::Value)>>;

    /// A fake API server holding a small object world. Reads of unknown paths
    /// return 404; writes must be scripted, so an unexpected write fails the
    /// test. Every request is recorded with its decoded body.
    #[derive(Clone, Default)]
    pub(super) struct World {
        routes: Arc<std::sync::Mutex<Routes>>,
        pub(super) log: Arc<std::sync::Mutex<Vec<Recorded>>>,
    }

    impl World {
        /// Answer `method path` with `status body`. Several answers for one
        /// route are served in order; the last one repeats.
        pub(super) fn on(&self, method: http::Method, path: &str, status: u16, body: serde_json::Value) -> &Self {
            self.routes
                .lock()
                .unwrap()
                .entry((method, path.to_string()))
                .or_default()
                .push_back((status, body));
            self
        }

        pub(super) fn requests(&self) -> Vec<String> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .map(|(method, path, _)| format!("{method} {path}"))
                .collect()
        }

        pub(super) fn bodies(&self, method: &http::Method, path: &str) -> Vec<serde_json::Value> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, p, _)| m == method && p == path)
                .filter_map(|(_, _, body)| body.clone())
                .collect()
        }

        pub(super) fn driver(&self, config: KubernetesComputeConfig) -> KubernetesComputeDriver {
            use http_body_util::BodyExt as _;
            let world = self.clone();
            let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
                let world = world.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_string();
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let body = (!body.is_empty()).then(|| serde_json::from_slice(&body).unwrap());
                    world.log.lock().unwrap().push((method.clone(), path.clone(), body));
                    let answer = {
                        let mut routes = world.routes.lock().unwrap();
                        routes.get_mut(&(method.clone(), path.clone())).and_then(|queue| {
                            if queue.len() > 1 { queue.pop_front() } else { queue.front().cloned() }
                        })
                    };
                    let (status, body) = answer.unwrap_or_else(|| {
                        assert!(
                            method == http::Method::GET || method == http::Method::DELETE,
                            "unexpected write {method} {path}"
                        );
                        (404, not_found_status())
                    });
                    Ok::<_, std::convert::Infallible>(kube_test_response(
                        http::StatusCode::from_u16(status).unwrap(),
                        body,
                    ))
                }
            });
            let client = Client::new(service, NS);
            let driver = KubernetesComputeDriver::with_test_client(client, config);
            driver.sandbox_api_version.set(SANDBOX_VERSION_V1BETA1).unwrap();
            driver
        }
    }

    impl World {
        /// Drop every scripted answer for `method path`.
        pub(super) fn forget(&self, method: http::Method, path: &str) {
            self.routes.lock().unwrap().remove(&(method, path.to_string()));
        }
    }

    pub(super) fn not_found_status() -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": "not found", "reason": "NotFound", "code": 404
        })
    }

    pub(super) fn conflict_status() -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": "the object has been modified", "reason": "Conflict", "code": 409
        })
    }

    /// A sandbox CR whose stored generation binds the namespace fence with
    /// UID `fence-uid` at generation 1.
    pub(super) fn sandbox(running: bool) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "agents.x-k8s.io/v1beta1", "kind": "Sandbox",
            "metadata": {
                "name": "sandbox-cr", "namespace": NS, "uid": "cr-uid", "resourceVersion": "42",
                "labels": {LABEL_SANDBOX_ID: "sandbox-1", LABEL_SANDBOX_WORKSPACE: "team-a",
                    LABEL_MANAGED_BY: LABEL_MANAGED_BY_VALUE},
                "annotations": {
                    crate::resource_admission::CONFIG_USED: "false",
                    crate::resource_admission::IDENTITIES: "{}",
                    ANNOTATION_SANDBOX_RUNTIME_READINESS: if running { "ready" } else { "unavailable" },
                    ANNOTATION_SANDBOX_RUNTIME_GENERATION: "gen2",
                    ANNOTATION_SANDBOX_RUNTIME_WORKLOAD_UID: "workload-uid",
                    ANNOTATION_SANDBOX_RUNTIME_SUPERVISOR_UID: "supervisor-uid",
                    ANNOTATION_SANDBOX_RUNTIME_NETWORK_POLICY_UID: "fence-uid",
                    ANNOTATION_SANDBOX_RUNTIME_NETWORK_POLICY_GENERATION: "1"
                }
            },
            "spec": {
                "operatingMode": if running { "Running" } else { "Suspended" },
                "podTemplate": {"spec": {
                    "automountServiceAccountToken": false,
                    "volumes": [{"name": SANDBOX_BOOTSTRAP_VOLUME_NAME,
                        "secret": {"secretName": "os-sandbox-sandbox-1-gen2"}}]
                }}
            }
        })
    }

    pub(super) fn sandbox_list(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "agents.x-k8s.io/v1beta1", "kind": "SandboxList", "items": items})
    }

    /// The two namespace fences as the API server stores them.
    pub(super) fn stored_fences() -> (NetworkPolicy, NetworkPolicy) {
        let mut fence = workload_fence(NS, &SandboxRuntimeNames::new("sandbox-1"), 5500);
        for (policy, component, uid) in [
            (&mut fence.workload_policy, "sandbox-workload-fence", "fence-uid"),
            (&mut fence.supervisor_policy, "sandbox-supervisor-egress", "supervisor-fence-uid"),
        ] {
            policy.metadata.labels.get_or_insert_default().extend([
                (LABEL_MANAGED_BY.to_string(), LABEL_MANAGED_BY_VALUE.to_string()),
                ("openshell.ai/component".to_string(), component.to_string()),
            ]);
            policy.metadata.uid = Some(uid.to_string());
            policy.metadata.generation = Some(1);
            policy.metadata.resource_version = Some("100".to_string());
        }
        (fence.workload_policy, fence.supervisor_policy)
    }

    pub(super) fn world_with(sandbox: serde_json::Value) -> World {
        let world = World::default();
        let (workload, supervisor) = stored_fences();
        world
            .on(http::Method::GET, LIST_PATH, 200, sandbox_list(vec![sandbox.clone()]))
            .on(http::Method::GET, CR_PATH, 200, sandbox)
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::GET, SUPERVISOR_FENCE_PATH, 200, serde_json::to_value(supervisor).unwrap());
        world
    }

    pub(super) fn world_running_pods(world: &World) {
        world
            .on(http::Method::GET, "/api/v1/namespaces/openshell/services/os-boundary-sandbox-1", 200, serde_json::json!({
                "apiVersion": "v1", "kind": "Service", "metadata": {"name": "os-boundary-sandbox-1"},
                "spec": {"selector": {BOUNDARY_PAIR_LABEL: "sandbox-1", BOUNDARY_ROLE_LABEL: "workload"}}
            }))
            .on(http::Method::GET, "/api/v1/namespaces/openshell/pods/sandbox-cr", 200, serde_json::json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "sandbox-cr", "uid": "workload-uid"},
                "spec": {"containers": []}, "status": {"phase": "Running"}
            }))
            .on(http::Method::GET, "/api/v1/namespaces/openshell/pods/os-supervisor-sandbox-1", 200, serde_json::json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "os-supervisor-sandbox-1", "uid": "supervisor-uid"},
                "spec": {"containers": []},
                "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}
            }));
    }

    fn suspended(world: &World) -> bool {
        world.bodies(&http::Method::PATCH, CR_PATH).iter().any(|body| {
            body["spec"]["operatingMode"] == "Suspended"
                && body["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_PHASE]
                    == SandboxRuntimeBootstrapPhase::Suspending.as_str()
        })
    }

    /// Run one pass over the single listed sandbox and return its outcome.
    pub(super) async fn reconcile(world: &World) -> ReconcileOutcome {
        let outcomes = world
            .driver(KubernetesComputeConfig::default())
            .reconcile_sandbox_runtime_resources()
            .await;
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        outcomes[0].outcome
    }

    fn suspended_by(trigger: SuspendTrigger) -> ReconcileOutcome {
        ReconcileOutcome::Suspended { trigger, write: SuspendWrite::Written }
    }

    fn get(path: &str) -> String {
        format!("GET {path}")
    }
    fn patch(path: &str) -> String {
        format!("PATCH {path}")
    }

    #[tokio::test]
    async fn refresh_not_found_stops_before_any_dependency_read() {
        let world = world_with(sandbox(true));
        world.on(http::Method::GET, CR_PATH, 404, not_found_status());
        world.routes.lock().unwrap().get_mut(&(http::Method::GET, CR_PATH.to_string())).unwrap().pop_front();
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, ReconcileOutcome::Skipped(SkipReason::RefreshNotFound));
        assert_eq!(world.requests(), [get(LIST_PATH), get(CR_PATH)]);
    }

    #[tokio::test]
    async fn admission_precondition_suspends_without_reading_fences() {
        let mut object = sandbox(true);
        object["metadata"]["annotations"]
            .as_object_mut()
            .unwrap()
            .remove(crate::resource_admission::IDENTITIES);
        let world = world_with(object);
        world.on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::AdmissionRevalidation));
        assert_eq!(world.requests(), [get(LIST_PATH), get(CR_PATH), patch(CR_PATH)]);
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn altered_workload_fence_suspends() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.spec.as_mut().unwrap().egress = Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyEgressRule::default()]);
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceSpec { supervisor: false }));
        assert_eq!(
            world.requests(),
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                // The 30-second pass classifies before failing closed.
                get(WORKLOAD_FENCE_PATH),
                patch(CR_PATH),
            ]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn label_stripped_workload_fence_is_restored_without_suspending() {
        let world = world_with(sandbox(true));
        let (workload, _) = stored_fences();
        let mut stripped = workload.clone();
        stripped.metadata.labels = None;
        world.forget(http::Method::GET, WORKLOAD_FENCE_PATH);
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&stripped).unwrap())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&stripped).unwrap())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&workload).unwrap())
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&workload).unwrap());
        world_running_pods(&world);
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, ReconcileOutcome::Reconciled);
        assert!(!suspended(&world), "{:#?}", world.requests());
        assert_eq!(
            world.requests()[..6],
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                get(WORKLOAD_FENCE_PATH),
                format!("PUT {WORKLOAD_FENCE_PATH}"),
                get(SUPERVISOR_FENCE_PATH),
            ]
        );
        let put = &world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH)[0];
        assert_eq!(put["metadata"]["labels"][LABEL_MANAGED_BY], LABEL_MANAGED_BY_VALUE);
        assert_eq!(put["metadata"]["resourceVersion"], "100");
    }

    #[tokio::test]
    async fn label_stripped_and_widened_fence_still_suspends() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.metadata.labels = None;
        workload.spec.as_mut().unwrap().egress =
            Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyEgressRule::default()]);
        world.forget(http::Method::GET, WORKLOAD_FENCE_PATH);
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceSpec { supervisor: false }));
        assert!(!world.requests().iter().any(|r| r.starts_with("PUT")));
    }

    #[tokio::test]
    async fn missing_workload_fence_is_created_then_generation_mismatch_suspends() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.metadata.uid = Some("recreated-uid".to_string());
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 404, not_found_status())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&workload).unwrap())
            .on(http::Method::POST, POLICIES_PATH, 201, serde_json::to_value(&workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceGeneration));
        assert_eq!(
            world.requests(),
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                format!("POST {POLICIES_PATH}"),
                get(SUPERVISOR_FENCE_PATH),
                get(WORKLOAD_FENCE_PATH),
                patch(CR_PATH),
            ]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn fence_generation_change_suspends_a_running_generation() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.metadata.generation = Some(2);
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceGeneration));
        assert_eq!(
            world.requests(),
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                get(SUPERVISOR_FENCE_PATH),
                get(WORKLOAD_FENCE_PATH),
                patch(CR_PATH),
            ]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn fence_generation_change_leaves_a_stopped_sandbox_alone() {
        let world = world_with(sandbox(false));
        let (mut workload, _) = stored_fences();
        workload.metadata.generation = Some(2);
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world.on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap());
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, ReconcileOutcome::Reconciled);
        assert!(!suspended(&world), "{:#?}", world.requests());
    }

    #[tokio::test]
    async fn missing_boundary_service_suspends_a_running_generation() {
        let world = world_with(sandbox(true));
        world.on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::ControlPath));
        assert_eq!(
            world.requests(),
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                get(SUPERVISOR_FENCE_PATH),
                get(WORKLOAD_FENCE_PATH),
                get("/api/v1/namespaces/openshell/services/os-boundary-sandbox-1"),
                patch(CR_PATH),
            ]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn healthy_running_generation_is_only_read() {
        let world = world_with(sandbox(true));
        world_running_pods(&world);
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, ReconcileOutcome::Reconciled);
        let boundary = "/api/v1/namespaces/openshell/services/os-boundary-sandbox-1";
        let supervisor = "/api/v1/namespaces/openshell/pods/os-supervisor-sandbox-1";
        assert_eq!(
            world.requests(),
            [
                get(LIST_PATH),
                get(CR_PATH),
                get(WORKLOAD_FENCE_PATH),
                get(SUPERVISOR_FENCE_PATH),
                get(WORKLOAD_FENCE_PATH),
                get(boundary),
                get("/api/v1/namespaces/openshell/pods/sandbox-cr"),
                get(supervisor),
                get(supervisor),
                get(boundary),
                get(WORKLOAD_FENCE_PATH),
                get(SUPERVISOR_FENCE_PATH),
                get(supervisor),
            ]
        );
    }

    #[tokio::test]
    async fn missing_runtime_control_policy_suspends() {
        let mut object = sandbox(true);
        let annotations = &mut object["metadata"]["annotations"];
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_CONNECTION] = serde_json::json!("runtime-initiated");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_GENERATION] = serde_json::json!("gen2");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_UID] = serde_json::json!("control-uid");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_GENERATION] = serde_json::json!("1");
        let world = world_with(object);
        world
            .on(http::Method::GET, "/api/v1/namespaces/openshell/services/os-supervisor-sandbox-1", 200, serde_json::json!({
                "apiVersion": "v1", "kind": "Service", "metadata": {"name": "os-supervisor-sandbox-1"},
                "spec": {"selector": {BOUNDARY_PAIR_LABEL: "sandbox-1", BOUNDARY_ROLE_LABEL: "supervisor"},
                    "publishNotReadyAddresses": true}
            }))
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::ControlPolicy { shared: false }));
        assert_eq!(
            world.requests()[5..],
            [
                get("/api/v1/namespaces/openshell/services/os-supervisor-sandbox-1"),
                get("/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/os-control-sandbox-1"),
                patch(CR_PATH),
            ]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn releasing_sandbox_runs_only_the_release_path() {
        let mut object = sandbox(false);
        let annotations = &mut object["metadata"]["annotations"];
        annotations[ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAPPING] = serde_json::json!("true");
        annotations[ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_OPERATION] = serde_json::json!("stop");
        annotations[ANNOTATION_SANDBOX_RUNTIME_BOOTSTRAP_PHASE] =
            serde_json::json!(SandboxRuntimeBootstrapPhase::Releasing.as_str());
        let world = world_with(object);
        world.on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, ReconcileOutcome::Released);
        let requests = world.requests();
        assert!(!requests.iter().any(|r| r.contains("networkpolicies")), "{requests:#?}");
        assert_eq!(requests.last().unwrap(), &patch(CR_PATH));
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn suspension_conflict_is_reported_as_raced() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.metadata.generation = Some(2);
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 409, conflict_status());
        let outcome = reconcile(&world).await;
        assert_eq!(
            outcome,
            ReconcileOutcome::Suspended {
                trigger: SuspendTrigger::FenceGeneration,
                write: SuspendWrite::Raced
            }
        );
    }

    #[tokio::test]
    async fn busy_gate_skips_the_sandbox_after_listing() {
        let world = world_with(sandbox(true));
        let driver = world.driver(KubernetesComputeConfig::default());
        let _held = driver.lifecycle_gates.gate_for("sandbox-1").lock_owned().await;
        let outcomes = driver.reconcile_sandbox_runtime_resources().await;
        assert_eq!(outcomes[0].outcome, ReconcileOutcome::Skipped(SkipReason::GateBusy));
        assert_eq!(world.requests(), [get(LIST_PATH)]);
    }
}

/// The `NetworkPolicy` guard's repair executor against the fake API world.
mod netpol_guard_repairs {
    use super::reconcile_request_sequences::*;
    use super::*;
    use crate::driver::netpol_guard::{Drift, NoAction, PolicyKey};
    use crate::driver::netpol_guard_shell::{GuardOutcome, REPAIR_BUDGET, RepairWrite};

    const NAMESPACE_PATH: &str = "/api/v1/namespaces/openshell";
    const CONTROL_PATH: &str =
        "/apis/networking.k8s.io/v1/namespaces/openshell/networkpolicies/os-control-sandbox-1";

    fn guarded_world(sandbox: serde_json::Value) -> World {
        let world = world_with(sandbox);
        world.on(
            http::Method::GET,
            NAMESPACE_PATH,
            200,
            serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS, "uid": "ns-uid"}}),
        );
        world
    }

    /// Serve `first` for the next read of `path`, then `then` for every later read.
    fn reads(world: &World, path: &str, first: &NetworkPolicy, then: &NetworkPolicy) {
        world.forget(http::Method::GET, path);
        world
            .on(http::Method::GET, path, 200, serde_json::to_value(first).unwrap())
            .on(http::Method::GET, path, 200, serde_json::to_value(then).unwrap());
    }

    fn workload_key() -> PolicyKey {
        PolicyKey::new(NS, "openshell-sandbox-workloads")
    }

    fn reconciled(outcome: &GuardOutcome) -> Vec<ReconcileOutcome> {
        match outcome {
            GuardOutcome::Checked { reconciled, .. } => {
                reconciled.iter().map(|(_, outcome)| *outcome).collect()
            }
            GuardOutcome::Skipped(reason) => panic!("skipped: {reason}"),
        }
    }

    fn write_of(outcome: &GuardOutcome) -> (Drift, RepairWrite) {
        match outcome {
            GuardOutcome::Checked { drift, write, .. } => (*drift, write.clone()),
            GuardOutcome::Skipped(reason) => panic!("skipped: {reason}"),
        }
    }

    fn widened(policy: &NetworkPolicy) -> NetworkPolicy {
        let mut policy = policy.clone();
        policy.spec.as_mut().unwrap().egress =
            Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyEgressRule::default()]);
        policy.metadata.generation = Some(2);
        policy.metadata.resource_version = Some("101".into());
        policy
    }

    fn repaired(policy: &NetworkPolicy, generation: i64) -> NetworkPolicy {
        let mut policy = policy.clone();
        policy.metadata.generation = Some(generation);
        policy.metadata.resource_version = Some("200".into());
        policy
    }

    fn suspended_by_fence_generation() -> ReconcileOutcome {
        ReconcileOutcome::Suspended {
            trigger: SuspendTrigger::FenceGeneration,
            write: SuspendWrite::Written,
        }
    }

    #[tokio::test]
    async fn edited_fence_is_replaced_under_its_resource_version_and_bound_sandboxes_suspend() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let edited = widened(&fence);
        reads(&world, WORKLOAD_FENCE_PATH, &edited, &repaired(&fence, 3));
        world
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let outcome = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&outcome), (Drift::Spec, RepairWrite::Replaced));
        assert_eq!(reconciled(&outcome), [suspended_by_fence_generation()]);
        let put = &world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH)[0];
        assert_eq!(put["metadata"]["resourceVersion"], "101");
        assert_eq!(put["spec"]["egress"], serde_json::json!([]), "the widening must be removed: {put}");
        assert_eq!(put["metadata"]["labels"][LABEL_MANAGED_BY], LABEL_MANAGED_BY_VALUE);
        assert!(put["metadata"]["annotations"]["openshell.ai/rendered-by"].is_string());
    }

    #[tokio::test]
    async fn label_only_drift_is_restored_without_suspending() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut stripped = fence.clone();
        stripped.metadata.labels = None;
        reads(&world, WORKLOAD_FENCE_PATH, &stripped, &repaired(&fence, 1));
        world.on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 1)).unwrap());
        world_running_pods(&world);
        let driver = world.driver(KubernetesComputeConfig::default());
        let outcome = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&outcome), (Drift::Metadata, RepairWrite::Replaced));
        assert_eq!(reconciled(&outcome), [ReconcileOutcome::Reconciled]);
        assert!(world.bodies(&http::Method::PATCH, CR_PATH).is_empty());
    }

    #[tokio::test]
    async fn deleted_fence_is_recreated_and_bound_sandboxes_suspend() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut recreated = fence.clone();
        recreated.metadata.uid = Some("recreated-uid".into());
        world.forget(http::Method::GET, WORKLOAD_FENCE_PATH);
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 404, not_found_status())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&recreated).unwrap())
            .on(http::Method::POST, POLICIES_PATH, 201, serde_json::to_value(&recreated).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = world.driver(KubernetesComputeConfig::default()).guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&outcome), (Drift::Missing, RepairWrite::Created));
        assert_eq!(reconciled(&outcome), [suspended_by_fence_generation()]);
    }

    #[tokio::test]
    async fn missing_control_policy_is_never_created_but_its_sandbox_suspends() {
        let mut object = sandbox(true);
        let annotations = &mut object["metadata"]["annotations"];
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_CONNECTION] = serde_json::json!("runtime-initiated");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_GENERATION] = serde_json::json!("gen2");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_UID] = serde_json::json!("control-uid");
        annotations[ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_GENERATION] = serde_json::json!("1");
        let world = guarded_world(object);
        world
            .on(http::Method::GET, "/api/v1/namespaces/openshell/services/os-supervisor-sandbox-1", 200, serde_json::json!({
                "apiVersion": "v1", "kind": "Service", "metadata": {"name": "os-supervisor-sandbox-1"},
                "spec": {"selector": {BOUNDARY_PAIR_LABEL: "sandbox-1", BOUNDARY_ROLE_LABEL: "supervisor"},
                    "publishNotReadyAddresses": true}
            }))
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = world
            .driver(KubernetesComputeConfig::default())
            .guard_policy_key(&PolicyKey::new(NS, "os-control-sandbox-1"))
            .await;
        assert_eq!(write_of(&outcome), (Drift::Missing, RepairWrite::None(NoAction::PerSandboxMissing)));
        assert_eq!(
            reconciled(&outcome),
            [ReconcileOutcome::Suspended { trigger: SuspendTrigger::ControlPolicy { shared: false }, write: SuspendWrite::Written }]
        );
        assert!(!world.requests().iter().any(|r| r.starts_with("POST")), "{:#?}", world.requests());
    }

    #[tokio::test]
    async fn edited_control_policy_is_replaced_with_its_sandbox_owner() {
        let mut object = sandbox(false);
        object["metadata"]["annotations"][ANNOTATION_SANDBOX_RUNTIME_CONTROL_POLICY_GENERATION] = serde_json::json!("1");
        let world = guarded_world(object);
        let names = SandboxRuntimeNames::new("sandbox-1");
        let mut stored = control_policy(NS, &names, "sandbox-1", 5500, OwnerReference::default());
        stored.metadata.owner_references = None;
        stored.metadata.uid = Some("control-uid".into());
        stored.metadata.generation = Some(1);
        stored.metadata.resource_version = Some("300".into());
        reads(&world, CONTROL_PATH, &widened(&stored), &repaired(&stored, 3));
        world.on(http::Method::PUT, CONTROL_PATH, 200, serde_json::to_value(repaired(&stored, 3)).unwrap());
        let outcome = world
            .driver(KubernetesComputeConfig::default())
            .guard_policy_key(&PolicyKey::new(NS, "os-control-sandbox-1"))
            .await;
        assert_eq!(write_of(&outcome).1, RepairWrite::Replaced);
        let put = &world.bodies(&http::Method::PUT, CONTROL_PATH)[0];
        assert_eq!(put["metadata"]["ownerReferences"][0]["uid"], "cr-uid");
        assert_eq!(put["metadata"]["ownerReferences"][0]["kind"], "Sandbox");
    }

    #[tokio::test]
    async fn conflict_on_the_label_repair_requeues_and_the_reread_repairs_the_widening() {
        // Strip, then widen between the guard's GET and its label repair.
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut stripped = fence.clone();
        stripped.metadata.labels = None;
        let mut stripped_and_widened = widened(&fence);
        stripped_and_widened.metadata.labels = None;
        world.forget(http::Method::GET, WORKLOAD_FENCE_PATH);
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&stripped).unwrap())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&stripped_and_widened).unwrap())
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 409, conflict_status())
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let first = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&first), (Drift::Metadata, RepairWrite::Raced));
        assert!(first.needs_requeue());
        assert!(reconciled(&first).is_empty());
        let second = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&second), (Drift::Spec, RepairWrite::Replaced));
        assert_eq!(reconciled(&second), [suspended_by_fence_generation()]);
        let puts = world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH);
        assert_eq!(puts[0]["metadata"]["resourceVersion"], "100");
        assert_eq!(puts[1]["metadata"]["resourceVersion"], "101");
        assert_eq!(puts[1]["spec"]["egress"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn terminating_namespace_is_left_alone() {
        let world = guarded_world(sandbox(true));
        world.forget(http::Method::GET, NAMESPACE_PATH);
        world.on(http::Method::GET, NAMESPACE_PATH, 200, serde_json::json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": {"name": NS, "deletionTimestamp": "2026-01-01T00:00:00Z"}
        }));
        let outcome = world.driver(KubernetesComputeConfig::default()).guard_policy_key(&workload_key()).await;
        assert_eq!(outcome, GuardOutcome::Skipped("namespace-gone"));
        assert_eq!(world.requests(), [format!("GET {NAMESPACE_PATH}")]);
    }

    #[tokio::test]
    async fn namespace_outside_the_driver_scope_gets_no_requests() {
        let world = guarded_world(sandbox(true));
        let outcome = world
            .driver(KubernetesComputeConfig::default())
            .guard_policy_key(&PolicyKey::new("someone-else", "openshell-sandbox-workloads"))
            .await;
        assert_eq!(outcome, GuardOutcome::Skipped("namespace-not-accepted"));
        assert!(world.requests().is_empty());
    }

    #[tokio::test]
    async fn busy_lifecycle_gate_requeues_a_per_sandbox_repair() {
        let world = guarded_world(sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let _held = driver.lifecycle_gates.gate_for("sandbox-1").lock_owned().await;
        let outcome = driver.guard_policy_key(&PolicyKey::new(NS, "os-control-sandbox-1")).await;
        assert_eq!(outcome, GuardOutcome::Skipped("lifecycle-busy"));
        assert!(outcome.needs_requeue());
        assert!(!world.requests().iter().any(|r| r.contains("networkpolicies")));
    }

    #[tokio::test]
    async fn repair_budget_stops_a_fight_with_another_controller() {
        let world = guarded_world(sandbox(false));
        let (fence, _) = stored_fences();
        world.forget(http::Method::GET, WORKLOAD_FENCE_PATH);
        // Another controller re-applies the widening after every repair.
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(widened(&fence)).unwrap())
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        for _ in 0..REPAIR_BUDGET {
            let outcome = driver.guard_policy_key(&workload_key()).await;
            assert_eq!(write_of(&outcome).1, RepairWrite::Replaced);
        }
        let outcome = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(write_of(&outcome).1, RepairWrite::BudgetExhausted);
        assert_eq!(world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH).len(), REPAIR_BUDGET);
    }

    /// Feed watch events through the guard exactly as the worker does.
    async fn drive(
        driver: &KubernetesComputeDriver,
        events: Vec<Event<NetworkPolicy>>,
    ) -> Vec<(PolicyKey, GuardOutcome)> {
        use crate::driver::netpol_guard_watch::{GuardMessage, messages_for_event};
        let mut outcomes = Vec::new();
        for event in events {
            for message in messages_for_event(NS, &event) {
                let keys = match message {
                    GuardMessage::Key(key) => vec![key],
                    GuardMessage::Namespace(namespace) => driver.expected_guard_keys(&namespace).await,
                };
                for key in keys {
                    let outcome = driver.guard_policy_key(&key).await;
                    outcomes.push((key, outcome));
                }
            }
        }
        outcomes
    }

    fn for_workload_fence(outcomes: &[(PolicyKey, GuardOutcome)]) -> Vec<GuardOutcome> {
        outcomes
            .iter()
            .filter(|(key, _)| *key == workload_key())
            .map(|(_, outcome)| outcome.clone())
            .collect()
    }

    #[tokio::test]
    async fn strip_then_widen_before_the_get_is_one_full_repair() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut stripped = fence.clone();
        stripped.metadata.labels = None;
        let mut stripped_and_widened = widened(&fence);
        stripped_and_widened.metadata.labels = None;
        // The strip leaves the selection: the watch reports DELETED. The
        // widening lands before the queued GET-by-name runs.
        reads(&world, WORKLOAD_FENCE_PATH, &stripped_and_widened, &repaired(&fence, 3));
        world
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let outcomes = drive(&driver, vec![Event::Delete(stripped)]).await;
        let outcomes = for_workload_fence(&outcomes);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(write_of(&outcomes[0]), (Drift::Spec, RepairWrite::Replaced));
        assert_eq!(reconciled(&outcomes[0]), [suspended_by_fence_generation()]);
        let put = &world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH)[0];
        assert_eq!(put["metadata"]["labels"][LABEL_MANAGED_BY], LABEL_MANAGED_BY_VALUE);
        assert_eq!(put["spec"]["egress"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn strip_then_widen_after_the_label_repair_is_seen_through_the_selector() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut stripped = fence.clone();
        stripped.metadata.labels = None;
        let relabelled = repaired(&fence, 1);
        // The GET sees only the strip; the label repair succeeds.
        reads(&world, WORKLOAD_FENCE_PATH, &stripped, &relabelled);
        world.on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(&relabelled).unwrap());
        world_running_pods(&world);
        let driver = world.driver(KubernetesComputeConfig::default());
        let first = for_workload_fence(&drive(&driver, vec![Event::Delete(stripped)]).await);
        assert_eq!(write_of(&first[0]), (Drift::Metadata, RepairWrite::Replaced));
        assert_eq!(reconciled(&first[0]), [ReconcileOutcome::Reconciled]);

        // Then the spec is widened. The object carries its labels again, so
        // the watch delivers MODIFIED.
        let mut widened_after = widened(&relabelled);
        widened_after.metadata.resource_version = Some("201".into());
        reads(&world, WORKLOAD_FENCE_PATH, &widened_after, &repaired(&fence, 3));
        world.forget(http::Method::PUT, WORKLOAD_FENCE_PATH);
        world
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let second = for_workload_fence(&drive(&driver, vec![Event::Apply(widened_after)]).await);
        assert_eq!(write_of(&second[0]), (Drift::Spec, RepairWrite::Replaced));
        assert_eq!(reconciled(&second[0]), [suspended_by_fence_generation()]);
        let puts = world.bodies(&http::Method::PUT, WORKLOAD_FENCE_PATH);
        assert_eq!(puts[1]["metadata"]["resourceVersion"], "201");
        assert_eq!(puts[1]["spec"]["egress"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_relist_with_no_event_still_repairs_every_expected_name() {
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        let mut stripped_and_widened = widened(&fence);
        stripped_and_widened.metadata.labels = None;
        // The change happened during a watch gap; after 410 Gone the relist
        // never selects the stripped object, so only InitDone arrives.
        reads(&world, WORKLOAD_FENCE_PATH, &stripped_and_widened, &repaired(&fence, 3));
        world
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let outcomes = drive(&driver, vec![Event::Init, Event::InitDone]).await;
        let keys: Vec<&str> = outcomes.iter().map(|(key, _)| key.name.as_str()).collect();
        assert_eq!(
            keys,
            [
                "openshell-sandbox-workloads",
                "openshell-sandbox-supervisors",
                "os-control-sandbox-1",
            ]
        );
        let fence_outcomes = for_workload_fence(&outcomes);
        assert_eq!(write_of(&fence_outcomes[0]), (Drift::Spec, RepairWrite::Replaced));
        assert_eq!(reconciled(&fence_outcomes[0]), [suspended_by_fence_generation()]);
        // An absent per-sandbox policy the sandbox never bound is left alone.
        for (key, outcome) in &outcomes[2..] {
            assert!(reconciled(outcome).is_empty(), "{key:?} {outcome:?}");
        }
    }

    fn reasons(events: &[WatchSandboxesEvent]) -> Vec<(String, String, String)> {
        events
            .iter()
            .map(|event| {
                let Some(watch_sandboxes_event::Payload::PlatformEvent(platform)) = &event.payload
                else {
                    panic!("not a platform event: {event:?}");
                };
                let inner = platform.event.as_ref().unwrap();
                assert_eq!(inner.source, netpol_guard_shell::PLATFORM_EVENT_SOURCE);
                assert_ne!(inner.source, "kubernetes");
                (platform.sandbox_id.clone(), inner.reason.clone(), inner.metadata["policy"].clone())
            })
            .collect()
    }

    #[tokio::test]
    async fn a_successful_repair_signals_once_per_sandbox_and_a_conflict_signals_nothing() {
        use crate::driver::netpol_guard_shell::guard_signals;
        let world = guarded_world(sandbox(true));
        let (fence, _) = stored_fences();
        reads(&world, WORKLOAD_FENCE_PATH, &widened(&fence), &repaired(&fence, 3));
        world
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 409, conflict_status())
            .on(http::Method::PUT, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(repaired(&fence, 3)).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let driver = world.driver(KubernetesComputeConfig::default());
        let raced = driver.guard_policy_key(&workload_key()).await;
        assert!(guard_signals(&workload_key(), &raced).is_empty());
        reads(&world, WORKLOAD_FENCE_PATH, &widened(&fence), &repaired(&fence, 3));
        let repaired_outcome = driver.guard_policy_key(&workload_key()).await;
        assert_eq!(
            reasons(&guard_signals(&workload_key(), &repaired_outcome)),
            [
                ("sandbox-1".into(), "NetworkPolicyRestored".into(), "openshell-sandbox-workloads".into()),
                ("sandbox-1".into(), "NetworkPolicyDriftSuspended".into(), "openshell-sandbox-workloads".into()),
            ]
        );
    }

    #[test]
    fn signals_fan_out_per_sandbox_and_only_for_written_suspensions() {
        use crate::driver::netpol_guard_shell::{periodic_pass_signals, restore_signals};
        let restored = restore_signals(
            NS,
            "openshell-sandbox-supervisors",
            Drift::Missing,
            &RepairWrite::Created,
            ["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(reasons(&restored).len(), 3);
        assert!(restore_signals(NS, "x", Drift::Spec, &RepairWrite::Raced, ["a".to_string()]).is_empty());
        let pass = |write| ReconciledSandbox {
            sandbox_id: "sandbox-1".into(),
            namespace: NS.into(),
            outcome: ReconcileOutcome::Suspended {
                trigger: SuspendTrigger::ControlPolicy { shared: false },
                write,
            },
        };
        assert_eq!(
            reasons(&periodic_pass_signals(&[pass(SuspendWrite::Written)], Vec::new())),
            [("sandbox-1".into(), "NetworkPolicyDriftSuspended".into(), "os-control-sandbox-1".into())]
        );
        assert!(periodic_pass_signals(&[pass(SuspendWrite::Raced)], Vec::new()).is_empty());
        // Suspensions that aren't NetworkPolicy drift don't signal drift.
        let unrelated = ReconciledSandbox {
            outcome: ReconcileOutcome::Suspended {
                trigger: SuspendTrigger::SupervisorUnavailable,
                write: SuspendWrite::Written,
            },
            ..pass(SuspendWrite::Written)
        };
        assert!(periodic_pass_signals(&[unrelated], Vec::new()).is_empty());
        // A label restore by the pass signals each sandbox in its namespace.
        let healthy = ReconciledSandbox {
            outcome: ReconcileOutcome::Reconciled,
            ..pass(SuspendWrite::Written)
        };
        assert_eq!(
            reasons(&periodic_pass_signals(&[healthy], vec![workload_key()])),
            [("sandbox-1".into(), "NetworkPolicyRestored".into(), "openshell-sandbox-workloads".into())]
        );
    }

    #[tokio::test]
    async fn no_sandboxes_means_no_repairs() {
        let world = guarded_world(sandbox(true));
        world.forget(http::Method::GET, LIST_PATH);
        world.on(http::Method::GET, LIST_PATH, 200, sandbox_list(Vec::new()));
        let outcome = world.driver(KubernetesComputeConfig::default()).guard_policy_key(&workload_key()).await;
        assert_eq!(outcome, GuardOutcome::Skipped("no-sandboxes"));
    }
}

/// Managed-mode guard `RoleBinding`s against the fake API world.
mod netpol_guard_bindings {
    use super::reconcile_request_sequences::World;
    use super::*;
    use crate::driver::netpol_guard_rbac::{GUARD_ROLE_BINDING_NAME, GuardBinding};

    const NS: &str = "openshell-gw-team-a";
    const PATH: &str =
        "/apis/rbac.authorization.k8s.io/v1/namespaces/openshell-gw-team-a/rolebindings/openshell-netpol-guard";
    const COLLECTION: &str =
        "/apis/rbac.authorization.k8s.io/v1/namespaces/openshell-gw-team-a/rolebindings";

    fn binding() -> GuardBinding {
        GuardBinding {
            cluster_role: "openshell-netpol-guard-openshell".into(),
            service_account: "openshell".into(),
            service_account_namespace: "openshell".into(),
        }
    }

    fn driver(world: &World) -> KubernetesComputeDriver {
        let mut driver = world.driver(KubernetesComputeConfig {
            workspace_mode: WorkspaceMode::Managed,
            gateway_id: "gw".into(),
            ..Default::default()
        });
        driver.netpol_guard_binding = Some(binding());
        driver
    }

    #[tokio::test]
    async fn a_new_managed_namespace_gets_the_guard_binding() {
        let world = World::default();
        world.on(
            http::Method::POST,
            COLLECTION,
            201,
            serde_json::to_value(binding().role_binding(NS)).unwrap(),
        );
        assert!(driver(&world).ensure_netpol_guard_role_binding(NS).await);
        let body = &world.bodies(&http::Method::POST, COLLECTION)[0];
        assert_eq!(body["metadata"]["name"], GUARD_ROLE_BINDING_NAME);
        assert_eq!(body["roleRef"]["kind"], "ClusterRole");
        assert_eq!(body["roleRef"]["name"], "openshell-netpol-guard-openshell");
        assert_eq!(body["subjects"][0]["kind"], "ServiceAccount");
        assert_eq!(body["subjects"][0]["namespace"], "openshell");
    }

    #[tokio::test]
    async fn an_existing_binding_is_adopted_and_a_foreign_one_is_left_alone() {
        let world = World::default();
        world.on(http::Method::GET, PATH, 200, serde_json::to_value(binding().role_binding(NS)).unwrap());
        assert!(driver(&world).ensure_netpol_guard_role_binding(NS).await);
        let mut foreign = binding().role_binding(NS);
        foreign.role_ref.name = "cluster-admin".into();
        let world = World::default();
        world.on(http::Method::GET, PATH, 200, serde_json::to_value(foreign).unwrap());
        assert!(!driver(&world).ensure_netpol_guard_role_binding(NS).await);
        assert!(world.requests().iter().all(|r| r.starts_with("GET")));
    }

    #[tokio::test]
    async fn without_chart_rbac_no_binding_is_attempted() {
        let world = World::default();
        let mut driver = driver(&world);
        driver.netpol_guard_binding = None;
        assert!(!driver.ensure_netpol_guard_role_binding(NS).await);
        assert!(world.requests().is_empty());
    }
}
