// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Guard for the `NetworkPolicy` objects the driver owns.
//!
//! The driver creates five kinds of `NetworkPolicy` that sandbox isolation
//! depends on. None of them is user-serviceable, so when one is deleted,
//! edited, replaced or loses its labels the guard restores it. The expected
//! object is always rendered from driver configuration and the trusted Sandbox
//! resources; nothing but the observed object's identity (UID,
//! resourceVersion) is read back from the cluster.
//!
//! This file holds the pure core: rendering, the accepted-render set used for
//! expand/contract upgrades, drift classification and repair planning. The
//! I/O shell that performs repairs lives in `netpol_guard_shell`.

use super::{
    LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE, MANAGED_SSH_NETWORK_POLICY_NAME,
    managed_ssh_network_policy,
};
use crate::config::KubernetesComputeConfig;
use crate::sandbox_runtime::{
    NAMESPACE_SUPERVISOR_EGRESS_POLICY_NAME, NAMESPACE_WORKLOAD_POLICY_NAME, SandboxRuntimeNames,
    control_policy, shared_control_policy, workload_fence,
};
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use std::collections::BTreeMap;

/// Diagnostic stamp naming the release that last wrote a guarded policy. It
/// is never compared: anyone who can edit the policy can edit the stamp, and
/// an expected render that required it would reject every policy written by
/// an earlier release.
pub const ANNOTATION_RENDERED_BY: &str = "openshell.ai/rendered-by";
const LABEL_COMPONENT: &str = "openshell.ai/component";
const CONTROL_POLICY_PREFIX: &str = "os-control-";
const SHARED_CONTROL_POLICY_PREFIX: &str = "os-shared-control-";

/// Identity of one guarded object.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PolicyKey {
    pub namespace: String,
    pub name: String,
}

impl PolicyKey {
    pub fn new(namespace: &str, name: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
        }
    }
}

/// What a guarded policy is for, resolved from its name and the trusted
/// Sandbox list, never from the observed object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardedKind {
    /// `openshell-sandbox-workloads`: namespace-wide workload deny.
    WorkloadFence,
    /// `openshell-sandbox-supervisors`: namespace-wide supervisor egress.
    SupervisorEgress,
    /// `os-control-<id>`: one workload's allowance to its own supervisor.
    Control { sandbox_id: String },
    /// `os-shared-control-<id>`: one workload's allowance to the shared
    /// supervisor.
    SharedControl { sandbox_id: String },
    /// `openshell-sandbox-ssh`: managed-mode SSH ingress from the gateway.
    ManagedSsh,
}

impl GuardedKind {
    /// Per-sandbox policies are created by `sandbox start`, owned by their
    /// Sandbox and garbage-collected with it. The guard only replaces them,
    /// so a deleted Sandbox's policy can never be resurrected.
    pub const fn is_per_sandbox(&self) -> bool {
        matches!(self, Self::Control { .. } | Self::SharedControl { .. })
    }

    pub fn sandbox_id(&self) -> Option<&str> {
        match self {
            Self::Control { sandbox_id } | Self::SharedControl { sandbox_id } => Some(sandbox_id),
            _ => None,
        }
    }
}

/// Whether `name` can be a guarded policy. Used to drop unrelated watch
/// events cheaply; the authoritative mapping is [`resolve_kind`].
pub fn guarded_name(name: &str) -> bool {
    name == NAMESPACE_WORKLOAD_POLICY_NAME
        || name == NAMESPACE_SUPERVISOR_EGRESS_POLICY_NAME
        || name == MANAGED_SSH_NETWORK_POLICY_NAME
        || name.starts_with(CONTROL_POLICY_PREFIX)
        || name.starts_with(SHARED_CONTROL_POLICY_PREFIX)
}

