// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `KubeVirt` runtime mode (proof of concept).
//!
//! When `[openshell.drivers.kubernetes] runtime = "kubevirt"` is configured,
//! the driver provisions sandboxes as `kubevirt.io/v1` `VirtualMachine`
//! objects instead of `agents.x-k8s.io` Sandbox CRs. The `KubeVirt` controller
//! (VM -> VMI -> virt-launcher pod) plays the role the Agent Sandbox
//! controller plays in the pod runtime.
//!
//! Resource model per sandbox:
//! - one `VirtualMachine` carrying the standard `OpenShell` ownership labels
//!   (the durable lifecycle handle; `runStrategy: Once` mirrors the pod
//!   path's `restartPolicy: Never` — supervisor exit is terminal),
//! - one bootstrap `Secret` referenced through a `cloudInitNoCloud` volume
//!   (transport only — the guest init parses the ISO itself, no cloud-init
//!   software required). It carries the supervisor environment plus the
//!   gateway-minted sandbox JWT from `DriverSandboxSpec.sandbox_token`, the
//!   same pre-minted-token model the libkrun VM driver uses,
//! - one workspace `PersistentVolumeClaim` mounted by the guest init at
//!   `/sandbox` (mirrors the pod path's PVC-backed workspace).
//!
//! What the `PoC` deliberately skips: GPU, managed/operator workspace modes,
//! mTLS to the gateway, `NetworkPolicy` coverage for virt-launcher pods,
//! live-migration/VMI-replacement semantics, and arbitrary OCI sandbox
//! images as VM disks (the guest image is a purpose-built containerDisk).

use std::collections::BTreeMap;

use std::time::Duration;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, Secret, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{
    Api, ApiResource, DeleteParams, ListParams, ObjectList, Patch, PatchParams, Preconditions,
};
use kube::core::DynamicObject;
use kube::{Client, Error as KubeError};
use openshell_core::driver_utils::{
    LABEL_MANAGED_BY, LABEL_MANAGED_BY_VALUE, LABEL_SANDBOX_ID, LABEL_SANDBOX_NAME,
    LABEL_SANDBOX_WORKSPACE,
};
use openshell_core::proto::compute::v1::{
    DriverCondition, DriverSandbox as Sandbox, DriverSandboxStatus as SandboxStatus,
};
use openshell_core::sandbox_env;

use crate::KubernetesDriverError;
use crate::config::{KubernetesComputeConfig, KubernetesRuntimeMode, WorkspaceMode};
use crate::driver::KUBE_API_TIMEOUT;
use crate::driver::{sandbox_annotations, sandbox_labels};

pub const KUBEVIRT_GROUP: &str = "kubevirt.io";
pub const KUBEVIRT_VERSION: &str = "v1";
const KUBEVIRT_VM_PLURAL: &str = "virtualmachines";
const KUBEVIRT_RUN_STRATEGY: &str = "Once";

/// Kernel and initramfs locations inside the purpose-built guest image
/// (see `e2e/kubevirt/` for the image build).
const KUBEVIRT_GUEST_KERNEL_PATH: &str = "/boot/vmlinuz-virt";
const KUBEVIRT_GUEST_INITRD_PATH: &str = "/boot/initramfs-virt";
const KUBEVIRT_KERNEL_ARGS: &str =
    "console=ttyS0 root=/dev/vda rw rootfstype=ext4 modules=ext4 init=/opt/openshell/init";

/// Condition reason set on `Ready=False` while the VM is booting. Matches the
/// gateway's transient-reason vocabulary so the sandbox stays `Provisioning`
/// rather than `Error`.
const REASON_VM_NOT_READY: &str = "Starting";

/// Non-transient condition reason: the gateway's phase mapper maps this to
/// the public `Error` phase rather than leaving the sandbox provisioning.
const REASON_VM_FAILED: &str = "VMFailed";

/// Classify a terminal VM failure from the printable status and the paired
/// VMI's phase. Returns a human-readable summary when the VM has failed.
///
/// Terminal printable statuses (per `KubeVirt` v1.9's
/// `VirtualMachinePrintableStatus`): `CrashLoopBackOff`,
/// `ErrorUnschedulable`, `ErrorPvcNotFound`, and `DataVolumeError` mean the
/// VM cannot run and will not recover on its own.
///
/// Deliberately **not** terminal:
/// - `Unknown` (the controller could not obtain the VM state) can appear
///   transiently during apiserver/communication hiccups; mapping it to
///   `Error` would flip a booting sandbox to `Error` on a blip.
/// - `ErrImagePull` / `ImagePullBackOff`: kubelet retries the pull, so they
///   stay transient — but the condition message carries the printable
///   status so the cause is visible while the sandbox provisions.
/// - A `Failed` or `Succeeded` VMI under a not-ready VM **is** terminal: the
///   guest exited (kernel panic, supervisor death, or a completed `Once`
///   run). Without the VMI signal those states are indistinguishable from
///   a not-yet-booted VM, whose printable status is also `Stopped`.
pub fn terminal_vm_failure(
    printable_status: &str,
    vmi_phase: Option<&str>,
    vm_ready: bool,
) -> Option<String> {
    match printable_status {
        "CrashLoopBackOff" | "ErrorUnschedulable" | "ErrorPvcNotFound" | "DataVolumeError" => {
            return Some(format!("printableStatus {printable_status}"));
        }
        _ => {}
    }
    if !vm_ready {
        match vmi_phase {
            Some("Failed") => return Some("VirtualMachineInstance phase Failed".to_string()),
            Some("Succeeded") => {
                return Some("VirtualMachineInstance phase Succeeded (guest exited)".to_string());
            }
            _ => {}
        }
    }
    None
}

/// Namespaced API for `kubevirt.io/v1` `virtualmachines`.
pub fn virtual_machine_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client.clone(), namespace, &virtual_machine_api_resource())
}

pub fn virtual_machine_api_resource() -> ApiResource {
    ApiResource {
        group: KUBEVIRT_GROUP.to_string(),
        version: KUBEVIRT_VERSION.to_string(),
        api_version: format!("{KUBEVIRT_GROUP}/{KUBEVIRT_VERSION}"),
        kind: "VirtualMachine".to_string(),
        plural: KUBEVIRT_VM_PLURAL.to_string(),
    }
}

