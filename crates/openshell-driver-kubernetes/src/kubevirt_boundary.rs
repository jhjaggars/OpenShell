// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `KubeVirt` boundary provisioning for the shared authenticated boundary
//! protocol, modeled on the VM driver's `VmBoundarySpec`
//! (`crates/openshell-driver-vm/src/isolation/mod.rs`).
//!
//! Claims model (user decision, option A): the boundary claims the
//! `VirtualMachine`'s UID (stable across stop/start) and the generation,
//! plus the image identity. The outer-fence evidence carries the
//! `NetworkPolicy` UID that fences the virt-launcher pod's egress,
//! projected against the generation. The virt-launcher pod's own UID
//! cannot be claimed: `KubeVirt` creates that pod asynchronously, so its
//! UID does not exist when the boundary config is rendered.
//!
//! Image identity: the VM driver claims the image's registry digest;
//! the `KubeVirt` `PoC` claims the configured `guest_image` reference (a tag),
//! which is weaker. The production path is a digest-pinned guest image.
//!
//! Who verifies the fence:
//!
//! - the **driver** attests the fence by re-reading the `NetworkPolicy`
//!   each generation (`verify_fence`): it fails the start when the fence
//!   was deleted, its UID changed (replaced), or its spec drifted;
//! - the **boundary** checks the evidence's generation binding
//!   (`contract.rs:254 validate`);
//! - **descriptor/config evidence and claims consistency** is enforced at
//!   `isolation_protocol.rs:100` (the driver descriptor must match the
//!   provisioned boundary config exactly) and on confirmation
//!   (`runtime.rs:757`).
//!
//! Neither the boundary nor the supervisor can independently query the
//! cluster; the driver is the attestation authority for the fence.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

use openshell_isolation_interface::contract::{
    BackendError, ProvisioningEvidence, ResolvedWorkloadIdentity,
};
use openshell_sandbox_backend::boundary_protocol::{
    BoundaryConfig, BoundaryListener, GatewayVerificationKey, SandboxRuntimeDescriptor,
    SandboxTlsClientConfig, SandboxTlsServerConfig, SandboxTransport,
};
use serde::Serialize;

/// Image identity note.
///
/// The VM driver claims the image's registry digest
/// (`bootstrap_image_cache_identity` in
/// `crates/openshell-driver-vm/src/driver.rs`). The `KubeVirt` `PoC` claims
/// the configured `guest_image` reference (a tag), which is weaker: a tag
/// does not identify an image. The production path is a digest-pinned
/// Resource-claim keys.
///
/// The generation marker is recognized by the shared `is_vm_guest`
/// predicate in `boundary_protocol` (user decision, option A: both
/// `vm.generation` and `kubevirt.generation` mark a VM guest).
pub const CLAIM_VM_UID: &str = "kubevirt.vm_uid";
pub const CLAIM_GENERATION: &str =
    openshell_sandbox_backend::boundary_protocol::KUBEVIRT_GENERATION_CLAIM;
pub const CLAIM_IMAGE_IDENTITY: &str = "kubevirt.image_identity";

/// Where the guest init stages the boundary bootstrap artifacts. These are
/// the paths written into the `BoundaryConfig`'s TLS config and read by the
/// boundary at startup.
pub const GUEST_BOUNDARY_CONFIG_PATH: &str = "/.openshell/state/bootstrap/boundary.json";
pub const GUEST_BOUNDARY_CERTIFICATE_PATH: &str = "/.openshell/state/bootstrap/tls.crt";
pub const GUEST_BOUNDARY_PRIVATE_KEY_PATH: &str = "/.openshell/state/bootstrap/tls.key";

/// Native evidence that the virt-launcher `NetworkPolicy` fence exists and
/// belongs to this generation.
#[derive(Serialize)]
struct KubeVirtOuterFenceEvidence<'a> {
    generation: &'a str,
    network_policy_uid: &'a str,
    network_policy_resource_version: &'a str,
}