/// Map a policy name to its kind using only trusted sandbox IDs. A per-sandbox
/// name that no listed Sandbox renders maps to `None`, so a policy left behind
/// or created by someone else is never adopted.
pub fn resolve_kind<'a>(
    name: &str,
    sandbox_ids: impl IntoIterator<Item = &'a str>,
    managed_ssh: bool,
) -> Option<GuardedKind> {
    if name == NAMESPACE_WORKLOAD_POLICY_NAME {
        return Some(GuardedKind::WorkloadFence);
    }
    if name == NAMESPACE_SUPERVISOR_EGRESS_POLICY_NAME {
        return Some(GuardedKind::SupervisorEgress);
    }
    if name == MANAGED_SSH_NETWORK_POLICY_NAME {
        return managed_ssh.then_some(GuardedKind::ManagedSsh);
    }
    sandbox_ids.into_iter().find_map(|sandbox_id| {
        let names = SandboxRuntimeNames::new(sandbox_id);
        if names.control_policy == name {
            Some(GuardedKind::Control {
                sandbox_id: sandbox_id.to_string(),
            })
        } else if names.shared_control_policy == name {
            Some(GuardedKind::SharedControl {
                sandbox_id: sandbox_id.to_string(),
            })
        } else {
            None
        }
    })
}

/// Render generations of the guarded policies, for expand/contract upgrades.
///
/// A release that changes a render first adds the new generation to
/// [`ACCEPTED_RENDERS`] while still writing the old one ([`WRITE_RENDER`]).
/// The following release switches `WRITE_RENDER`. Old and new replicas of a
/// rolling upgrade between adjacent releases then accept each other's
/// objects, so neither rewrites nor suspends on the other's render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderGeneration {
    V1,
}

pub const ACCEPTED_RENDERS: &[RenderGeneration] = &[RenderGeneration::V1];
pub const WRITE_RENDER: RenderGeneration = RenderGeneration::V1;

/// The two namespace fences with the labels the driver writes.
pub fn namespace_fences(
    namespace: &str,
    boundary_port: u16,
    generation: RenderGeneration,
) -> [NetworkPolicy; 2] {
    match generation {
        RenderGeneration::V1 => {
            let fence = workload_fence(
                namespace,
                &SandboxRuntimeNames::new("namespace"),
                boundary_port,
            );
            let mut policies = [fence.workload_policy, fence.supervisor_policy];
            for (policy, component) in policies
                .iter_mut()
                .zip(["sandbox-workload-fence", "sandbox-supervisor-egress"])
            {
                let labels = policy.metadata.labels.get_or_insert_default();
                labels.insert(
                    LABEL_MANAGED_BY.to_string(),
                    LABEL_MANAGED_BY_VALUE.to_string(),
                );
                labels.insert(LABEL_COMPONENT.to_string(), component.to_string());
            }
            policies
        }
    }
}

/// Where the shared supervisor's Pods are, read from its live Service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedTarget {
    pub namespace: String,
    pub selector: BTreeMap<String, String>,
    pub port: u16,
}

/// Everything rendering needs, from driver configuration and trusted reads.
pub struct RenderInputs<'a> {
    pub namespace: &'a str,
    pub config: &'a KubernetesComputeConfig,
    /// Owner of a per-sandbox policy, built from the trusted Sandbox CR.
    pub owner: Option<OwnerReference>,
    /// Live shared supervisor target, for `os-shared-control-*` only.
    pub shared_target: Option<&'a SharedTarget>,
}

/// Render `kind` at `generation`, or `None` when an input is unavailable.
pub fn render(
    kind: &GuardedKind,
    generation: RenderGeneration,
    inputs: &RenderInputs<'_>,
) -> Option<NetworkPolicy> {
    let port = inputs.config.sandbox_runtime.boundary_port;
    match kind {
        GuardedKind::WorkloadFence => {
            let [workload, _] = namespace_fences(inputs.namespace, port, generation);
            Some(workload)
        }
        GuardedKind::SupervisorEgress => {
            let [_, supervisor] = namespace_fences(inputs.namespace, port, generation);
            Some(supervisor)
        }
        GuardedKind::Control { sandbox_id } => Some(control_policy(
            inputs.namespace,
            &SandboxRuntimeNames::new(sandbox_id),
            sandbox_id,
            port,
            inputs.owner.clone()?,
        )),
        GuardedKind::SharedControl { sandbox_id } => {
            let target = inputs.shared_target?;
            Some(shared_control_policy(
                inputs.namespace,
                &SandboxRuntimeNames::new(sandbox_id),
                sandbox_id,
                &target.namespace,
                target.selector.clone(),
                target.port,
                inputs.owner.clone()?,
            ))
        }
        GuardedKind::ManagedSsh => {
            Some(managed_ssh_network_policy(inputs.namespace, inputs.config))
        }
    }
}

