// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "e2e-kubernetes")]

//! The Kubernetes driver restores the `NetworkPolicy` objects it owns.
//!
//! Each case deletes, edits, replaces or strips the labels of one
//! driver-owned policy and measures how long the gateway takes to restore
//! it. The guard is a watch, so the restore must land well inside the
//! 30-second periodic pass: the test fails above 15 seconds and warns above
//! 5. A sandbox bound to a policy whose spec drifted must be suspended with
//! reason `NetworkPolicyDrift`, and `sandbox start` must then work.
//!
//! These cases mutate namespace-wide policies, which suspends every sandbox
//! in the namespace, so they run only when `OPENSHELL_E2E_NETPOL_GUARD`
//! selects them: `isolation` (the default Helm lane), `shared-supervisor`,
//! `managed` (with `values-workspace-managed.yaml`), `operator` (with
//! `values-workspace-operator.yaml`) or `ha` (two gateway replicas). Use the `e2e:kubernetes:netpol-guard*` mise tasks.

use std::process::Stdio;
use std::time::{Duration, Instant};

use openshell_e2e::harness::cli::{run_cli, wait_for_sandbox_phase};
use openshell_e2e::harness::output::strip_ansi;
use openshell_e2e::harness::sandbox::SandboxGuard;

const WORKLOADS: &str = "openshell-sandbox-workloads";
const SUPERVISORS: &str = "openshell-sandbox-supervisors";
const SSH: &str = "openshell-sandbox-ssh";
const MANAGED_BY: &str = "openshell.ai/managed-by";
const COMPONENT: &str = "openshell.ai/component";
const TARGET: Duration = Duration::from_secs(5);
const LIMIT: Duration = Duration::from_secs(15);

fn lane() -> Option<String> {
    std::env::var("OPENSHELL_E2E_NETPOL_GUARD")
        .ok()
        .filter(|lane| !lane.is_empty())
}

/// Whether this case belongs to the selected lane; prints why when not.
fn selected(case_lane: &str) -> bool {
    let selected = lane().as_deref() == Some(case_lane);
    if !selected {
        eprintln!(
            "skipping: set OPENSHELL_E2E_NETPOL_GUARD={case_lane} to run NetworkPolicy guard cases that suspend every sandbox in the namespace"
        );
    }
    selected
}

fn namespace() -> String {
    std::env::var("OPENSHELL_E2E_SANDBOX_NAMESPACE").unwrap_or_else(|_| "openshell".to_string())
}

async fn kubectl_raw(args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    use tokio::io::AsyncWriteExt as _;
    let mut cmd = tokio::process::Command::new("kubectl");
    if let Ok(context) = std::env::var("OPENSHELL_E2E_KUBE_CONTEXT_ACTIVE")
        && !context.trim().is_empty()
    {
        cmd.arg("--context").arg(context);
    }
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|error| panic!("spawn kubectl {args:?}: {error}"));
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("kubectl stdin");
        pipe.write_all(input.as_bytes())
            .await
            .expect("write kubectl stdin");
        drop(pipe);
    }
    let output = child.wait_with_output().await.expect("wait for kubectl");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn kubectl(args: &[&str]) -> String {
    let (ok, stdout, stderr) = kubectl_raw(args, None).await;
    assert!(ok, "kubectl {args:?} failed: {stderr}");
    stdout
}

async fn kubectl_stdin(args: &[&str], input: &str) -> String {
    let (ok, stdout, stderr) = kubectl_raw(args, Some(input)).await;
    assert!(ok, "kubectl {args:?} failed: {stderr}");
    stdout
}

async fn policy(ns: &str, name: &str) -> Option<serde_json::Value> {
    let (ok, stdout, stderr) = kubectl_raw(
        &["-n", ns, "get", "networkpolicy", name, "-o", "json"],
        None,
    )
    .await;
    if ok {
        Some(serde_json::from_str(&stdout).expect("parse NetworkPolicy"))
    } else {
        assert!(
            stderr.contains("NotFound"),
            "get networkpolicy {name}: {stderr}"
        );
        None
    }
}

/// What the driver must restore: the spec and the labels it requires.
#[derive(Clone)]
struct Baseline {
    spec: serde_json::Value,
    labels: serde_json::Map<String, serde_json::Value>,
    uid: String,
}