/// Check that the cluster serves `kubevirt.io/v1` virtualmachines.
pub async fn kubevirt_available(client: &Client, namespace: &str) -> Result<(), String> {
    let api = virtual_machine_api(client, namespace);
    match api.list(&ListParams::default().limit(1)).await {
        Ok(_) => Ok(()),
        Err(KubeError::Api(err)) if err.code == 404 => Err(format!(
            "kubevirt.io/{KUBEVIRT_VERSION} virtualmachines is not served by this cluster; \
             install KubeVirt to use runtime = \"kubevirt\""
        )),
        Err(err) => Err(err.to_string()),
    }
}

/// Everything the renderer needs for one sandbox.
pub struct KubeVirtSandboxParams<'a> {
    pub sandbox: &'a Sandbox,
    pub config: &'a KubernetesComputeConfig,
    pub namespace: &'a str,
    /// `KubeVirt` resource name (`{workspace}--{name}` in shared mode).
    pub kube_name: &'a str,
}

/// Render the bootstrap Secret consumed by the guest init through the
/// `cloudInitNoCloud` volume. `user-data` is a shell-sourceable `KEY=VALUE`
/// file; the guest init writes the token to a 0600 file and unsets it before
/// exec'ing the supervisor.
pub fn render_bootstrap_secret(params: &KubeVirtSandboxParams<'_>) -> Secret {
    let sandbox = params.sandbox;
    let spec = sandbox.spec.as_ref();
    let mut lines = Vec::new();

    let mut push = |key: &str, value: &str| {
        lines.push(format!("{key}={}", shell_quote(value)));
    };

    push(sandbox_env::ENDPOINT, &params.config.grpc_endpoint);
    push(sandbox_env::SANDBOX_ID, &sandbox.id);
    push(sandbox_env::SANDBOX, &sandbox.name);
    // The supervisor runs its own SSH server on this in-guest unix socket;
    // providing the path is what enables the readiness-bearing supervisor
    // session (same value as the libkrun VM driver uses).
    push(
        sandbox_env::SSH_SOCKET_PATH,
        openshell_core::container_paths::SSH_SOCKET_PATH,
    );
    push(
        sandbox_env::SANDBOX_TOKEN_FILE,
        openshell_core::container_paths::VM_GUEST_SANDBOX_TOKEN_PATH,
    );
    // Pre-minted gateway JWT — same delivery model as the libkrun VM driver.
    if let Some(token) = spec.and_then(|spec| {
        let token = spec.sandbox_token.as_str();
        (!token.is_empty()).then_some(token)
    }) {
        push(sandbox_env::SANDBOX_TOKEN, token);
    }
    let main_process =
        sandbox_env::MainProcessConfig::encode_driver_spec_base64url(spec).unwrap_or_default();
    push(sandbox_env::MAIN_PROCESS_SPEC, &main_process);
    push(
        sandbox_env::LOG_LEVEL,
        &openshell_core::driver_utils::sandbox_log_level(sandbox, "info"),
    );
    push(
        sandbox_env::TELEMETRY_ENABLED,
        openshell_core::telemetry::enabled_env_value(),
    );
    // Runtime capabilities are driver-owned; this PoC provides no policy DNS
    // or transparent TCP substrate inside the guest.
    push(sandbox_env::NETWORK_RUNTIME_CAPABILITIES, "");
    let user_env = spec
        .map(|spec| spec.environment.clone())
        .unwrap_or_default();
    if !user_env.is_empty()
        && let Ok(json) = serde_json::to_string(&user_env)
    {
        push(sandbox_env::USER_ENVIRONMENT, &json);
    }

    let user_data = lines.join("\n") + "\n";

    Secret {
        metadata: ObjectMeta {
            name: Some(bootstrap_secret_name(params.kube_name)),
            namespace: Some(params.namespace.to_string()),
            labels: Some(sandbox_labels(sandbox, Some(&params.config.gateway_id))),
            annotations: Some(sandbox_annotations(sandbox)),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([("userData".to_string(), user_data)])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// Render the workspace PVC. The guest init formats it on first boot (sentinel
/// guarded, same pattern as the pod path's init-container seeding) and mounts
/// it at `/sandbox`.
pub fn render_workspace_pvc(params: &KubeVirtSandboxParams<'_>) -> PersistentVolumeClaim {
    let storage_class = (!params.config.workspace_storage_class.is_empty())
        .then(|| params.config.workspace_storage_class.clone());
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(workspace_pvc_name(params.kube_name)),
            namespace: Some(params.namespace.to_string()),
            labels: Some(sandbox_labels(
                params.sandbox,
                Some(&params.config.gateway_id),
            )),
            annotations: Some(sandbox_annotations(params.sandbox)),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(params.config.workspace_default_storage_size.clone()),
                )])),
                ..Default::default()
            }),
            storage_class_name: storage_class,
            ..Default::default()
        }),
        status: None,
    }
}