/// The expected object for one key: the write render first, then any other
/// accepted renders, which are matched but never written.
#[derive(Clone, Debug)]
pub struct ExpectedPolicy {
    pub key: PolicyKey,
    pub kind: GuardedKind,
    pub accepted: Vec<NetworkPolicy>,
}

impl ExpectedPolicy {
    pub fn build(key: PolicyKey, kind: GuardedKind, inputs: &RenderInputs<'_>) -> Option<Self> {
        let mut generations = vec![WRITE_RENDER];
        generations.extend(
            ACCEPTED_RENDERS
                .iter()
                .copied()
                .filter(|g| *g != WRITE_RENDER),
        );
        let accepted = generations
            .into_iter()
            .map(|generation| render(&kind, generation, inputs))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            key,
            kind,
            accepted,
        })
    }

    pub fn write_render(&self) -> &NetworkPolicy {
        &self.accepted[0]
    }
}

/// How an observed policy differs from what the driver expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drift {
    InSync,
    Missing,
    /// The spec matches no accepted render. Enforcement may have changed.
    Spec,
    /// The spec matches an accepted render but required labels or
    /// annotations are missing. Enforcement is unchanged.
    Metadata,
}

impl Drift {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in-sync",
            Self::Missing => "missing",
            Self::Spec => "spec",
            Self::Metadata => "metadata",
        }
    }
}

/// The Kubernetes API server omits explicitly empty rule arrays when it
/// persists a `NetworkPolicy`. For a policy type named in `policyTypes`, an
/// omitted rule array and an empty one both deny all traffic.
pub fn normalized_spec(mut spec: Option<NetworkPolicySpec>) -> Option<NetworkPolicySpec> {
    if let Some(spec) = spec.as_mut() {
        if spec.egress.as_ref().is_some_and(Vec::is_empty) {
            spec.egress = None;
        }
        if spec.ingress.as_ref().is_some_and(Vec::is_empty) {
            spec.ingress = None;
        }
    }
    spec
}

/// Whether every required entry is present with the same value. Extra
/// entries, such as ones injected by admission, are tolerated.
pub fn contains_required_metadata(
    actual: Option<&BTreeMap<String, String>>,
    required: Option<&BTreeMap<String, String>>,
) -> bool {
    required.is_none_or(|required| {
        actual.is_some_and(|actual| {
            required
                .iter()
                .filter(|(key, _)| key.as_str() != ANNOTATION_RENDERED_BY)
                .all(|(key, value)| actual.get(key) == Some(value))
        })
    })
}

pub fn spec_matches(existing: &NetworkPolicy, expected: &NetworkPolicy) -> bool {
    normalized_spec(existing.spec.clone()) == normalized_spec(expected.spec.clone())
}

pub fn metadata_matches(existing: &NetworkPolicy, expected: &NetworkPolicy) -> bool {
    contains_required_metadata(
        existing.metadata.labels.as_ref(),
        expected.metadata.labels.as_ref(),
    ) && contains_required_metadata(
        existing.metadata.annotations.as_ref(),
        expected.metadata.annotations.as_ref(),
    )
}

/// Whether `existing` is any accepted render, spec and required metadata.
pub fn matches_any(existing: &NetworkPolicy, accepted: &[NetworkPolicy]) -> bool {
    accepted
        .iter()
        .any(|expected| spec_matches(existing, expected) && metadata_matches(existing, expected))
}