impl KubeVirtOuterFenceEvidence<'_> {
    fn project(&self) -> Result<ProvisioningEvidence, BackendError> {
        if self.generation.is_empty()
            || self.network_policy_uid.is_empty()
            || self.network_policy_resource_version.is_empty()
        {
            return Err(BackendError::Descriptor(
                "KubeVirt provisioning evidence is incomplete: the NetworkPolicy fence must \
                 exist with a UID and resourceVersion before the boundary config is rendered"
                    .to_string(),
            ));
        }
        let encoded = serde_json::to_vec(self).map_err(|error| {
            BackendError::Descriptor(format!("encode KubeVirt outer fence evidence: {error}"))
        })?;
        let projection =
            ProvisioningEvidence::from_enforcement_evidence(self.generation, &encoded)?;
        projection.validate(self.generation)?;
        Ok(projection)
    }
}

/// Driver-owned inputs that bind one `KubeVirt` VM generation to one
/// supervisor boundary.
pub struct KubeVirtBoundarySpec {
    pub boundary_id: String,
    pub generation: String,
    pub session_id: openshell_core::SandboxSessionId,
    pub session_rotation: openshell_core::jwt::SessionRotation,
    pub auth_epoch: openshell_core::jwt::CredentialEpoch,
    pub gateway_id: String,
    pub verification_keys: Vec<GatewayVerificationKey>,
    /// The `VirtualMachine` object's UID, stable across stop/start.
    pub vm_uid: String,
    /// The guest image reference, claimed as the image identity.
    pub image_identity: String,
    /// UID of the `NetworkPolicy` that fences the virt-launcher pod's
    /// egress. Re-read and verified on every generation.
    pub network_policy_uid: String,
    /// resourceVersion of the fence `NetworkPolicy` at render time.
    pub network_policy_resource_version: String,
    /// The supervisor Service authority the boundary dials
    /// (`RuntimeInitiatedTlsTcp`), e.g. `supervisor.openshell.svc:5500`.
    pub supervisor_authority: String,
    /// Resolved supervisor endpoint addresses, tried in order.
    pub supervisor_addresses: Vec<SocketAddr>,
    /// Route handle for this sandbox instance on the gateway-shared
    /// supervisor. Required: the user's decision is shared-supervisor-only,
    /// and an empty route would route ambiguously.
    pub route: String,
    /// TLS client config the supervisor uses to authenticate the boundary.
    pub supervisor_tls: SandboxTlsClientConfig,
    /// The workload identity (uid/gid) the boundary runs as.
    pub workload_identity: ResolvedWorkloadIdentity,
    /// Environment the boundary passes to workload children.
    pub child_env: HashMap<String, String>,
}

/// The protected guest config and matching runtime descriptor for one
/// `KubeVirt` VM.
pub struct KubeVirtBoundaryProvisioning {
    pub boundary_config: BoundaryConfig,
    pub runtime_descriptor: SandboxRuntimeDescriptor,
}

