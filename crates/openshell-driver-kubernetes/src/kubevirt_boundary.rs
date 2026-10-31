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
//! Who verifies the fence: the provisioning evidence is a SHA-256 digest
//! binding the generation to the encoded native evidence
//! (`ProvisioningEvidence::from_enforcement_evidence`,
//! `crates/openshell-isolation-interface/src/contract.rs:233`). The
//! boundary rejects a bootstrap whose evidence belongs to a different
//! generation (`contract.rs:254 validate`). The driver re-reads the
//! `NetworkPolicy` UID on every start and fails the start when the fence
//! was deleted or replaced (see `verify_fence`).

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

/// Resource-claim keys.
///
/// The naming of the generation marker is pending a user decision (the
/// boundary and supervisor key VM-guest policy semantics on the literal
/// `vm.generation` in two places); keeping them as constants makes either
/// answer a one-line change.
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
    /// Route handle for a supervisor shared by several sandbox instances;
    /// `None` for a dedicated supervisor.
    pub route: Option<String>,
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
                    route: self.route.clone(),
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
                    route: self.route.unwrap_or_default(),
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
            route: None,
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
        // A different NetworkPolicy UID produces different evidence; the
        // old bootstrap's evidence no longer validates against the new
        // render (the digest changes).
        let original = spec().provision().expect("provision");
        let mut replaced = spec();
        replaced.network_policy_uid = "np-uid-2".to_string();
        let replacement = replaced.provision().expect("provision");
        assert_ne!(
            original
                .boundary_config
                .provisioning_evidence
                .evidence_digest,
            replacement
                .boundary_config
                .provisioning_evidence
                .evidence_digest,
            "a replaced fence must change the evidence digest"
        );
    }

    #[test]
    fn evidence_rejects_stale_bootstrap_from_previous_generation() {
        let generation_1 = spec().provision().expect("provision");
        let mut s = spec();
        s.generation = "generation-2".to_string();
        let generation_2 = s.provision().expect("provision");
        // A stale bootstrap from generation 1 presented after generation 2:
        // its evidence is bound to generation-1 and must not validate
        // against generation-2's expected generation.
        assert!(
            generation_1
                .boundary_config
                .provisioning_evidence
                .validate("generation-2")
                .is_err()
        );
        assert!(
            generation_2
                .boundary_config
                .provisioning_evidence
                .validate("generation-1")
                .is_err()
        );
    }

    #[test]
    fn vm_uid_mismatch_produces_a_different_resource_digest() {
        // A boundary config rendered for a different VM (different vm_uid in
        // the workload identity's resource digest) cannot be replayed for
        // this VM: the digest binding differs, so the supervisor's identity
        // confirmation (which checks the descriptor's workload identity
        // against the boundary's) rejects the mismatch.
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
        assert_ne!(
            original.boundary_config.workload_identity.resource_digest,
            other.boundary_config.workload_identity.resource_digest,
            "a different vm_uid must change the identity's resource digest"
        );
        assert_ne!(
            original.boundary_config.resource_claims[CLAIM_VM_UID],
            other.boundary_config.resource_claims[CLAIM_VM_UID]
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