/// Render the `VirtualMachine`.
pub fn render_virtual_machine(params: &KubeVirtSandboxParams<'_>) -> DynamicObject {
    let sandbox = params.sandbox;
    let kubevirt = &params.config.kubevirt;
    let mut labels = sandbox_labels(sandbox, Some(&params.config.gateway_id));
    let annotations = sandbox_annotations(sandbox);

    let mut container_disk = serde_json::json!({
        "image": kubevirt.guest_image,
    });
    if !kubevirt.guest_image_pull_policy.is_empty() {
        container_disk["imagePullPolicy"] = serde_json::json!(kubevirt.guest_image_pull_policy);
    }

    let disks = vec![
        serde_json::json!({"name": "rootdisk", "disk": {"bus": "virtio"}}),
        serde_json::json!({"name": "cloudinit", "disk": {"bus": "virtio"}}),
    ];
    let mut volumes = vec![
        serde_json::json!({"name": "rootdisk", "containerDisk": container_disk}),
        serde_json::json!({
            "name": "cloudinit",
            "cloudInitNoCloud": {
                "secretRef": {"name": bootstrap_secret_name(params.kube_name)}
            }
        }),
    ];
    // The workspace PVC is a filesystem volume (the cluster's default
    // storage class provisions filesystem PVCs), which KubeVirt shares
    // with the guest via virtio-fs rather than as a block disk.
    let mut filesystems = Vec::new();
    if kubevirt.workspace_pvc {
        filesystems.push(serde_json::json!({
            "name": "workspace",
            "virtiofs": {}
        }));
        volumes.push(serde_json::json!({
            "name": "workspace",
            "persistentVolumeClaim": {"claimName": workspace_pvc_name(params.kube_name)}
        }));
    }

    // VMI inherits template labels; keeping the OpenShell ownership labels
    // there makes VMI-scoped tooling (describe, events) correlate as well.
    labels.insert(
        KUBEVIRT_VM_LABEL.to_string(),
        KUBEVIRT_VM_LABEL_VALUE.to_string(),
    );
    // The boundary-role workload label: KubeVirt propagates template labels
    // to the virt-launcher pod, and the shared supervisor's ingress
    // NetworkPolicy (deploy/helm/openshell/templates/
    // supervisor-networkpolicy.yaml) admits connections only from pods
    // carrying this label.
    labels.insert(
        crate::isolation::BOUNDARY_ROLE_LABEL.to_string(),
        "workload".to_string(),
    );

    // Direct kernel boot: the guest image is purpose-built (see
    // `e2e/kubevirt/`), so no bootloader is installed in the rootfs. The
    // kernel and initramfs live next to the rootfs disk inside the same
    // containerDisk image.
    let mut kernel_boot_container = serde_json::json!({
        "image": kubevirt.guest_image,
        "kernelPath": KUBEVIRT_GUEST_KERNEL_PATH,
        "initrdPath": KUBEVIRT_GUEST_INITRD_PATH,
    });
    if !kubevirt.guest_image_pull_policy.is_empty() {
        kernel_boot_container["imagePullPolicy"] =
            serde_json::json!(kubevirt.guest_image_pull_policy);
    }

    let data = serde_json::json!({
        "spec": {
            "runStrategy": KUBEVIRT_RUN_STRATEGY,
            "template": {
                "metadata": {"labels": labels},
                "spec": {
                    "domain": {
                        "firmware": {
                            "kernelBoot": {
                                "container": kernel_boot_container,
                                "kernelArgs": KUBEVIRT_KERNEL_ARGS
                            }
                        },
                        "cpu": {"cores": kubevirt.cpu_cores},
                        "resources": {"requests": {"memory": kubevirt.memory}},
                        "devices": {
                            "disks": disks,
                            "filesystems": filesystems,
                            "interfaces": [{"name": "default", "masquerade": {}}]
                        }
                    },
                    "volumes": volumes,
                    "networks": [{"name": "default", "pod": {}}]
                }
            }
        }
    });

    let mut obj = DynamicObject::new(params.kube_name, &virtual_machine_api_resource());
    obj.metadata = ObjectMeta {
        name: Some(params.kube_name.to_string()),
        namespace: Some(params.namespace.to_string()),
        labels: Some(sandbox_labels(sandbox, Some(&params.config.gateway_id))),
        annotations: Some(annotations),
        ..Default::default()
    };
    obj.data = data;
    obj
}

/// Label set on the VM template so the VMI (and virt-launcher pod) carry the
/// `OpenShell` ownership markers.
const KUBEVIRT_VM_LABEL: &str = "kubevirt.io/domain";
const KUBEVIRT_VM_LABEL_VALUE: &str = "openshell";

pub fn bootstrap_secret_name(kube_name: &str) -> String {
    format!("{kube_name}-bootstrap")
}

pub fn workspace_pvc_name(kube_name: &str) -> String {
    format!("{kube_name}-workspace")
}

/// Map a `VirtualMachine` object to a driver sandbox snapshot without VMI
/// phase context. Production paths join the VMI phase (see
/// [`sandbox_from_vm_object_with_vmi`]); this convenience wrapper exists for
/// callers with only a VM object at hand.
#[cfg(test)]
fn sandbox_from_vm_object(
    namespace: &str,
    obj: DynamicObject,
) -> Result<(String, Sandbox), String> {
    sandbox_from_vm_object_with_vmi(namespace, obj, None)
}

/// Map a `VirtualMachine` object, joined with its paired VMI's phase, to a
/// driver sandbox snapshot.
///
/// Backend observations only: the gateway still composes public `Ready` from
/// the supervisor relay session, exactly as with Sandbox CRs. `vmi_phase` is
/// the phase of the paired `VirtualMachineInstance` when known (the VMI
/// carries the same labels); it distinguishes a failed or completed guest
/// from one that has not booted yet.
pub fn sandbox_from_vm_object_with_vmi(
    namespace: &str,
    obj: DynamicObject,
    vmi_phase: Option<&str>,
) -> Result<(String, Sandbox), String> {
    let kube_name = obj.metadata.name.clone().unwrap_or_default();

    if !is_openshell_managed(&obj) {
        return Err(format!("object {kube_name} not managed by openshell"));
    }
    let Some(id) = annotation_or_label(&obj, LABEL_SANDBOX_ID) else {
        return Err(format!("object {kube_name} missing sandbox id"));
    };
    let Some(name) = annotation_or_label(&obj, LABEL_SANDBOX_NAME) else {
        return Err(format!("object {kube_name} missing sandbox name"));
    };
    let Some(workspace) = annotation_or_label(&obj, LABEL_SANDBOX_WORKSPACE) else {
        return Err(format!("object {kube_name} missing sandbox workspace"));
    };

    let namespace = obj
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| namespace.to_string());
    let status = vm_status_from_object(&obj, &name, vmi_phase);

    Ok((
        kube_name,
        Sandbox {
            id,
            name,
            namespace,
            spec: None,
            status,
            workspace,
        },
    ))
}