impl KubeVirtBoundarySpec {
    /// Produce both sides of the common protocol from one set of immutable
    /// driver inputs so their identity claims cannot drift.
    pub fn provision(self) -> Result<KubeVirtBoundaryProvisioning, BackendError> {
        if self.route.trim().is_empty() {
            return Err(BackendError::Descriptor(
                "KubeVirt boundary requires a shared-supervisor route handle; an empty                  route would route ambiguously"
                    .to_string(),
            ));
        }
        let resource_claims = BTreeMap::from([
            (CLAIM_VM_UID.to_string(), self.vm_uid.clone()),
            (CLAIM_GENERATION.to_string(), self.generation.clone()),
            (
                CLAIM_IMAGE_IDENTITY.to_string(),
                self.image_identity.clone(),
            ),
        ]);
        let provisioning_evidence = KubeVirtOuterFenceEvidence {
            generation: &self.generation,
            network_policy_uid: &self.network_policy_uid,
            network_policy_resource_version: &self.network_policy_resource_version,
        }
        .project()?;
        let sandbox_tls = SandboxTlsServerConfig {
            certificate_chain_path: GUEST_BOUNDARY_CERTIFICATE_PATH.into(),
            private_key_path: GUEST_BOUNDARY_PRIVATE_KEY_PATH.into(),
        };
        Ok(KubeVirtBoundaryProvisioning {
            boundary_config: BoundaryConfig {
                boundary_id: self.boundary_id.clone(),
                generation: self.generation.clone(),
                session_id: self.session_id,
                session_rotation: self.session_rotation,
                auth_epoch: self.auth_epoch,
                gateway_id: self.gateway_id,
                verification_keys: self.verification_keys,
                listener: BoundaryListener::RuntimeInitiatedTlsTcp {
                    authority: self.supervisor_authority.clone(),
                    addresses: self.supervisor_addresses,
                    tls: sandbox_tls,
                    route: Some(self.route.clone()),
                },
                resource_claims: resource_claims.clone(),
                resource_claim_files: BTreeMap::new(),
                workload_identity: self.workload_identity.clone(),
                provisioning_evidence: provisioning_evidence.clone(),
                child_env: self.child_env,
            },
            runtime_descriptor: SandboxRuntimeDescriptor {
                boundary_id: self.boundary_id,
                generation: self.generation,
                session_id: self.session_id,
                workload_identity: self.workload_identity,
                transport: SandboxTransport::RuntimeInitiatedRouted {
                    authority: self.supervisor_authority,
                    route: self.route,
                },
                tls: self.supervisor_tls,
                host_gateway_ip: None,
                resource_claims,
                provisioning_evidence,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the descriptor/config consistency check at
    /// `isolation_protocol.rs:100`.
    fn descriptor_matches_config(
        descriptor: &SandboxRuntimeDescriptor,
        config: &BoundaryConfig,
    ) -> Result<(), ()> {
        if descriptor.boundary_id != config.boundary_id
            || descriptor.generation != config.generation
            || descriptor.session_id != config.session_id
            || descriptor.workload_identity != config.workload_identity
            || descriptor.resource_claims != config.resource_claims
            || descriptor.provisioning_evidence != config.provisioning_evidence
        {
            Err(())
        } else {
            Ok(())
        }
    }

    fn spec() -> KubeVirtBoundarySpec {
        KubeVirtBoundarySpec {
            boundary_id: "sandbox-1".to_string(),
            generation: "generation-1".to_string(),
            session_id: openshell_core::SandboxSessionId::new(),
            session_rotation: openshell_core::jwt::SessionRotation::new(1).unwrap(),
            auth_epoch: openshell_core::jwt::CredentialEpoch::new(1).unwrap(),
            gateway_id: "gateway-1".to_string(),
            verification_keys: vec![GatewayVerificationKey {
                key_id: "key-1".to_string(),
                public_key_pem: "public-key".to_string(),
            }],
            vm_uid: "vm-uid-1".to_string(),
            image_identity: "kvpoc/openshell-sandbox-vm:dev".to_string(),
            network_policy_uid: "np-uid-1".to_string(),
            network_policy_resource_version: "1945".to_string(),
            supervisor_authority: "supervisor.openshell.svc:5500".to_string(),
            supervisor_addresses: vec!["10.43.0.10:5500".parse().unwrap()],
            route: "route-1".to_string(),
            supervisor_tls: SandboxTlsClientConfig {
                server_name: "boundary.sandbox.openshell".to_string(),
                trust_anchor_pem: "test-ca".to_string(),
            },
            workload_identity: ResolvedWorkloadIdentity::new(
                10001,
                10001,
                Vec::new(),
                "kubevirt-config".to_string(),
                format!("sandbox:{}", "vm-uid-1"),
            )
            .unwrap(),
            child_env: HashMap::new(),
        }
    }

    #[test]
    fn provision_produces_matching_claims_and_evidence() {
        let provisioned = spec().provision().expect("provision");
        assert_eq!(
            provisioned.boundary_config.resource_claims[CLAIM_VM_UID],
            "vm-uid-1"
        );
        assert_eq!(
            provisioned.runtime_descriptor.resource_claims[CLAIM_VM_UID],
            "vm-uid-1"
        );
        // Evidence is bound to the generation and validates.
        provisioned
            .boundary_config
            .provisioning_evidence
            .validate("generation-1")
            .expect("evidence validates");
    }

    #[test]
    fn evidence_rejects_mismatched_generation() {
        let provisioned = spec().provision().expect("provision");
        assert!(
            provisioned
                .boundary_config
                .provisioning_evidence
                .validate("generation-2")
                .is_err()
        );
    }

    #[test]
    fn evidence_rejects_missing_fence() {
        let mut s = spec();
        s.network_policy_uid = String::new();
        assert!(s.provision().is_err(), "empty fence UID must fail closed");
    }

    #[test]
    fn evidence_rejects_missing_resource_version() {
        let mut s = spec();
        s.network_policy_resource_version = String::new();
        assert!(
            s.provision().is_err(),
            "empty fence resourceVersion must fail closed"
        );
    }

    #[test]
    fn evidence_rejects_replaced_fence() {
        // Render the boundary config from fence A and the descriptor from
        // fence B: the descriptor/config consistency check
        // (isolation_protocol.rs:100) must reject the mismatched pair.
        let original = spec().provision().expect("provision");
        let mut replaced = spec();
        replaced.network_policy_uid = "np-uid-2".to_string();
        let replacement = replaced.provision().expect("provision");
        assert!(
            descriptor_matches_config(&original.runtime_descriptor, &replacement.boundary_config)
                .is_err(),
            "a descriptor rendered against fence B must not match a config from fence A"
        );
    }

    #[test]
    fn evidence_rejects_stale_bootstrap_from_previous_generation() {
        let generation_1 = spec().provision().expect("provision");
        let mut s = spec();
        s.generation = "generation-2".to_string();
        let generation_2 = s.provision().expect("provision");
        assert!(
            generation_1
                .boundary_config
                .provisioning_evidence
                .validate("generation-2")
                .is_err()
        );
        assert!(
            descriptor_matches_config(
                &generation_2.runtime_descriptor,
                &generation_1.boundary_config
            )
            .is_err(),
            "a current descriptor must not match a stale previous-generation config"
        );
    }

    #[test]
    fn vm_uid_mismatch_rejected_by_descriptor_config_consistency() {
        let original = spec().provision().expect("provision");
        let mut replaced = spec();
        replaced.vm_uid = "vm-uid-2".to_string();
        replaced.workload_identity = ResolvedWorkloadIdentity::new(
            10001,
            10001,
            Vec::new(),
            "kubevirt-config".to_string(),
            "sandbox:vm-uid-2".to_string(),
        )
        .unwrap();
        let other = replaced.provision().expect("provision");
        assert!(
            descriptor_matches_config(&other.runtime_descriptor, &original.boundary_config)
                .is_err(),
            "a descriptor rendered for vm-uid-2 must not match a config from vm-uid-1"
        );
    }

    #[test]
    fn vm_uid_survives_stop_start_while_generation_advances() {
        // The VM UID is stable across stop/start; only the generation
        // advances. The claims carry the same vm_uid in both renders.
        let first = spec().provision().expect("provision");
        let mut s = spec();
        s.generation = "generation-2".to_string();
        let second = s.provision().expect("provision");
        assert_eq!(
            first.boundary_config.resource_claims[CLAIM_VM_UID],
            second.boundary_config.resource_claims[CLAIM_VM_UID]
        );
        assert_ne!(
            first.boundary_config.generation,
            second.boundary_config.generation
        );
        // Both generations' evidence validate against their own generation.
        first
            .boundary_config
            .provisioning_evidence
            .validate("generation-1")
            .expect("first generation validates");
        second
            .boundary_config
            .provisioning_evidence
            .validate("generation-2")
            .expect("second generation validates");
    }

    #[test]
    fn rendered_config_is_recognized_as_vm_guest() {
        let provisioned = spec().provision().expect("provision");
        assert!(
            openshell_sandbox_backend::boundary_protocol::is_vm_guest(
                &provisioned.boundary_config.resource_claims
            ),
            "the rendered KubeVirt boundary config must satisfy is_vm_guest"
        );
    }

    #[test]
    fn guest_tls_paths_match_staging_constants() {
        let provisioned = spec().provision().expect("provision");
        match &provisioned.boundary_config.listener {
            BoundaryListener::RuntimeInitiatedTlsTcp { tls, .. } => {
                assert_eq!(
                    tls.certificate_chain_path,
                    std::path::Path::new(GUEST_BOUNDARY_CERTIFICATE_PATH)
                );
                assert_eq!(
                    tls.private_key_path,
                    std::path::Path::new(GUEST_BOUNDARY_PRIVATE_KEY_PATH)
                );
            }
            other => panic!("expected RuntimeInitiatedTlsTcp listener, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Virt-launcher egress fence
// ---------------------------------------------------------------------------

use kube::Client;
use kube::api::Api;
use kube::core::DynamicObject;

/// Inputs describing the shared supervisor the fence allows egress to,
/// read from the configured Service at render time.
pub struct SupervisorEndpoint {
    /// Namespace of the shared supervisor Service.
    pub namespace: String,
    /// The Service's `spec.selector` (pod labels), used verbatim as the
    /// egress podSelector. Read from the live Service, never hard-coded.
    pub pod_selector: BTreeMap<String, String>,
    /// The Service's runtime port.
    pub port: u16,
}

/// The fence's observed state: UID, resourceVersion, and the full spec
/// (for drift comparison).
#[derive(Debug, Clone)]
pub struct ObservedFence {
    pub uid: String,
    pub resource_version: String,
    pub spec: serde_json::Value,
}

/// The fence identity and shape the boundary evidence was rendered against.
#[derive(Debug, Clone)]
pub struct ExpectedFence {
    pub uid: String,
    pub resource_version: String,
    pub spec: serde_json::Value,
}

impl ExpectedFence {
    /// Build from a freshly rendered fence JSON and the observed UID/RV.
    #[must_use]
    pub fn from_rendered(
        rendered: serde_json::Value,
        uid: String,
        resource_version: String,
    ) -> Self {
        Self {
            uid,
            resource_version,
            spec: rendered["spec"].clone(),
        }
    }
}

/// Render the virt-launcher fence `NetworkPolicy`.
///
/// The sandbox's only permitted egress is the shared supervisor's runtime
/// port (matched by the supervisor Service's own pod selector, read at
/// render time — never hard-coded, because the Helm chart's supervisor
/// labels vary by release). No DNS, no gateway: the boundary's in-guest
/// DNS relay forwards through the supervisor's network mediation, and the
/// supervisor connection uses driver-resolved addresses.
///
/// Ingress is deny-all (`policyTypes: [Ingress, Egress]` with no ingress
/// rules): the boundary dials out (`RuntimeInitiatedTlsTcp`), so the guest
/// needs no inbound connections. Serial console and virt-handler access go
/// through unix sockets on the node, not pod networking.
///
/// The policy carries the standard `OpenShell` ownership labels so the
/// driver's label-based cleanup and the demo's zero-leftovers check find
/// it.
#[must_use]
pub fn render_fence(
    namespace: &str,
    sandbox_id: &str,
    gateway_id: Option<&str>,
    supervisor: &SupervisorEndpoint,
) -> serde_json::Value {
    let mut labels = BTreeMap::new();
    labels.insert(
        openshell_core::driver_utils::LABEL_MANAGED_BY.to_string(),
        openshell_core::driver_utils::LABEL_MANAGED_BY_VALUE.to_string(),
    );
    labels.insert(
        openshell_core::driver_utils::LABEL_SANDBOX_ID.to_string(),
        sandbox_id.to_string(),
    );
    if let Some(gw_id) = gateway_id {
        labels.insert(
            openshell_core::driver_utils::LABEL_GATEWAY_ID.to_string(),
            gw_id.to_string(),
        );
    }

    let supervisor_selector: serde_json::Map<String, serde_json::Value> = supervisor
        .pod_selector
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
        .collect();

    serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": fence_name(sandbox_id),
            "namespace": namespace,
            "labels": labels,
        },
        "spec": {
            "podSelector": {
                "matchLabels": {
                    "kubevirt.io": "virt-launcher",
                    openshell_core::driver_utils::LABEL_SANDBOX_ID: sandbox_id,
                }
            },
            "policyTypes": ["Ingress", "Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": {
                        "matchLabels": {
                            "kubernetes.io/metadata.name": supervisor.namespace
                        }
                    },
                    "podSelector": {"matchLabels": supervisor_selector}
                }],
                "ports": [{
                    "port": supervisor.port,
                    "protocol": "TCP"
                }]
            }]
        }
    })
}

/// Name of the fence `NetworkPolicy` for one sandbox.
#[must_use]
pub fn fence_name(sandbox_id: &str) -> String {
    format!("openshell-kubevirt-fence-{sandbox_id}")
}

/// Read the fence's current state. `None` when the policy does not exist.
pub async fn observe_fence(
    client: &Client,
    namespace: &str,
    sandbox_id: &str,
) -> Result<Option<ObservedFence>, String> {
    let api: Api<DynamicObject> = Api::namespaced_with(
        client.clone(),
        namespace,
        &kube::api::ApiResource {
            group: "networking.k8s.io".to_string(),
            version: "v1".to_string(),
            api_version: "networking.k8s.io/v1".to_string(),
            kind: "NetworkPolicy".to_string(),
            plural: "networkpolicies".to_string(),
        },
    );
    let name = fence_name(sandbox_id);
    match api.get(&name).await {
        Ok(obj) => Ok(Some(ObservedFence {
            uid: obj.metadata.uid.unwrap_or_default(),
            resource_version: obj.metadata.resource_version.unwrap_or_default(),
            spec: obj
                .data
                .get("spec")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        })),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// Pure fence verification (unit-testable without a cluster):
/// - deleted (None) → Err;
/// - UID changed (replaced policy) → Err;
/// - spec drift (podSelector, policyTypes, egress rules) → Err;
/// - resourceVersion-only change (a label or annotation from a controller)
///   → Ok with the new RV, so the driver re-captures it into the new
///   generation's evidence.
pub fn check_fence(
    observed: Option<&ObservedFence>,
    expected: &ExpectedFence,
    sandbox_id: &str,
) -> Result<ObservedFence, String> {
    let Some(observed) = observed else {
        return Err(format!(
            "the virt-launcher egress fence {} was deleted; the boundary \
             evidence was rendered against a fence that no longer exists",
            fence_name(sandbox_id)
        ));
    };
    if observed.uid != expected.uid {
        return Err(format!(
            "the virt-launcher egress fence was replaced (expected UID {}, got {}); \
             the boundary evidence was rendered against the old fence and cannot \
             be reused",
            expected.uid, observed.uid
        ));
    }
    if observed.spec != expected.spec {
        return Err(
            "the virt-launcher egress fence spec drifted from the rendered shape \
             (podSelector, policyTypes, or egress rules changed); the fence no \
             longer matches what the boundary evidence was rendered against"
                .to_string(),
        );
    }
    Ok(observed.clone())
}

/// Verify the fence on every new generation.
///
/// The async wrapper around [`check_fence`]. Returns the current
/// `ObservedFence` so the caller can re-capture the resourceVersion into
/// the new generation's evidence.
pub async fn verify_fence(
    client: &Client,
    namespace: &str,
    sandbox_id: &str,
    expected: &ExpectedFence,
) -> Result<ObservedFence, String> {
    let observed = observe_fence(client, namespace, sandbox_id).await?;
    check_fence(observed.as_ref(), expected, sandbox_id)
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    fn supervisor() -> SupervisorEndpoint {
        SupervisorEndpoint {
            namespace: "openshell".to_string(),
            pod_selector: BTreeMap::from([
                (
                    "app.kubernetes.io/name".to_string(),
                    "openshell-supervisor".to_string(),
                ),
                (
                    "app.kubernetes.io/component".to_string(),
                    "supervisor".to_string(),
                ),
            ]),
            port: 5500,
        }
    }

    fn rendered_fence() -> serde_json::Value {
        render_fence("kvpoc-sandboxes", "sbx-1", Some("gw"), &supervisor())
    }

    fn observed(uid: &str, rv: &str, spec: serde_json::Value) -> ObservedFence {
        ObservedFence {
            uid: uid.to_string(),
            resource_version: rv.to_string(),
            spec,
        }
    }

    #[test]
    fn fence_renders_deny_all_ingress_and_supervisor_only_egress() {
        let fence = rendered_fence();
        let policy_types = fence["spec"]["policyTypes"].as_array().unwrap();
        assert!(policy_types.contains(&serde_json::json!("Ingress")));
        assert!(policy_types.contains(&serde_json::json!("Egress")));
        // No ingress rules: deny-all ingress.
        assert!(fence["spec"]["ingress"].is_null());
        // The podSelector matches the virt-launcher pod.
        assert_eq!(
            fence["spec"]["podSelector"]["matchLabels"]["kubevirt.io"],
            "virt-launcher"
        );
        assert_eq!(
            fence["spec"]["podSelector"]["matchLabels"]["openshell.ai/sandbox-id"],
            "sbx-1"
        );
        // Egress allows only the supervisor's port, matched by the Service
        // selector (not hard-coded labels).
        assert_eq!(fence["spec"]["egress"][0]["ports"][0]["port"], 5500);
        assert_eq!(
            fence["spec"]["egress"][0]["to"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/name"],
            "openshell-supervisor"
        );
        assert_eq!(
            fence["spec"]["egress"][0]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "openshell"
        );
    }

    #[test]
    fn fence_carries_openshell_labels_for_cleanup() {
        let fence = rendered_fence();
        assert_eq!(
            fence["metadata"]["labels"]["openshell.ai/managed-by"],
            "openshell"
        );
        assert_eq!(
            fence["metadata"]["labels"]["openshell.ai/sandbox-id"],
            "sbx-1"
        );
        assert_eq!(fence["metadata"]["labels"]["openshell.ai/gateway-id"], "gw");
    }

    #[test]
    fn fence_name_is_derived_from_sandbox_id() {
        assert_eq!(fence_name("sbx-1"), "openshell-kubevirt-fence-sbx-1");
    }

    #[test]
    fn check_fence_rejects_deleted() {
        let expected = ExpectedFence::from_rendered(rendered_fence(), "uid-1".into(), "1".into());
        let err = check_fence(None, &expected, "sbx-1").unwrap_err();
        assert!(err.contains("was deleted"));
    }

    #[test]
    fn check_fence_rejects_replaced_uid() {
        let expected = ExpectedFence::from_rendered(rendered_fence(), "uid-1".into(), "1".into());
        let replaced = observed("uid-2", "1", rendered_fence()["spec"].clone());
        let err = check_fence(Some(&replaced), &expected, "sbx-1").unwrap_err();
        assert!(err.contains("was replaced"));
    }

    #[test]
    fn check_fence_rejects_spec_drift() {
        let expected = ExpectedFence::from_rendered(rendered_fence(), "uid-1".into(), "1".into());
        let mut drifted = rendered_fence();
        drifted["spec"]["egress"][0]["ports"][0]["port"] = serde_json::json!(9999);
        let observed_drift = observed("uid-1", "1", drifted["spec"].clone());
        let err = check_fence(Some(&observed_drift), &expected, "sbx-1").unwrap_err();
        assert!(err.contains("spec drifted"));
    }

    #[test]
    fn check_fence_accepts_rv_only_change() {
        let expected = ExpectedFence::from_rendered(rendered_fence(), "uid-1".into(), "1".into());
        let rv_changed = observed("uid-1", "42", rendered_fence()["spec"].clone());
        let result =
            check_fence(Some(&rv_changed), &expected, "sbx-1").expect("RV-only change must pass");
        assert_eq!(result.resource_version, "42");
    }

    #[test]
    fn check_fence_accepts_unchanged() {
        let expected = ExpectedFence::from_rendered(rendered_fence(), "uid-1".into(), "1".into());
        let unchanged = observed("uid-1", "1", rendered_fence()["spec"].clone());
        check_fence(Some(&unchanged), &expected, "sbx-1").expect("unchanged must pass");
    }
}

// ---------------------------------------------------------------------------
// Cloud-init delivery
// ---------------------------------------------------------------------------

use base64::Engine as _;

/// Keys in the cloudInitNoCloud `userData` payload.
///
/// Carrying the boundary bootstrap artifacts (base64-encoded so the
/// shell-sourceable format is preserved). No gateway material appears in
/// the payload — no sandbox token, no gateway endpoint, no JWT.
pub const PAYLOAD_BOUNDARY_CONFIG: &str = "OPENSHELL_BOUNDARY_CONFIG_B64";
pub const PAYLOAD_BOUNDARY_CERTIFICATE: &str = "OPENSHELL_BOUNDARY_CERTIFICATE_B64";
pub const PAYLOAD_BOUNDARY_PRIVATE_KEY: &str = "OPENSHELL_BOUNDARY_PRIVATE_KEY_B64";
/// The workload uid/gid the boundary runs as (launch-capability-free args).
pub const PAYLOAD_WORKLOAD_UID: &str = "OPENSHELL_WORKLOAD_UID";
pub const PAYLOAD_WORKLOAD_GID: &str = "OPENSHELL_WORKLOAD_GID";

/// Render the cloudInitNoCloud `userData` payload.
///
/// Delivers the boundary bootstrap artifacts to the guest. The payload is
/// shell-sourceable; the guest init decodes the base64 values, stages
/// the files at the paths written in the `BoundaryConfig`, and runs
/// `openshell-sandbox launch-capability-free <uid> <gid> <config-path>`.
///
/// Assert (checkpoint 3): no gateway endpoint, no sandbox token, no JWT.
#[must_use]
pub fn render_guest_payload(
    artifacts: &crate::sandbox_runtime::BoundaryBootstrapArtifacts,
    uid: u32,
    gid: u32,
) -> String {
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    format!(
        "{PAYLOAD_BOUNDARY_CONFIG}={}\n\
         {PAYLOAD_BOUNDARY_CERTIFICATE}={}\n\
         {PAYLOAD_BOUNDARY_PRIVATE_KEY}={}\n\
         {PAYLOAD_WORKLOAD_UID}={uid}\n\
         {PAYLOAD_WORKLOAD_GID}={gid}\n",
        b64(&artifacts.config_json),
        b64(&artifacts.certificate_pem),
        b64(&artifacts.private_key_pem),
    )
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn guest_payload_carries_no_gateway_material() {
        let artifacts = crate::sandbox_runtime::BoundaryBootstrapArtifacts {
            config_json: br#"{"boundary_id":"sbx","generation":"g1"}"#.to_vec(),
            certificate_pem: b"-----BEGIN CERTIFICATE-----".to_vec(),
            private_key_pem: b"-----BEGIN PRIVATE KEY-----".to_vec(),
        };
        let payload = render_guest_payload(&artifacts, 10001, 10001);
        // No gateway material (checkpoint 3).
        assert!(!payload.contains("OPENSHELL_ENDPOINT"));
        assert!(!payload.contains("OPENSHELL_SANDBOX_TOKEN"));
        assert!(!payload.contains("OPENSHELL_SANDBOX_TOKEN_FILE"));
        assert!(!payload.contains("OPENSHELL_ENDPOINT="));
        assert!(!payload.contains("grpc_endpoint"));
        // The boundary artifacts and identity are present.
        assert!(payload.contains(PAYLOAD_BOUNDARY_CONFIG));
        assert!(payload.contains(PAYLOAD_BOUNDARY_CERTIFICATE));
        assert!(payload.contains(PAYLOAD_BOUNDARY_PRIVATE_KEY));
        assert!(payload.contains("OPENSHELL_WORKLOAD_UID=10001"));
        assert!(payload.contains("OPENSHELL_WORKLOAD_GID=10001"));
    }

    #[test]
    fn guest_payload_is_shell_sourceable() {
        let artifacts = crate::sandbox_runtime::BoundaryBootstrapArtifacts {
            config_json: b"{}".to_vec(),
            certificate_pem: b"cert".to_vec(),
            private_key_pem: b"key".to_vec(),
        };
        let payload = render_guest_payload(&artifacts, 1000, 1000);
        // Every line is KEY=VALUE (base64 has no spaces or shell metachars).
        for line in payload.lines() {
            assert!(line.contains('='), "line must be KEY=VALUE: {line}");
            let value = line.split_once('=').unwrap().1;
            assert!(
                !value.contains(' ') && !value.contains('\'') && !value.contains('"'),
                "value must be shell-safe: {value}"
            );
        }
    }
}