pub fn classify(observed: Option<&NetworkPolicy>, expected: &ExpectedPolicy) -> Drift {
    let Some(observed) = observed else {
        return Drift::Missing;
    };
    if matches_any(observed, &expected.accepted) {
        return Drift::InSync;
    }
    if expected
        .accepted
        .iter()
        .any(|render| spec_matches(observed, render))
    {
        Drift::Metadata
    } else {
        Drift::Spec
    }
}

/// Facts about the Sandbox that owns a per-sandbox policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoundState {
    /// The generation may run (running or bootstrapping).
    pub may_run: bool,
    /// The policy generation bound into the running generation's evidence.
    pub bound_generation: Option<i64>,
}

/// Why no repair is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAction {
    InSync,
    /// A missing per-sandbox policy is re-created by `sandbox start`.
    PerSandboxMissing,
    /// A bound shared-control policy differs only from a re-read of the live
    /// shared supervisor Service, not from what was bound. Rewriting it would
    /// suspend a healthy sandbox, for example after a Helm upgrade changed the
    /// Service selector.
    BoundSharedTarget,
    /// The observed object carries no resourceVersion to guard a replace.
    NoResourceVersion,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RepairAction {
    None(NoAction),
    Create(NetworkPolicy),
    Replace {
        body: NetworkPolicy,
        resource_version: String,
    },
}

pub fn plan_repair(
    drift: Drift,
    observed: Option<&NetworkPolicy>,
    expected: &ExpectedPolicy,
    bound: BoundState,
    release: &str,
) -> RepairAction {
    match drift {
        Drift::InSync => RepairAction::None(NoAction::InSync),
        Drift::Missing if expected.kind.is_per_sandbox() => {
            RepairAction::None(NoAction::PerSandboxMissing)
        }
        Drift::Missing => RepairAction::Create(write_body(expected, None, release)),
        Drift::Spec | Drift::Metadata => {
            let Some(observed) = observed else {
                return RepairAction::None(NoAction::NoResourceVersion);
            };
            if drift == Drift::Spec
                && matches!(expected.kind, GuardedKind::SharedControl { .. })
                && bound.may_run
                && bound.bound_generation.is_some()
                && bound.bound_generation == observed.metadata.generation
            {
                return RepairAction::None(NoAction::BoundSharedTarget);
            }
            let Some(resource_version) = observed.metadata.resource_version.clone() else {
                return RepairAction::None(NoAction::NoResourceVersion);
            };
            RepairAction::Replace {
                body: write_body(expected, Some(observed), release),
                resource_version,
            }
        }
    }
}