/// Translate VM status into driver conditions:
/// - `status.ready == true` becomes a backend `Ready=True` observation.
///   Without a connected supervisor session the gateway still reports
///   `Provisioning` ("`SupervisorNotConnected`") — the central readiness
///   invariant.
/// - `runStrategy: Halted` becomes `Suspended=True` (public `Stopped`).
/// - a terminal VMI phase (`Failed`/`Succeeded` while the VM is not ready) or
///   a terminal printable status (e.g. `CrashLoopBackOff`)
///   becomes `Ready=False` with the non-transient `VMFailed` reason so the
///   gateway surfaces the sandbox as `Error` instead of provisioning forever.
/// - a deletion timestamp becomes `deleting`.
fn vm_status_from_object(
    obj: &DynamicObject,
    sandbox_name: &str,
    vmi_phase: Option<&str>,
) -> Option<SandboxStatus> {
    let status = obj.data.get("status")?;
    let status_obj = status.as_object()?;

    let ready = status_obj
        .get("ready")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // Only an explicit `runStrategy: Halted` maps to `Suspended`. A fresh VM
    // (or one whose `Once` VMI exited) also reports `printableStatus:
    // Stopped`; treating that as Suspended would flip a provisioning sandbox
    // to public `Stopped`.
    let halted = obj
        .data
        .get("spec")
        .and_then(|spec| spec.get("runStrategy"))
        .and_then(serde_json::Value::as_str)
        == Some("Halted");
    let stopped = halted;

    let printable = status_obj
        .get("printableStatus")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let terminal = terminal_vm_failure(printable, vmi_phase, ready);

    let mut conditions = Vec::new();
    conditions.push(DriverCondition {
        r#type: "Ready".to_string(),
        status: if ready { "True" } else { "False" }.to_string(),
        reason: if ready {
            "VMReady".to_string()
        } else if terminal.is_some() {
            REASON_VM_FAILED.to_string()
        } else {
            REASON_VM_NOT_READY.to_string()
        },
        message: if ready {
            "VirtualMachineInstance is running".to_string()
        } else if let Some(terminal) = terminal {
            format!("VirtualMachine failed: {terminal}")
        } else {
            format!("VirtualMachine status: {printable}")
        },
        transition_time: None,
    });
    if stopped {
        conditions.push(DriverCondition {
            r#type: "Suspended".to_string(),
            status: "True".to_string(),
            reason: "VMHalted".to_string(),
            message: "VirtualMachine runStrategy is Halted".to_string(),
            transition_time: None,
        });
    }

    Some(SandboxStatus {
        name: sandbox_name.to_string(),
        // The VMI is created with the same name as the VM.
        instance_id: obj.metadata.name.clone().unwrap_or_default(),
        agent_fd: String::new(),
        sandbox_fd: String::new(),
        conditions,
        deleting: obj.metadata.deletion_timestamp.is_some(),
        ..Default::default()
    })
}

fn is_openshell_managed(obj: &DynamicObject) -> bool {
    obj.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(LABEL_MANAGED_BY))
        .or_else(|| {
            obj.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_MANAGED_BY))
        })
        .map(String::as_str)
        == Some(LABEL_MANAGED_BY_VALUE)
}

fn annotation_or_label(obj: &DynamicObject, key: &str) -> Option<String> {
    obj.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(key))
        .or_else(|| obj.metadata.labels.as_ref().and_then(|l| l.get(key)))
        .cloned()
}

/// Single-quote a value for the shell-sourceable user-data file.
fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/:+=@".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// True when the driver is configured for the `KubeVirt` runtime.
pub fn is_kubevirt_mode(config: &KubernetesComputeConfig) -> bool {
    config.runtime == KubernetesRuntimeMode::KubeVirt
}

/// Shared-mode namespace the VM resources render into.
pub fn kubevirt_namespace(config: &KubernetesComputeConfig) -> Result<String, String> {
    if config.workspace_mode != WorkspaceMode::Shared {
        return Err(format!(
            "kubevirt runtime (PoC) supports only the shared workspace mode; configured mode is {}",
            config.workspace_mode
        ));
    }
    Ok(config.namespace.clone())
}