async fn baseline(ns: &str, name: &str) -> Baseline {
    let current = policy(ns, name)
        .await
        .unwrap_or_else(|| panic!("driver-owned NetworkPolicy {ns}/{name} is missing"));
    let labels = current["metadata"]["labels"]
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, _)| {
            key == MANAGED_BY || key == COMPONENT || key == "openshell.ai/sandbox-id"
        })
        .collect();
    Baseline {
        spec: current["spec"].clone(),
        labels,
        uid: current["metadata"]["uid"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    }
}

fn restored(current: &serde_json::Value, expected: &Baseline) -> bool {
    current["spec"] == expected.spec
        && expected
            .labels
            .iter()
            .all(|(key, value)| current["metadata"]["labels"][key] == *value)
}

/// Wait for the policy to match `expected` again and return how long it took
/// from `since`, the moment the mutation finished.
async fn wait_restored(
    ns: &str,
    name: &str,
    expected: &Baseline,
    since: Instant,
    case: &str,
) -> Duration {
    loop {
        if let Some(current) = policy(ns, name).await
            && restored(&current, expected)
        {
            let elapsed = since.elapsed();
            println!(
                "NETPOL_REPAIR case={case} policy={name} repair_ms={}",
                elapsed.as_millis()
            );
            if elapsed > TARGET {
                eprintln!("warning: {case}: {name} took {elapsed:?}, over the {TARGET:?} target");
            }
            return elapsed;
        }
        assert!(
            since.elapsed() < LIMIT,
            "{case}: {ns}/{name} was not restored within {LIMIT:?}; the watch-driven guard did not repair it"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mutation {
    Delete,
    Edit,
    Replace,
    StripLabels,
}

impl Mutation {
    const ALL: [Self; 4] = [Self::Delete, Self::Edit, Self::Replace, Self::StripLabels];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Edit => "edit",
            Self::Replace => "replace",
            Self::StripLabels => "strip-labels",
        }
    }
}

/// A spec change that widens what the policy allows.
fn widening(name: &str) -> serde_json::Value {
    if name == SUPERVISORS {
        // Selecting every Pod gives workloads the supervisors' egress.
        serde_json::json!({"spec": {"podSelector": {}}})
    } else if name == SSH {
        serde_json::json!({"spec": {"ingress": [{}]}})
    } else {
        // Allow all egress from the selected workloads.
        serde_json::json!({"spec": {"egress": [{}], "policyTypes": ["Ingress", "Egress"]}})
    }
}

/// Apply `mutation` to `ns/name` and return when it finished.
async fn mutate(ns: &str, name: &str, mutation: Mutation) -> Instant {
    match mutation {
        Mutation::Delete => {
            kubectl(&["-n", ns, "delete", "networkpolicy", name, "--wait=false"]).await;
        }
        Mutation::Edit => {
            // A JSON patch, because a merge patch can't empty an object such
            // as `podSelector`.
            let patch: Vec<serde_json::Value> = widening(name)["spec"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    serde_json::json!({"op": "replace", "path": format!("/spec/{key}"), "value": value})
                })
                .collect();
            let patch = serde_json::Value::from(patch).to_string();
            kubectl(&[
                "-n",
                ns,
                "patch",
                "networkpolicy",
                name,
                "--type",
                "json",
                "-p",
                &patch,
            ])
            .await;
        }
        Mutation::Replace => {
            // Delete and re-create permissive and without the driver's labels.
            let mut object = policy(ns, name).await.expect("policy to replace");
            let metadata = serde_json::json!({"name": name, "namespace": ns});
            object["metadata"] = metadata;
            let widened = widening(name);
            for (key, value) in widened["spec"].as_object().unwrap() {
                object["spec"][key] = value.clone();
            }
            kubectl_stdin(&["replace", "--force", "-f", "-"], &object.to_string()).await;
        }
        Mutation::StripLabels => {
            let mut args = vec![
                "-n",
                ns,
                "label",
                "networkpolicy",
                name,
                "openshell.ai/managed-by-",
            ];
            let current = policy(ns, name).await.expect("policy to relabel");
            if current["metadata"]["labels"].get(COMPONENT).is_some() {
                args.push("openshell.ai/component-");
            }
            kubectl(&args).await;
        }
    }
    Instant::now()
}

