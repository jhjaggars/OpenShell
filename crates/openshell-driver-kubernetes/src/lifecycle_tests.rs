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
        let driver = KubernetesComputeDriver {
            client: client.clone(),
            watch_client: client,
            sandbox_api_version: Arc::new(OnceCell::new()),
            lifecycle_gates: Arc::default(),
            config: KubernetesComputeConfig::default(),
            network_trust_bundle: None,
            operator_allowlist: None,
            shared_supervisor: None,
        };
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

    fn running_pods(world: &World) {
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
        outcomes[0].1
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
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceSpec));
        assert_eq!(
            world.requests(),
            [get(LIST_PATH), get(CR_PATH), get(WORKLOAD_FENCE_PATH), patch(CR_PATH)]
        );
        assert!(suspended(&world));
    }

    #[tokio::test]
    async fn label_stripped_workload_fence_suspends_today() {
        let world = world_with(sandbox(true));
        let (mut workload, _) = stored_fences();
        workload.metadata.labels = None;
        world.routes.lock().unwrap().remove(&(http::Method::GET, WORKLOAD_FENCE_PATH.to_string()));
        world
            .on(http::Method::GET, WORKLOAD_FENCE_PATH, 200, serde_json::to_value(workload).unwrap())
            .on(http::Method::PATCH, CR_PATH, 200, sandbox(false));
        let outcome = reconcile(&world).await;
        assert_eq!(outcome, suspended_by(SuspendTrigger::FenceSpec));
        assert_eq!(
            world.requests(),
            [get(LIST_PATH), get(CR_PATH), get(WORKLOAD_FENCE_PATH), patch(CR_PATH)]
        );
        assert!(suspended(&world));
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
        running_pods(&world);
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
        assert_eq!(outcome, suspended_by(SuspendTrigger::ControlPath));
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
        assert_eq!(outcomes[0].1, ReconcileOutcome::Skipped(SkipReason::GateBusy));
        assert_eq!(world.requests(), [get(LIST_PATH)]);
    }
}