/// List driver-owned VMs by label selector.
pub async fn list_virtual_machines(
    client: &Client,
    config: &KubernetesComputeConfig,
    selector: &str,
) -> Result<ObjectList<DynamicObject>, KubernetesDriverError> {
    let namespace = kubevirt_namespace(config).map_err(KubernetesDriverError::Message)?;
    let api = virtual_machine_api(client, &namespace);
    api.list(&ListParams::default().labels(selector))
        .await
        .map_err(KubernetesDriverError::from_kube)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KubernetesComputeConfig;
    use openshell_core::proto::compute::v1::DriverSandbox as Sandbox;

    fn sandbox() -> Sandbox {
        Sandbox {
            id: "sbx-id".to_string(),
            name: "demo".to_string(),
            namespace: "kvpoc-sandboxes".to_string(),
            spec: None,
            status: None,
            workspace: "default".to_string(),
        }
    }

    fn config() -> KubernetesComputeConfig {
        KubernetesComputeConfig {
            runtime: KubernetesRuntimeMode::KubeVirt,
            namespace: "kvpoc-sandboxes".to_string(),
            gateway_id: "kvpoc".to_string(),
            grpc_endpoint: "http://172.18.0.1:18085/".to_string(),
            kubevirt: crate::KubeVirtConfig {
                guest_image: "kvpoc/openshell-sandbox-vm:dev".to_string(),
                guest_image_pull_policy: "IfNotPresent".to_string(),
                cpu_cores: 2,
                memory: "2Gi".to_string(),
                workspace_pvc: true,
            },
            ..KubernetesComputeConfig::default()
        }
    }

    fn params<'a>(
        sandbox: &'a Sandbox,
        config: &'a KubernetesComputeConfig,
    ) -> KubeVirtSandboxParams<'a> {
        KubeVirtSandboxParams {
            sandbox,
            config,
            namespace: "kvpoc-sandboxes",
            kube_name: "default--demo",
        }
    }

    #[test]
    fn bootstrap_secret_carries_supervisor_environment() {
        let mut sbx = sandbox();
        sbx.spec = Some(openshell_core::proto::compute::v1::DriverSandboxSpec {
            sandbox_token: "jwt-value".to_string(),
            ..Default::default()
        });
        let cfg = config();
        let secret = render_bootstrap_secret(&params(&sbx, &cfg));

        let data = secret
            .string_data
            .expect("string data")
            .remove("userData")
            .expect("userData key uses KubeVirt cloudInitNoCloud convention");
        assert!(data.contains("OPENSHELL_ENDPOINT=http"));
        assert!(data.contains("OPENSHELL_SANDBOX_ID=sbx-id"));
        assert!(data.contains("OPENSHELL_SANDBOX=demo"));
        assert!(data.contains("OPENSHELL_SANDBOX_TOKEN=jwt-value"));
        assert!(data.contains("OPENSHELL_SANDBOX_TOKEN_FILE=/opt/openshell/auth/sandbox.jwt"));
        // The main-process spec envelope must survive shell quoting.
        assert!(data.contains("OPENSHELL_MAIN_PROCESS_SPEC=base64url:"));
        let labels = secret.metadata.labels.expect("labels");
        assert_eq!(
            labels.get(LABEL_SANDBOX_ID).map(String::as_str),
            Some("sbx-id")
        );
        assert_eq!(
            secret.metadata.name.as_deref(),
            Some("default--demo-bootstrap")
        );
    }

    #[test]
    fn virtual_machine_renders_kubevirt_contract() {
        let cfg = config();
        let vm = render_virtual_machine(&params(&sandbox(), &cfg));

        assert_eq!(vm.metadata.name.as_deref(), Some("default--demo"));
        assert_eq!(vm.metadata.namespace.as_deref(), Some("kvpoc-sandboxes"));
        let labels = vm.metadata.labels.expect("labels");
        assert_eq!(
            labels.get(LABEL_MANAGED_BY).map(String::as_str),
            Some("openshell")
        );

        assert_eq!(vm.data["spec"]["runStrategy"], "Once");

        let domain = &vm.data["spec"]["template"]["spec"]["domain"];
        assert_eq!(domain["cpu"]["cores"], 2);
        assert_eq!(domain["resources"]["requests"]["memory"], "2Gi");
        // Direct kernel boot from the purpose-built image.
        assert_eq!(
            domain["firmware"]["kernelBoot"]["container"]["image"],
            "kvpoc/openshell-sandbox-vm:dev"
        );
        assert_eq!(
            domain["firmware"]["kernelBoot"]["container"]["kernelPath"],
            "/boot/vmlinuz-virt"
        );
        let kernel_args = domain["firmware"]["kernelBoot"]["kernelArgs"]
            .as_str()
            .expect("kernel args");
        assert!(kernel_args.contains("root=/dev/vda"));
        assert!(kernel_args.contains("init=/opt/openshell/init"));

        let volumes = vm.data["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = volumes
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["rootdisk", "cloudinit", "workspace"]);
        assert_eq!(
            volumes[1]["cloudInitNoCloud"]["secretRef"]["name"],
            "default--demo-bootstrap"
        );
        assert_eq!(
            volumes[2]["persistentVolumeClaim"]["claimName"],
            "default--demo-workspace"
        );

        let filesystems = vm.data["spec"]["template"]["spec"]["domain"]["devices"]["filesystems"]
            .as_array()
            .unwrap();
        assert_eq!(filesystems.len(), 1);
        assert_eq!(filesystems[0]["name"], "workspace");
    }

    #[test]
    fn virtual_machine_omits_workspace_without_pvc() {
        let mut cfg = config();
        cfg.kubevirt.workspace_pvc = false;
        let vm = render_virtual_machine(&params(&sandbox(), &cfg));
        let volumes = vm.data["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(
            vm.data["spec"]["template"]["spec"]["domain"]["devices"]["filesystems"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn vm_status_maps_conditions() {
        let cfg = config();
        let vm = render_virtual_machine(&params(&sandbox(), &cfg));
        let mut with_status = vm.clone();
        with_status.data["status"] = serde_json::json!({"ready": true});
        let (_, sbx) =
            sandbox_from_vm_object("kvpoc-sandboxes", with_status).expect("managed vm converts");
        let status = sbx.status.expect("status");
        let ready = status
            .conditions
            .iter()
            .find(|c| c.r#type == "Ready")
            .expect("ready condition");
        assert_eq!(ready.status, "True");

        // A halted VM reports Suspended.
        let mut halted = vm;
        halted.data["spec"]["runStrategy"] = serde_json::json!("Halted");
        halted.data["status"] = serde_json::json!({"ready": false});
        let (_, sbx) = sandbox_from_vm_object("kvpoc-sandboxes", halted).expect("managed vm");
        let status = sbx.status.expect("status");
        assert!(
            status
                .conditions
                .iter()
                .any(|c| c.r#type == "Suspended" && c.status == "True")
        );

        // A freshly created VM (runStrategy Once, not ready) must NOT report
        // Suspended even though printableStatus is Stopped.
        let mut fresh = render_virtual_machine(&params(&sandbox(), &config()));
        fresh.data["status"] = serde_json::json!({"ready": false, "printableStatus": "Stopped"});
        let (_, sbx) = sandbox_from_vm_object("kvpoc-sandboxes", fresh).expect("managed vm");
        let status = sbx.status.expect("status");
        assert!(!status.conditions.iter().any(|c| c.r#type == "Suspended"));
    }

    #[test]
    fn unmanaged_vm_is_rejected() {
        let cfg = config();
        let mut vm = render_virtual_machine(&params(&sandbox(), &cfg));
        vm.metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(LABEL_MANAGED_BY);
        assert!(sandbox_from_vm_object("kvpoc-sandboxes", vm).is_err());
    }

    #[test]
    fn kubevirt_namespace_rejects_non_shared_modes() {
        let mut cfg = config();
        cfg.workspace_mode = WorkspaceMode::Managed;
        assert!(kubevirt_namespace(&cfg).is_err());
        assert!(kubevirt_namespace(&config()).is_ok());
    }

    #[test]
    fn shell_quote_keeps_values_lossless() {
        assert_eq!(shell_quote("simple-value.1"), "simple-value.1");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}

/// How `start` should drive a `runStrategy: Once` VM back to running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubeVirtStartPlan {
    /// The VM is halted: a single `runStrategy: Once` patch boots a fresh
    /// VMI.
    PatchOnce,
    /// The VMI is live or booting (Pending/Scheduling/Scheduled/Running):
    /// start is idempotent. The gateway admits retries from the `Starting`
    /// phase, and tearing down a healthy boot to "really start" would turn
    /// every retry into a restart. The start returns the current runtime
    /// identity.
    AlreadyRunning,
    /// The VMI is terminal (`Succeeded`/`Failed`) or gone while the strategy
    /// is `Once` and the VM reports `Stopped`: a completed VMI makes an
    /// `Once` -> `Once` patch a silent no-op, so boot requires bouncing
    /// through `Halted` first so `KubeVirt` discards the dead VMI.
    BounceHaltedThenOnce,
}

/// Decide how `start` must drive the VM, from the run strategy, the VM's
/// ready flag and printable status, and the paired VMI's phase. Extracted
/// for unit tests. Never tears down a VMI it does not positively know is
/// terminal: unknown phases and missing-context combinations are treated as
/// a boot in progress.
#[must_use]
pub fn kubevirt_start_plan(
    run_strategy: &str,
    vm_ready: bool,
    vmi_phase: Option<&str>,
    printable_status: &str,
) -> KubeVirtStartPlan {
    // A halted VM always needs the `Once` patch, whatever VMI phase is
    // still reported: `stop` only waits for the VM's ready flag to clear,
    // so a VMI in graceful shutdown can still show Running (or any other
    // phase) here. Returning AlreadyRunning for those would silently leave
    // the VM halted. The driver waits out a lingering VMI before patching.
    if run_strategy == "Halted" {
        return KubeVirtStartPlan::PatchOnce;
    }
    match vmi_phase {
        // Live or booting VMI: idempotent no-op.
        Some("Pending" | "Scheduling" | "Scheduled" | "Running" | "WaitingForSync") => {
            KubeVirtStartPlan::AlreadyRunning
        }
        // Terminal VMI: the Once->Once patch would be ignored.
        Some("Succeeded" | "Failed") => KubeVirtStartPlan::BounceHaltedThenOnce,
        // Unknown phase (KubeVirt could not obtain state): conservatively
        // treat as live.
        Some(_) => KubeVirtStartPlan::AlreadyRunning,
        None => {
            if run_strategy == "Once" && !vm_ready && printable_status == "Stopped" {
                // The VMI object is gone and the VM reports Stopped under
                // Once: a dead VMI the controller will not replace on a
                // no-op patch.
                KubeVirtStartPlan::BounceHaltedThenOnce
            } else {
                // No VMI object yet under Once with a printable status that
                // implies the controller is creating one: boot in progress.
                KubeVirtStartPlan::AlreadyRunning
            }
        }
    }
}

/// Namespaced API for `kubevirt.io/v1` `virtualmachineinstances`.
pub fn virtual_machine_instance_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    Api::namespaced_with(
        client.clone(),
        namespace,
        &ApiResource {
            group: KUBEVIRT_GROUP.to_string(),
            version: KUBEVIRT_VERSION.to_string(),
            api_version: format!("{KUBEVIRT_GROUP}/{KUBEVIRT_VERSION}"),
            kind: "VirtualMachineInstance".to_string(),
            plural: "virtualmachineinstances".to_string(),
        },
    )
}

/// Delete an object and wait until it is actually gone from the API.
///
/// Companion resources (the bootstrap Secret, the workspace PVC with its
/// `pvc-protection` finalizer and provisioner cleanup, proxy companions)
/// take time to terminate; an immediate recreate collides with the dying
/// object and surfaces as a confusing `AlreadyExists`. Fails with a clear
/// error when the object is still present after `timeout`.
pub async fn delete_and_wait(
    api: &Api<DynamicObject>,
    name: &str,
    what: &str,
    timeout: Duration,
) -> Result<(), KubernetesDriverError> {
    let _ = api.delete(name, &DeleteParams::default()).await;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match api.get(name).await {
            Err(KubeError::Api(err)) if err.code == 404 => return Ok(()),
            Err(err) => {
                return Err(KubernetesDriverError::Message(format!(
                    "failed to confirm deletion of {what} {name}: {err}"
                )));
            }
            Ok(_) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(KubernetesDriverError::Message(format!(
                "timed out after {}s waiting for {what} {name} to terminate",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A companion resource this create path provisioned, identified by its API
/// resource and UID so rollback only ever deletes objects this create made
/// (a concurrent create may have replaced an orphan with its own).
#[derive(Debug, Clone)]
pub struct CreatedCompanion {
    pub resource: ApiResource,
    pub name: String,
    pub uid: String,
}

/// Best-effort rollback of exactly the companions this create made. Deletes
/// carry UID preconditions, so a companion already replaced by a concurrent
/// create is left alone.
pub async fn rollback_companions(client: &Client, namespace: &str, created: &[CreatedCompanion]) {
    for companion in created {
        let api =
            Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &companion.resource);
        let preconditions = DeleteParams::default().preconditions(Preconditions {
            uid: Some(companion.uid.clone()),
            resource_version: None,
        });
        let _ = api.delete(&companion.name, &preconditions).await;
    }
}

/// Replace the `OPENSHELL_SANDBOX_TOKEN=...` line in a rendered `userData`
/// bootstrap payload with a freshly minted token. Extracted for unit tests.
///
/// A restarted guest re-reads the bootstrap Secret from scratch, so the
/// token frozen at create time would expire under the gateway's JWT TTL and
/// leave the restarted sandbox provisioning forever. `start` refreshes it.
#[must_use]
pub fn replace_user_data_token(user_data: &str, token: &str) -> String {
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in user_data.lines() {
        if line.starts_with("OPENSHELL_SANDBOX_TOKEN=") {
            if !replaced {
                lines.push(format!("OPENSHELL_SANDBOX_TOKEN={token}"));
                replaced = true;
            }
            // A duplicated token line (never rendered today) collapses to the
            // fresh value rather than leaking the stale one.
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        // The create-time render always carries the token line; tolerate a
        // hand-edited payload by appending it.
        lines.push(format!("OPENSHELL_SANDBOX_TOKEN={token}"));
    }
    lines.join("\n") + "\n"
}

/// Refresh the bootstrap Secret's sandbox token from the gateway's launch
/// authentication. Reads the stored `data.userData` (the apiserver persists
/// `stringData` there), swaps the token line, and patches the Secret.
pub async fn refresh_bootstrap_secret_token(
    client: &Client,
    namespace: &str,
    kube_name: &str,
    token: &str,
) -> Result<(), KubernetesDriverError> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret_name = bootstrap_secret_name(kube_name);
    let secret = tokio::time::timeout(KUBE_API_TIMEOUT, api.get(&secret_name))
        .await
        .map_err(|_| {
            KubernetesDriverError::Message(format!(
                "timed out reading bootstrap Secret {secret_name}"
            ))
        })?
        .map_err(KubernetesDriverError::from_kube)?;
    // k8s-openapi's `ByteString` already holds the decoded content bytes;
    // `Secret.data` values arrive base64-decoded.
    let user_data = secret
        .data
        .as_ref()
        .and_then(|data| data.get("userData"))
        .and_then(|value| String::from_utf8(value.0.clone()).ok())
        .unwrap_or_default();
    let refreshed = replace_user_data_token(&user_data, token);
    let patch = serde_json::json!({
        "stringData": {"userData": refreshed}
    });
    tokio::time::timeout(
        KUBE_API_TIMEOUT,
        api.patch(&secret_name, &PatchParams::default(), &Patch::Merge(&patch)),
    )
    .await
    .map_err(|_| {
        KubernetesDriverError::Message(format!("timed out patching bootstrap Secret {secret_name}"))
    })?
    .map_err(KubernetesDriverError::from_kube)?;
    Ok(())
}

#[cfg(test)]
mod kubevirt_review_tests {
    use super::*;
    use crate::KubernetesComputeConfig;
    use openshell_core::proto::compute::v1::DriverSandbox as Sandbox;

    fn review_sandbox() -> Sandbox {
        Sandbox {
            id: "sbx-id".to_string(),
            name: "demo".to_string(),
            namespace: "kvpoc-sandboxes".to_string(),
            spec: None,
            status: None,
            workspace: "default".to_string(),
        }
    }

    fn review_config() -> KubernetesComputeConfig {
        KubernetesComputeConfig {
            runtime: KubernetesRuntimeMode::KubeVirt,
            namespace: "kvpoc-sandboxes".to_string(),
            gateway_id: "kvpoc".to_string(),
            grpc_endpoint: "http://172.18.0.1:18085/".to_string(),
            kubevirt: crate::KubeVirtConfig {
                guest_image: "kvpoc/openshell-sandbox-vm:dev".to_string(),
                guest_image_pull_policy: "IfNotPresent".to_string(),
                cpu_cores: 2,
                memory: "2Gi".to_string(),
                workspace_pvc: true,
            },
            ..KubernetesComputeConfig::default()
        }
    }

    fn review_params<'a>(
        sandbox: &'a Sandbox,
        config: &'a KubernetesComputeConfig,
    ) -> KubeVirtSandboxParams<'a> {
        KubeVirtSandboxParams {
            sandbox,
            config,
            namespace: "kvpoc-sandboxes",
            kube_name: "default--demo",
        }
    }

    #[test]
    fn start_plan_is_idempotent_during_boot() {
        use super::KubeVirtStartPlan;
        // A start admitted from Starting during the boot window must not
        // restart a healthy, booting VM (the gateway admits retries from
        // Starting, and the VM is Once + not ready throughout the window).
        for phase in ["Pending", "Scheduling", "Scheduled", "Running"] {
            assert_eq!(
                kubevirt_start_plan("Once", false, Some(phase), "Starting"),
                KubeVirtStartPlan::AlreadyRunning,
                "a booting VMI ({phase}) must not be bounced"
            );
        }
        // So is a ready VM and an unrecognized VMI phase.
        assert_eq!(
            kubevirt_start_plan("Once", true, Some("Running"), "Running"),
            KubeVirtStartPlan::AlreadyRunning
        );
        assert_eq!(
            kubevirt_start_plan("Once", false, Some("Unknown"), "Unknown"),
            KubeVirtStartPlan::AlreadyRunning
        );
        // No VMI object yet while the controller creates one: boot starting.
        assert_eq!(
            kubevirt_start_plan("Once", false, None, "Provisioning"),
            KubeVirtStartPlan::AlreadyRunning
        );
    }

    #[test]
    fn start_plan_bounces_dead_once_vm() {
        use super::KubeVirtStartPlan;
        // A terminal VMI under Once makes the Once->Once patch a no-op.
        for phase in ["Succeeded", "Failed"] {
            assert_eq!(
                kubevirt_start_plan("Once", false, Some(phase), "Stopped"),
                KubeVirtStartPlan::BounceHaltedThenOnce,
                "a terminal VMI ({phase}) must be bounced"
            );
        }
        // A gone VMI with a Stopped printable under Once is also dead.
        assert_eq!(
            kubevirt_start_plan("Once", false, None, "Stopped"),
            KubeVirtStartPlan::BounceHaltedThenOnce
        );
        // A halted VM boots with one patch; a ready VM is left alone.
        assert_eq!(
            kubevirt_start_plan("Halted", false, None, "Stopped"),
            KubeVirtStartPlan::PatchOnce
        );
        assert_eq!(
            kubevirt_start_plan("Once", true, None, "Running"),
            KubeVirtStartPlan::AlreadyRunning
        );
    }

    #[test]
    fn start_plan_halved_vm_always_patches_despite_lingering_vmi() {
        use super::KubeVirtStartPlan;
        // `stop` only waits for the VM's ready flag; a VMI in graceful
        // shutdown can still report any phase. A start issued right after
        // stop must patch Once (after waiting out the VMI), never report
        // AlreadyRunning and silently leave the VM halted.
        for phase in ["Running", "Succeeded", "Failed", "Pending", "Unknown"] {
            assert_eq!(
                kubevirt_start_plan("Halted", false, Some(phase), "Stopping"),
                KubeVirtStartPlan::PatchOnce,
                "a halted VM with a lingering VMI ({phase}) must patch"
            );
        }
        assert_eq!(
            kubevirt_start_plan("Halted", false, Some("Succeeded"), "Stopped"),
            KubeVirtStartPlan::PatchOnce
        );
    }

    #[test]
    fn terminal_failure_maps_vmi_and_printable_states() {
        // A failed or completed VMI under a not-ready VM is terminal.
        assert!(terminal_vm_failure("Stopped", Some("Failed"), false).is_some());
        assert!(terminal_vm_failure("Stopped", Some("Succeeded"), false).is_some());
        // KubeVirt's terminal printable statuses stand on their own
        // (VirtualMachinePrintableStatus, KubeVirt v1.9).
        for status in [
            "CrashLoopBackOff",
            "ErrorUnschedulable",
            "ErrorPvcNotFound",
            "DataVolumeError",
        ] {
            assert!(
                terminal_vm_failure(status, None, false).is_some(),
                "{status} must be terminal"
            );
        }
        // Image-pull problems are transient (kubelet retries) but visible:
        // the caller puts the printableStatus in the condition message.
        for status in ["ErrImagePull", "ImagePullBackOff"] {
            assert!(
                terminal_vm_failure(status, None, false).is_none(),
                "{status} must stay transient"
            );
        }
        // Unknown (controller could not obtain state) can appear transiently;
        // mapping it to Error would flip a booting sandbox on a blip.
        assert!(terminal_vm_failure("Unknown", None, false).is_none());
        // A running VM, a not-yet-booted VM, and a missing VMI are not
        // terminal: those stay provisioning.
        assert!(terminal_vm_failure("Running", Some("Running"), true).is_none());
        assert!(terminal_vm_failure("Stopped", None, false).is_none());
        assert!(terminal_vm_failure("Starting", Some("Pending"), false).is_none());
        assert!(terminal_vm_failure("WaitingForVolumeBinding", None, false).is_none());
        // A Succeeded VMI under a ready VM is not a failure signal.
        assert!(terminal_vm_failure("Running", Some("Succeeded"), true).is_none());
    }

    #[test]
    fn transient_image_pull_status_is_visible_in_condition_message() {
        // ErrImagePull stays transient but the condition message must carry
        // the printableStatus so the cause reaches the user.
        let cfg = review_config();
        let mut vm = render_virtual_machine(&review_params(&review_sandbox(), &cfg));
        vm.data["status"] =
            serde_json::json!({"ready": false, "printableStatus": "ImagePullBackOff"});
        let (_, sbx) =
            sandbox_from_vm_object_with_vmi("kvpoc-sandboxes", vm, None).expect("converts");
        let ready = sbx.status.unwrap().conditions[0].clone();
        assert_eq!(ready.reason, "Starting");
        assert!(
            ready.message.contains("ImagePullBackOff"),
            "message must name the pull problem: {}",
            ready.message
        );
    }

    #[test]
    fn vm_status_marks_terminal_vmi_as_error() {
        let cfg = review_config();
        let mut vm = render_virtual_machine(&review_params(&review_sandbox(), &cfg));
        vm.data["spec"]["runStrategy"] = serde_json::json!("Once");
        vm.data["status"] = serde_json::json!({"ready": false, "printableStatus": "Stopped"});

        // Without the VMI phase the not-ready VM stays transient (Starting).
        let (_, sbx) =
            sandbox_from_vm_object_with_vmi("kvpoc-sandboxes", vm.clone(), None).expect("converts");
        let ready = sbx.status.unwrap().conditions[0].clone();
        assert_eq!(ready.reason, "Starting");

        // With a Failed VMI the same VM reports the non-transient VMFailed
        // reason, which the gateway maps to the public Error phase.
        let (_, sbx) = sandbox_from_vm_object_with_vmi("kvpoc-sandboxes", vm, Some("Failed"))
            .expect("converts");
        let ready = sbx.status.unwrap().conditions[0].clone();
        assert_eq!(ready.status, "False");
        assert_eq!(ready.reason, "VMFailed");
        assert!(ready.message.contains("Failed"));
    }

    #[test]
    fn replace_user_data_token_swaps_stale_token() {
        let user_data = "OPENSHELL_SANDBOX_ID=sbx-id\nOPENSHELL_SANDBOX_TOKEN=stale-jwt\nOPENSHELL_ENDPOINT=http://gw\n";
        let refreshed = replace_user_data_token(user_data, "fresh-jwt");
        assert!(refreshed.contains("OPENSHELL_SANDBOX_TOKEN=fresh-jwt"));
        assert!(!refreshed.contains("stale-jwt"));
        assert!(refreshed.contains("OPENSHELL_SANDBOX_ID=sbx-id"));
        assert!(refreshed.contains("OPENSHELL_ENDPOINT=http://gw"));
        assert!(refreshed.ends_with('\n'));
    }

    #[test]
    fn replace_user_data_token_appends_when_missing() {
        let user_data = "OPENSHELL_SANDBOX_ID=sbx-id\n";
        let refreshed = replace_user_data_token(user_data, "fresh-jwt");
        assert!(refreshed.contains("OPENSHELL_SANDBOX_TOKEN=fresh-jwt"));
        assert!(refreshed.contains("OPENSHELL_SANDBOX_ID=sbx-id"));
    }

    #[test]
    fn replace_user_data_token_collapses_duplicates() {
        let user_data = "OPENSHELL_SANDBOX_TOKEN=a\nOPENSHELL_SANDBOX_TOKEN=b\n";
        let refreshed = replace_user_data_token(user_data, "fresh");
        assert_eq!(refreshed.matches("OPENSHELL_SANDBOX_TOKEN=").count(), 1);
        assert!(refreshed.contains("OPENSHELL_SANDBOX_TOKEN=fresh"));
    }

    #[test]
    fn https_endpoint_is_rejected_with_clear_message() {
        let mut cfg = review_config();
        cfg.grpc_endpoint = "https://openshell-gateway.ns.svc:8080".to_string();
        let err = cfg.validate_kubevirt_config().expect_err("https rejected");
        assert!(err.contains("https://"), "message should name TLS: {err}");
        // A plain http endpoint with any host shape passes endpoint checks.
        cfg.grpc_endpoint = "http://openshell-gateway.ns.svc:8080".to_string();
        assert!(cfg.validate_kubevirt_config().is_ok());
    }
}

/// Decode the gateway's launch authentication for the start-path token
/// refresh. Empty input skips the refresh (with a warning: the guest keeps
/// the create-time token, which expires under the gateway JWT TTL); a
/// malformed bundle is an error rather than a silent stale-token fallback.
pub fn decode_launch_authentication(
    encoded: &[u8],
) -> Result<Option<openshell_core::jwt::SandboxLaunchAuthentication>, KubernetesDriverError> {
    if encoded.is_empty() {
        tracing::warn!(
            "start carried no launch authentication; the guest keeps the create-time \
             sandbox token, which expires under the gateway JWT TTL"
        );
        return Ok(None);
    }
    serde_json::from_slice::<openshell_core::jwt::SandboxLaunchAuthentication>(encoded)
        .map(Some)
        .map_err(|error| {
            KubernetesDriverError::Precondition(format!(
                "invalid launch authentication bundle: {error}"
            ))
        })
}