async fn sandbox_json(name: &str, workspace: Option<&str>) -> serde_json::Value {
    let mut args = vec!["sandbox", "get", name, "--output", "json"];
    if let Some(workspace) = workspace {
        args.extend(["--workspace", workspace]);
    }
    let (output, code) = run_cli(&args).await;
    assert_eq!(code, 0, "sandbox get {name}: {output}");
    serde_json::from_str(&strip_ansi(&output)).unwrap_or_else(|error| panic!("{error}: {output}"))
}

async fn sandbox_id(name: &str, workspace: Option<&str>) -> String {
    sandbox_json(name, workspace).await["id"]
        .as_str()
        .expect("sandbox id")
        .to_ascii_lowercase()
}

/// Wait until the sandbox is stopped with reason `NetworkPolicyDrift` naming
/// `policy`, and return how long it took from `since`.
async fn wait_drift_suspended(
    name: &str,
    workspace: Option<&str>,
    policy: &str,
    since: Instant,
    case: &str,
) -> Duration {
    let deadline = Instant::now() + Duration::from_mins(2);
    let mut saw_reason = None;
    loop {
        let sandbox = sandbox_json(name, workspace).await;
        if let Some(conditions) = sandbox["conditions"].as_array() {
            for condition in conditions {
                if condition["type"] == "Ready" && condition["reason"] == "NetworkPolicyDrift" {
                    let message = condition["message"].as_str().unwrap_or_default();
                    assert!(
                        message.contains(policy),
                        "{case}: drift message {message:?} must name {policy}"
                    );
                    saw_reason.get_or_insert_with(|| since.elapsed());
                }
            }
        }
        assert_ne!(
            sandbox["phase"], "Error",
            "{case}: drift must never show as Error: {sandbox}"
        );
        if sandbox["phase"] == "Stopped"
            && let Some(reason_at) = saw_reason
        {
            println!(
                "NETPOL_SUSPEND case={case} policy={policy} reason_ms={} stopped_ms={}",
                reason_at.as_millis(),
                since.elapsed().as_millis()
            );
            return since.elapsed();
        }
        assert!(
            Instant::now() < deadline,
            "{case}: sandbox {name} was not stopped with reason NetworkPolicyDrift: {sandbox}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Assert the sandbox stays running and is never marked with drift.
async fn assert_not_suspended(name: &str, workspace: Option<&str>, case: &str) {
    // Longer than the periodic pass, which must not suspend either.
    let until = Instant::now() + Duration::from_secs(35);
    while Instant::now() < until {
        let sandbox = sandbox_json(name, workspace).await;
        assert_eq!(
            sandbox["phase"], "Ready",
            "{case}: label-only drift must not suspend: {sandbox}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("NETPOL_NO_SUSPEND case={case} observed_s=35");
}

async fn start_and_exec(sandbox: &SandboxGuard, workspace: Option<&str>, case: &str) {
    let mut args = vec!["sandbox", "start", sandbox.name.as_str()];
    if let Some(workspace) = workspace {
        args.extend(["--workspace", workspace]);
    }
    let started = Instant::now();
    let (output, code) = run_cli(&args).await;
    assert_eq!(
        code, 0,
        "{case}: sandbox start after repair failed: {output}"
    );
    wait_for_sandbox_phase(&sandbox.name, "Ready", Duration::from_mins(3))
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));
    let marker = format!("netpol-{case}");
    let out = sandbox
        .exec(&["printf", &marker])
        .await
        .unwrap_or_else(|error| panic!("{case}: exec after start: {error}"));
    assert!(out.contains(&marker), "{case}: exec output {out:?}");
    println!(
        "NETPOL_START case={case} start_to_exec_ms={}",
        started.elapsed().as_millis()
    );
}

async fn workload_pod(ns: &str, sandbox_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let pods: serde_json::Value = serde_json::from_str(
            &kubectl(&[
                "-n",
                ns,
                "get",
                "pods",
                "-l",
                &format!(
                    "openshell.ai/boundary-pair={sandbox_id},openshell.ai/boundary-role=workload"
                ),
                "-o",
                "json",
            ])
            .await,
        )
        .expect("parse pods");
        if let Some(pod) = pods["items"].as_array().and_then(|items| {
            items.iter().find(|pod| {
                pod["status"]["phase"] == "Running"
                    && pod["metadata"]["deletionTimestamp"].is_null()
            })
        }) {
            return pod["metadata"]["name"].as_str().unwrap().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no running workload Pod for {sandbox_id}: {pods}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A raw TCP connect from inside a Pod's network namespace, bypassing the
/// sandbox proxy. Returns whether it connected.
async fn raw_connect(ns: &str, pod: &str, container: Option<&str>, host: &str, port: u16) -> bool {
    let probe = format!(
        "if command -v bash >/dev/null 2>&1; then timeout 4 bash -c 'exec 3<>/dev/tcp/{host}/{port}' && echo NETPOL_OPEN; \
         elif command -v nc >/dev/null 2>&1; then nc -z -w 4 {host} {port} && echo NETPOL_OPEN; \
         else echo NETPOL_NO_PROBE; fi"
    );
    let mut args = vec!["-n", ns, "exec", pod];
    if let Some(container) = container {
        args.extend(["-c", container]);
    }
    args.extend(["--", "sh", "-c", &probe]);
    let (_, stdout, stderr) = kubectl_raw(&args, None).await;
    assert!(
        !stdout.contains("NETPOL_NO_PROBE"),
        "no bash or nc in {pod} to probe egress: {stderr}"
    );
    stdout.contains("NETPOL_OPEN")
}

/// The Kubernetes API server's `ClusterIP`: reachable from any unfenced Pod.
async fn api_server_ip() -> String {
    kubectl(&[
        "-n",
        "default",
        "get",
        "service",
        "kubernetes",
        "-o",
        "jsonpath={.spec.clusterIP}",
    ])
    .await
}

/// A public address that answers on 443, used by the conformance suite too.
const EXTERNAL_HOST: &str = "1.1.1.1";

/// Prove the workload Pod is fenced: a raw connect to an external address and
/// to the API server fails from it, while both succeed from an unfenced probe
/// Pod in the same namespace.
async fn assert_workload_egress_blocked(ns: &str, sandbox_id: &str, case: &str) {
    let api = api_server_ip().await;
    let targets = [EXTERNAL_HOST, api.as_str()];
    let probe = format!("netpol-probe-{}", std::process::id());
    kubectl(&[
        "-n",
        ns,
        "run",
        &probe,
        "--image=busybox:1.36",
        "--restart=Never",
        "--command",
        "--",
        "sleep",
        "300",
    ])
    .await;
    kubectl(&[
        "-n",
        ns,
        "wait",
        "--for=condition=Ready",
        &format!("pod/{probe}"),
        "--timeout=120s",
    ])
    .await;
    let mut controls = Vec::new();
    for target in targets {
        controls.push((target, raw_connect(ns, &probe, None, target, 443).await));
    }
    kubectl(&["-n", ns, "delete", "pod", &probe, "--wait=false"]).await;
    for (target, open) in controls {
        assert!(
            open,
            "{case}: the unfenced probe Pod could not reach {target}:443, so the egress check proves nothing"
        );
    }
    let pod = workload_pod(ns, sandbox_id).await;
    for target in targets {
        let open = raw_connect(ns, &pod, Some("agent"), target, 443).await;
        assert!(
            !open,
            "{case}: workload Pod {pod} reached {target}:443 directly after the repair"
        );
        println!("NETPOL_EGRESS_BLOCKED case={case} pod={pod} target={target}:443");
    }
}

/// Run every mutation against one policy and check the outcome.
async fn mutation_matrix(
    sandbox: &SandboxGuard,
    ns: &str,
    name: &str,
    suspends: impl Fn(Mutation) -> bool,
) {
    for mutation in Mutation::ALL {
        let case = format!("{name}/{}", mutation.as_str());
        let expected = baseline(ns, name).await;
        let since = mutate(ns, name, mutation).await;
        wait_restored(ns, name, &expected, since, &case).await;
        if suspends(mutation) {
            wait_drift_suspended(&sandbox.name, None, name, since, &case).await;
            start_and_exec(sandbox, None, &case.replace('/', "-")).await;
        } else {
            assert_not_suspended(&sandbox.name, None, &case).await;
        }
        let after = policy(ns, name).await.expect("restored policy");
        if mutation == Mutation::StripLabels {
            assert_eq!(
                after["metadata"]["uid"], expected.uid,
                "{case}: a label repair keeps the object"
            );
        }
    }
}

async fn create_sandbox() -> SandboxGuard {
    SandboxGuard::create_keep(
        &["sh", "-c", "echo netpol-ready; exec sleep infinity"],
        "netpol-ready",
    )
    .await
    .expect("create sandbox")
}

#[tokio::test]
async fn workload_fence_is_restored_after_every_mutation() {
    if !selected("isolation") {
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    // Every change but a label strip touches the deny fence's spec or UID.
    mutation_matrix(&sandbox, &ns, WORKLOADS, |mutation| {
        mutation != Mutation::StripLabels
    })
    .await;
    let id = sandbox_id(&sandbox.name, None).await;
    assert_workload_egress_blocked(&ns, &id, "workloads-after-matrix").await;
    sandbox.cleanup().await;
}

#[tokio::test]
async fn supervisor_egress_is_restored_after_every_mutation() {
    if !selected("isolation") {
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    // Deleting the allowance only narrows egress; spec changes may widen it.
    mutation_matrix(&sandbox, &ns, SUPERVISORS, |mutation| {
        matches!(mutation, Mutation::Edit | Mutation::Replace)
    })
    .await;
    sandbox.cleanup().await;
}

#[tokio::test]
async fn control_policy_is_repaired_and_never_resurrected() {
    if !selected("isolation") {
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    let id = sandbox_id(&sandbox.name, None).await;
    let name = format!("os-control-{id}");
    if policy(&ns, &name).await.is_none() {
        eprintln!("skipping: {name} absent; this lane uses supervisor-initiated control");
        sandbox.cleanup().await;
        return;
    }
    for mutation in [Mutation::Edit, Mutation::Replace, Mutation::StripLabels] {
        let case = format!("os-control/{}", mutation.as_str());
        let expected = baseline(&ns, &name).await;
        let since = mutate(&ns, &name, mutation).await;
        wait_restored(&ns, &name, &expected, since, &case).await;
        if mutation == Mutation::StripLabels {
            assert_not_suspended(&sandbox.name, None, &case).await;
        } else {
            wait_drift_suspended(&sandbox.name, None, &name, since, &case).await;
            start_and_exec(&sandbox, None, &case.replace('/', "-")).await;
        }
    }
    // Delete: never re-created by the guard; the sandbox is suspended and
    // `start` re-creates it.
    let since = mutate(&ns, &name, Mutation::Delete).await;
    wait_drift_suspended(&sandbox.name, None, &name, since, "os-control/delete").await;
    assert!(
        policy(&ns, &name).await.is_none(),
        "the guard must not re-create {name}"
    );
    start_and_exec(&sandbox, None, "os-control-delete").await;
    assert!(
        policy(&ns, &name).await.is_some(),
        "start re-creates {name}"
    );

    // No resurrection: delete the policy, then the sandbox.
    mutate(&ns, &name, Mutation::Delete).await;
    sandbox.delete().await.expect("delete sandbox");
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert!(
        policy(&ns, &name).await.is_none(),
        "{name} came back after its sandbox was deleted"
    );
    println!("NETPOL_NO_RESURRECTION policy={name} observed_s=40");
}

#[tokio::test]
async fn strip_then_widen_is_repaired_and_fails_closed() {
    if !selected("isolation") {
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    let id = sandbox_id(&sandbox.name, None).await;
    let expected = baseline(&ns, WORKLOADS).await;
    mutate(&ns, WORKLOADS, Mutation::StripLabels).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let since = mutate(&ns, WORKLOADS, Mutation::Edit).await;
    wait_restored(&ns, WORKLOADS, &expected, since, "strip-then-widen").await;
    wait_drift_suspended(&sandbox.name, None, WORKLOADS, since, "strip-then-widen").await;
    start_and_exec(&sandbox, None, "strip-then-widen").await;
    assert_workload_egress_blocked(&ns, &id, "strip-then-widen").await;
    sandbox.cleanup().await;
}

#[tokio::test]
async fn each_repair_is_signalled_once_across_gateway_replicas() {
    if !matches!(lane().as_deref(), Some("isolation" | "ha")) {
        eprintln!("skipping: set OPENSHELL_E2E_NETPOL_GUARD=isolation or ha");
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    let expected = baseline(&ns, SUPERVISORS).await;
    let since = mutate(&ns, SUPERVISORS, Mutation::Delete).await;
    wait_restored(&ns, SUPERVISORS, &expected, since, "signal-once").await;
    // Let every replica's watch and periodic pass see the repaired object.
    tokio::time::sleep(Duration::from_secs(35)).await;
    let gateway_ns =
        std::env::var("OPENSHELL_E2E_GATEWAY_NAMESPACE").unwrap_or_else(|_| "openshell".into());
    let pods: serde_json::Value =
        serde_json::from_str(&kubectl(&["-n", &gateway_ns, "get", "pods", "-o", "json"]).await)
            .unwrap();
    let needle = format!("NetworkPolicy restored {ns}/{SUPERVISORS}");
    let mut count = 0;
    let mut replicas = 0;
    for pod in pods["items"].as_array().unwrap() {
        let is_gateway = pod["spec"]["containers"]
            .as_array()
            .is_some_and(|containers| containers.iter().any(|c| c["name"] == "openshell-gateway"));
        if !is_gateway {
            continue;
        }
        replicas += 1;
        let name = pod["metadata"]["name"].as_str().unwrap();
        let logs = kubectl(&[
            "-n",
            &gateway_ns,
            "logs",
            name,
            "-c",
            "openshell-gateway",
            "--since=3m",
        ])
        .await;
        count += strip_ansi(&logs)
            .lines()
            .filter(|line| line.contains("OCSF CONFIG:RESTORED") && line.contains(&needle))
            .count();
    }
    println!("NETPOL_SIGNALS replicas={replicas} restored_events={count}");
    if lane().as_deref() == Some("ha") {
        assert!(replicas >= 2, "the ha lane must run two gateway replicas");
    }
    assert_eq!(
        count, 1,
        "one repair must produce exactly one audit record across {replicas} replicas"
    );
    sandbox.cleanup().await;
}

/// The harness's sandbox guard doesn't pass a workspace to its follow-up
/// commands, so managed cases drive the CLI directly.
async fn create_managed_sandbox(workspace: &str, name: &str) {
    let (output, code) = run_cli(&[
        "sandbox",
        "create",
        "--detach",
        "--workspace",
        workspace,
        "--name",
        name,
        "--",
        "sh",
        "-c",
        "exec sleep infinity",
    ])
    .await;
    assert_eq!(code, 0, "sandbox create: {output}");
    wait_managed_ready(workspace, name).await;
}

async fn wait_managed_ready(workspace: &str, name: &str) {
    let deadline = Instant::now() + Duration::from_mins(3);
    loop {
        let sandbox = sandbox_json(name, Some(workspace)).await;
        if sandbox["phase"] == "Ready" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "managed sandbox {name} not Ready: {sandbox}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Shared supervisor topology: the per-sandbox allowance to the shared
/// supervisor is replaced after drift and never re-created by the guard.
#[tokio::test]
async fn shared_control_policy_is_repaired_and_never_resurrected() {
    if !selected("shared-supervisor") {
        return;
    }
    let mut sandbox = create_sandbox().await;
    let ns = namespace();
    let id = sandbox_id(&sandbox.name, None).await;
    let name = format!("os-shared-control-{id}");
    assert!(
        policy(&ns, &name).await.is_some(),
        "{name} missing; this lane must place sandboxes on the shared supervisor"
    );
    for mutation in [Mutation::Edit, Mutation::Replace, Mutation::StripLabels] {
        let case = format!("os-shared-control/{}", mutation.as_str());
        let expected = baseline(&ns, &name).await;
        let since = mutate(&ns, &name, mutation).await;
        wait_restored(&ns, &name, &expected, since, &case).await;
        if mutation == Mutation::StripLabels {
            assert_not_suspended(&sandbox.name, None, &case).await;
        } else {
            wait_drift_suspended(&sandbox.name, None, &name, since, &case).await;
            start_and_exec(&sandbox, None, &case.replace('/', "-")).await;
        }
    }
    let since = mutate(&ns, &name, Mutation::Delete).await;
    wait_drift_suspended(
        &sandbox.name,
        None,
        &name,
        since,
        "os-shared-control/delete",
    )
    .await;
    assert!(
        policy(&ns, &name).await.is_none(),
        "the guard must not re-create {name}"
    );
    start_and_exec(&sandbox, None, "os-shared-control-delete").await;
    assert!(
        policy(&ns, &name).await.is_some(),
        "start re-creates {name}"
    );
    mutate(&ns, &name, Mutation::Delete).await;
    sandbox.delete().await.expect("delete sandbox");
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert!(
        policy(&ns, &name).await.is_none(),
        "{name} came back after its sandbox was deleted"
    );
    println!("NETPOL_NO_RESURRECTION policy={name} observed_s=40");
}

/// Operator mode: the guard watches allowlisted namespaces using the verbs the
/// workspace chart grants, and leaves everything else alone.
#[tokio::test]
async fn operator_mode_guards_allowlisted_namespaces_only() {
    if !selected("operator") {
        return;
    }
    let ns = format!("ng-op-{}", std::process::id() % 100_000);
    kubectl(&["create", "namespace", &ns]).await;
    kubectl(&[
        "label",
        "namespace",
        &ns,
        "openshell.ai/e2e-operator-workspace=true",
    ])
    .await;
    let chart = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/helm/openshell-workspace"
    );
    let context = std::env::var("OPENSHELL_E2E_KUBE_CONTEXT_ACTIVE").expect("kube context");
    let helm = tokio::process::Command::new("helm")
        .args([
            "--kube-context",
            &context,
            "install",
            "openshell-workspace",
            chart,
            "--namespace",
            &ns,
            "--wait",
        ])
        .output()
        .await
        .expect("spawn helm");
    assert!(
        helm.status.success(),
        "workspace chart: {}",
        String::from_utf8_lossy(&helm.stderr)
    );

    // A namespace outside the allowlist holding a look-alike policy.
    let foreign = format!("ng-foreign-{}", std::process::id() % 100_000);
    kubectl(&["create", "namespace", &foreign]).await;
    let look_alike = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": {"name": WORKLOADS, "namespace": foreign, "labels": {MANAGED_BY: "openshell"}},
        "spec": {"podSelector": {}, "policyTypes": ["Egress"], "egress": [{}]}
    });
    kubectl_stdin(&["apply", "-f", "-"], &look_alike.to_string()).await;

    let (output, code) = run_cli(&["workspace", "create", "--name", &ns]).await;
    assert_eq!(code, 0, "workspace create: {output}");
    let name = format!("ng-sb-{}", std::process::id() % 100_000);
    // The gateway's namespace watcher needs a moment to allowlist the label.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (output, code) = run_cli(&[
            "sandbox",
            "create",
            "--detach",
            "--workspace",
            &ns,
            "--name",
            &name,
            "--",
            "sh",
            "-c",
            "exec sleep infinity",
        ])
        .await;
        if code == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "sandbox create: {output}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    wait_managed_ready(&ns, &name).await;
    // Give the allowlist poll time to start the namespace's watch.
    tokio::time::sleep(Duration::from_secs(6)).await;

    for mutation in Mutation::ALL {
        let case = format!("operator-workloads/{}", mutation.as_str());
        let expected = baseline(&ns, WORKLOADS).await;
        let since = mutate(&ns, WORKLOADS, mutation).await;
        wait_restored(&ns, WORKLOADS, &expected, since, &case).await;
        if mutation == Mutation::StripLabels {
            let phase = sandbox_json(&name, Some(&ns)).await["phase"].clone();
            assert_eq!(phase, "Ready", "{case}: label repair must not suspend");
        } else {
            wait_drift_suspended(&name, Some(&ns), WORKLOADS, since, &case).await;
            let (output, code) = run_cli(&["sandbox", "start", &name, "--workspace", &ns]).await;
            assert_eq!(code, 0, "{case}: start: {output}");
            wait_managed_ready(&ns, &name).await;
        }
    }

    let untouched = policy(&foreign, WORKLOADS)
        .await
        .expect("look-alike policy");
    assert_eq!(
        untouched["metadata"]["generation"], 1,
        "the guard must not touch a namespace outside the allowlist: {untouched}"
    );
    println!("NETPOL_FOREIGN_UNTOUCHED namespace={foreign}");
    let _ = run_cli(&["sandbox", "delete", &name, "--workspace", &ns]).await;
    let _ = run_cli(&["workspace", "delete", &ns]).await;
    kubectl(&["delete", "namespace", &ns, &foreign, "--wait=false"]).await;
}

/// Managed mode: the guard binds its role in each managed namespace and
/// restores the namespace fences and the SSH ingress policy there.
#[tokio::test]
async fn managed_namespaces_get_the_guard_binding_and_watch() {
    if !selected("managed") {
        return;
    }
    let workspace = format!("ng{}", std::process::id() % 100_000);
    let (output, code) = run_cli(&["workspace", "create", "--name", &workspace]).await;
    assert_eq!(code, 0, "workspace create: {output}");
    let name = format!("ng-sb-{}", std::process::id() % 100_000);
    create_managed_sandbox(&workspace, &name).await;
    let ns = format!("openshell-openshell-{workspace}");

    let binding: serde_json::Value = serde_json::from_str(
        &kubectl(&[
            "-n",
            &ns,
            "get",
            "rolebinding",
            "openshell-netpol-guard",
            "-o",
            "json",
        ])
        .await,
    )
    .unwrap();
    assert_eq!(binding["roleRef"]["kind"], "ClusterRole");
    assert!(
        binding["roleRef"]["name"]
            .as_str()
            .unwrap()
            .contains("netpol-guard")
    );
    println!(
        "NETPOL_BINDING namespace={ns} role={}",
        binding["roleRef"]["name"]
    );
    // No cluster-wide list or watch was granted.
    let (allowed, _, _) = kubectl_raw(
        &[
            "auth",
            "can-i",
            "watch",
            "networkpolicies",
            "-A",
            "--as",
            "system:serviceaccount:openshell:openshell",
        ],
        None,
    )
    .await;
    assert!(
        !allowed,
        "the gateway must not watch NetworkPolicies cluster-wide"
    );

    for (policy_name, mutations) in [
        (SSH, &Mutation::ALL[..]),
        (WORKLOADS, &[Mutation::Delete, Mutation::Edit][..]),
    ] {
        for mutation in mutations {
            let mutation = *mutation;
            let case = format!("managed-{policy_name}/{}", mutation.as_str());
            let expected = baseline(&ns, policy_name).await;
            let since = mutate(&ns, policy_name, mutation).await;
            wait_restored(&ns, policy_name, &expected, since, &case).await;
            if policy_name == WORKLOADS {
                wait_drift_suspended(&name, Some(&workspace), policy_name, since, &case).await;
                let (output, code) =
                    run_cli(&["sandbox", "start", &name, "--workspace", &workspace]).await;
                assert_eq!(code, 0, "{case}: start: {output}");
                wait_managed_ready(&workspace, &name).await;
            } else {
                // SSH ingress isn't bound into evidence: nothing is suspended.
                let phase = sandbox_json(&name, Some(&workspace)).await["phase"].clone();
                assert_eq!(phase, "Ready", "{case}: SSH repair must not suspend");
            }
        }
    }
    let _ = run_cli(&["sandbox", "delete", &name, "--workspace", &workspace]).await;
    let _ = run_cli(&["workspace", "delete", &workspace]).await;
}

/// Managed mode: a namespace created before the gateway started gets the
/// binding and a watch when the gateway restarts. The restart ends the CLI's
/// port-forward, so this sorts after the other managed case and uses kubectl.
#[tokio::test]
async fn restarted_gateway_guards_existing_managed_namespaces() {
    if !selected("managed") {
        return;
    }
    let workspace = format!("nr{}", std::process::id() % 100_000);
    let (output, code) = run_cli(&["workspace", "create", "--name", &workspace]).await;
    assert_eq!(code, 0, "workspace create: {output}");
    let name = format!("nr-sb-{}", std::process::id() % 100_000);
    create_managed_sandbox(&workspace, &name).await;
    let ns = format!("openshell-openshell-{workspace}");
    kubectl(&["-n", &ns, "delete", "rolebinding", "openshell-netpol-guard"]).await;
    let gateway_ns =
        std::env::var("OPENSHELL_E2E_GATEWAY_NAMESPACE").unwrap_or_else(|_| "openshell".into());
    kubectl(&[
        "-n",
        &gateway_ns,
        "rollout",
        "restart",
        "statefulset/openshell",
    ])
    .await;
    kubectl(&[
        "-n",
        &gateway_ns,
        "rollout",
        "status",
        "statefulset/openshell",
        "--timeout=300s",
    ])
    .await;
    let deadline = Instant::now() + Duration::from_mins(2);
    while !kubectl_raw(
        &["-n", &ns, "get", "rolebinding", "openshell-netpol-guard"],
        None,
    )
    .await
    .0
    {
        assert!(
            Instant::now() < deadline,
            "restarted gateway did not bind the guard role in {ns}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("NETPOL_BINDING_AFTER_RESTART namespace={ns}");
    // Give the new watch its first list.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let expected = baseline(&ns, SSH).await;
    let since = mutate(&ns, SSH, Mutation::Delete).await;
    wait_restored(&ns, SSH, &expected, since, "managed-existing-ssh/delete").await;
    kubectl(&["delete", "namespace", &ns, "--wait=false"]).await;
}