/// The object a repair writes: the write render with its required labels,
/// the diagnostic `rendered-by` stamp, and for a replace the observed
/// resourceVersion as the precondition. Labels and annotations other writers
/// added are kept so the guard doesn't fight admission injectors; they never
/// affect enforcement. Owner references always come from the render, so a
/// foreign owner can't get a namespace fence garbage-collected.
pub fn write_body(
    expected: &ExpectedPolicy,
    observed: Option<&NetworkPolicy>,
    release: &str,
) -> NetworkPolicy {
    let mut body = expected.write_render().clone();
    if let Some(observed) = observed {
        let mut labels = observed.metadata.labels.clone().unwrap_or_default();
        labels.extend(body.metadata.labels.take().unwrap_or_default());
        body.metadata.labels = Some(labels);
        let mut annotations = observed.metadata.annotations.clone().unwrap_or_default();
        annotations.extend(body.metadata.annotations.take().unwrap_or_default());
        body.metadata.annotations = Some(annotations);
        body.metadata
            .resource_version
            .clone_from(&observed.metadata.resource_version);
    }
    body.metadata.annotations.get_or_insert_default().insert(
        ANNOTATION_RENDERED_BY.to_string(),
        format!("openshell {release}"),
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::networking::v1::{NetworkPolicyEgressRule, NetworkPolicyPeer};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

    const NS: &str = "openshell";
    type Mutation = Box<dyn Fn(&mut NetworkPolicy)>;

    fn config() -> KubernetesComputeConfig {
        KubernetesComputeConfig::default()
    }

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "agents.x-k8s.io/v1beta1".into(),
            kind: "Sandbox".into(),
            name: "sandbox-cr".into(),
            uid: "cr-uid".into(),
            ..Default::default()
        }
    }

    fn expected(kind: GuardedKind, name: &str) -> ExpectedPolicy {
        let config = config();
        let target = SharedTarget {
            namespace: "openshell".into(),
            selector: BTreeMap::from([("app".into(), "shared".into())]),
            port: 5600,
        };
        ExpectedPolicy::build(
            PolicyKey::new(NS, name),
            kind,
            &RenderInputs {
                namespace: NS,
                config: &config,
                owner: Some(owner()),
                shared_target: Some(&target),
            },
        )
        .unwrap()
    }

    fn workload() -> ExpectedPolicy {
        expected(GuardedKind::WorkloadFence, NAMESPACE_WORKLOAD_POLICY_NAME)
    }

    /// The object as the API server stores it: identity added, empty rule
    /// arrays dropped, and metadata injected by admission.
    fn stored(expected: &ExpectedPolicy) -> NetworkPolicy {
        let mut policy = expected.write_render().clone();
        policy.metadata.uid = Some("uid-1".into());
        policy.metadata.resource_version = Some("100".into());
        policy.metadata.generation = Some(1);
        policy
            .metadata
            .labels
            .get_or_insert_default()
            .insert("injected".into(), "yes".into());
        policy
            .metadata
            .annotations
            .get_or_insert_default()
            .insert("injected".into(), "yes".into());
        policy.spec = normalized_spec(policy.spec);
        policy
    }

    fn spec(policy: &mut NetworkPolicy) -> &mut NetworkPolicySpec {
        policy.spec.as_mut().unwrap()
    }

    #[test]
    fn stored_policies_with_api_normalization_and_injected_metadata_are_in_sync() {
        for expected in [
            workload(),
            expected(
                GuardedKind::SupervisorEgress,
                NAMESPACE_SUPERVISOR_EGRESS_POLICY_NAME,
            ),
            expected(
                GuardedKind::Control {
                    sandbox_id: "sb".into(),
                },
                "os-control-sb",
            ),
            expected(
                GuardedKind::SharedControl {
                    sandbox_id: "sb".into(),
                },
                "os-shared-control-sb",
            ),
            expected(GuardedKind::ManagedSsh, MANAGED_SSH_NETWORK_POLICY_NAME),
        ] {
            assert_eq!(
                classify(Some(&stored(&expected)), &expected),
                Drift::InSync,
                "{:?}",
                expected.kind
            );
        }
    }

    #[test]
    fn classify_reports_each_kind_of_drift() {
        let expected = workload();
        assert_eq!(classify(None, &expected), Drift::Missing);

        let cases: Vec<(&str, Mutation)> = vec![
            (
                "pod selector widened",
                Box::new(|p| spec(p).pod_selector = LabelSelector::default()),
            ),
            (
                "egress policy type removed",
                Box::new(|p| spec(p).policy_types = Some(vec!["Ingress".into()])),
            ),
            (
                "allow-all egress added",
                Box::new(|p| spec(p).egress = Some(vec![NetworkPolicyEgressRule::default()])),
            ),
            (
                "ingress source widened",
                Box::new(|p| {
                    spec(p).ingress.as_mut().unwrap()[0].from =
                        Some(vec![NetworkPolicyPeer::default()]);
                }),
            ),
            (
                "boundary port changed",
                Box::new(|p| {
                    spec(p).ingress.as_mut().unwrap()[0].ports.as_mut().unwrap()[0].port =
                        Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1));
                }),
            ),
        ];
        for (case, mutate) in cases {
            let mut observed = stored(&expected);
            mutate(&mut observed);
            assert_eq!(classify(Some(&observed), &expected), Drift::Spec, "{case}");
        }

        let mut stripped = stored(&expected);
        stripped.metadata.labels = None;
        assert_eq!(classify(Some(&stripped), &expected), Drift::Metadata);
        let mut relabelled = stored(&expected);
        relabelled
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(LABEL_COMPONENT.into(), "other".into());
        assert_eq!(classify(Some(&relabelled), &expected), Drift::Metadata);
        // A new UID with the expected content is not drift: the generation
        // binding, not the guard, decides whether a running sandbox adopted it.
        let mut replaced = stored(&expected);
        replaced.metadata.uid = Some("uid-2".into());
        assert_eq!(classify(Some(&replaced), &expected), Drift::InSync);
    }

    #[test]
    fn rendered_by_never_affects_matching() {
        let expected = workload();
        let mut old = stored(&expected);
        old.metadata
            .annotations
            .as_mut()
            .unwrap()
            .remove(ANNOTATION_RENDERED_BY);
        assert_eq!(classify(Some(&old), &expected), Drift::InSync);
        let mut forged = stored(&expected);
        forged
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(ANNOTATION_RENDERED_BY.into(), "openshell 999.0.0".into());
        assert_eq!(classify(Some(&forged), &expected), Drift::InSync);
        // Even an expected object that carried the stamp must not require it.
        let mut stamped = expected.clone();
        stamped.accepted[0] = write_body(&expected, None, "1.2.3");
        assert_eq!(classify(Some(&old), &stamped), Drift::InSync);
    }

    #[test]
    fn a_release_accepting_two_renders_never_rewrites_the_other_render() {
        let mut expected = workload();
        // Model release N+1: it writes the current render but also accepts a
        // future one that, say, adds a DNS egress rule.
        let mut future = expected.accepted[0].clone();
        spec(&mut future).egress = Some(vec![NetworkPolicyEgressRule::default()]);
        expected.accepted.push(future.clone());
        let mut written_by_next_release = future;
        written_by_next_release.metadata.resource_version = Some("7".into());
        written_by_next_release.metadata.generation = Some(1);
        let drift = classify(Some(&written_by_next_release), &expected);
        assert_eq!(drift, Drift::InSync);
        assert_eq!(
            plan_repair(
                drift,
                Some(&written_by_next_release),
                &expected,
                BoundState::default(),
                "1"
            ),
            RepairAction::None(NoAction::InSync)
        );
        // Anything outside the accepted set is still repaired to the write render.
        let mut edited = stored(&expected);
        spec(&mut edited).pod_selector = LabelSelector::default();
        let RepairAction::Replace { body, .. } = plan_repair(
            classify(Some(&edited), &expected),
            Some(&edited),
            &expected,
            BoundState::default(),
            "1",
        ) else {
            panic!("expected a replace");
        };
        assert!(spec_matches(&body, &expected.accepted[0]));
    }

    #[test]
    fn plan_repair_matrix() {
        let fence = workload();
        let control = expected(
            GuardedKind::Control {
                sandbox_id: "sb".into(),
            },
            "os-control-sb",
        );
        assert!(matches!(
            plan_repair(Drift::Missing, None, &fence, BoundState::default(), "1"),
            RepairAction::Create(_)
        ));
        assert_eq!(
            plan_repair(Drift::Missing, None, &control, BoundState::default(), "1"),
            RepairAction::None(NoAction::PerSandboxMissing)
        );
        for expected in [&fence, &control] {
            let observed = stored(expected);
            for drift in [Drift::Spec, Drift::Metadata] {
                let RepairAction::Replace {
                    resource_version,
                    body,
                } = plan_repair(drift, Some(&observed), expected, BoundState::default(), "1")
                else {
                    panic!("{drift:?} must replace");
                };
                assert_eq!(resource_version, "100");
                assert_eq!(body.metadata.resource_version.as_deref(), Some("100"));
            }
        }
    }

    #[test]
    fn bound_shared_control_is_not_rewritten_to_a_new_service_selector() {
        let expected = expected(
            GuardedKind::SharedControl {
                sandbox_id: "sb".into(),
            },
            "os-shared-control-sb",
        );
        let mut observed = stored(&expected);
        // Bound with an older Service selector, never edited since.
        spec(&mut observed).egress.as_mut().unwrap()[0].to =
            Some(vec![NetworkPolicyPeer::default()]);
        let drift = classify(Some(&observed), &expected);
        assert_eq!(drift, Drift::Spec);
        let running = BoundState {
            may_run: true,
            bound_generation: Some(1),
        };
        assert_eq!(
            plan_repair(drift, Some(&observed), &expected, running, "1"),
            RepairAction::None(NoAction::BoundSharedTarget)
        );
        // Edited after binding, or not running: repair to the live render.
        let edited = BoundState {
            may_run: true,
            bound_generation: Some(0),
        };
        assert!(matches!(
            plan_repair(drift, Some(&observed), &expected, edited, "1"),
            RepairAction::Replace { .. }
        ));
        assert!(matches!(
            plan_repair(
                drift,
                Some(&observed),
                &expected,
                BoundState::default(),
                "1"
            ),
            RepairAction::Replace { .. }
        ));
    }

    #[test]
    fn write_body_restores_labels_keeps_foreign_metadata_and_drops_foreign_owners() {
        let fence = workload();
        let mut observed = stored(&fence);
        observed.metadata.labels = Some(BTreeMap::from([("team".into(), "a".into())]));
        observed.metadata.owner_references = Some(vec![owner()]);
        spec(&mut observed).egress = Some(vec![NetworkPolicyEgressRule::default()]);
        let body = write_body(&fence, Some(&observed), "1.2.3");
        let labels = body.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get(LABEL_MANAGED_BY).map(String::as_str),
            Some(LABEL_MANAGED_BY_VALUE)
        );
        assert_eq!(labels.get("team").map(String::as_str), Some("a"));
        assert_eq!(body.metadata.owner_references, None);
        assert_eq!(
            body.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get(ANNOTATION_RENDERED_BY)
                .map(String::as_str),
            Some("openshell 1.2.3")
        );
        assert!(matches_any(&body, &fence.accepted));
        // A per-sandbox repair carries the owner from the trusted Sandbox CR.
        let control = expected(
            GuardedKind::Control {
                sandbox_id: "sb".into(),
            },
            "os-control-sb",
        );
        let mut foreign = stored(&control);
        foreign.metadata.owner_references = None;
        assert_eq!(
            write_body(&control, Some(&foreign), "1")
                .metadata
                .owner_references,
            Some(vec![owner()])
        );
    }

    #[test]
    fn expected_state_never_depends_on_the_observed_object() {
        assert_eq!(
            resolve_kind(NAMESPACE_WORKLOAD_POLICY_NAME, [], false),
            Some(GuardedKind::WorkloadFence)
        );
        assert_eq!(
            resolve_kind(MANAGED_SSH_NETWORK_POLICY_NAME, [], false),
            None
        );
        assert_eq!(
            resolve_kind(MANAGED_SSH_NETWORK_POLICY_NAME, [], true),
            Some(GuardedKind::ManagedSsh)
        );
        // Per-sandbox names map only through trusted sandbox IDs.
        assert_eq!(resolve_kind("os-control-other", ["sb"], false), None);
        assert_eq!(
            resolve_kind("os-control-sb", ["other", "sb"], false),
            Some(GuardedKind::Control {
                sandbox_id: "sb".into()
            })
        );
        assert_eq!(
            resolve_kind("os-shared-control-sb", ["sb"], false),
            Some(GuardedKind::SharedControl {
                sandbox_id: "sb".into()
            })
        );
        // Rendering takes no observed input, so a hostile sandbox-id label or
        // annotation on the stored object can't change what is written.
        let a = workload();
        let b = workload();
        assert_eq!(a.accepted, b.accepted);
        assert!(guarded_name("os-control-x") && guarded_name(NAMESPACE_WORKLOAD_POLICY_NAME));
        assert!(!guarded_name("allow-dns"));
    }
}
