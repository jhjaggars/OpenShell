# KubeVirt compute-driver proof of concept

This directory contains the proof of concept for running OpenShell sandboxes as
**KubeVirt VirtualMachines** through the Kubernetes compute driver, instead of
`agents.x-k8s.io` Sandbox CRs reconciled into pods.

This is the `-v2` port of the PoC onto current `main`. Main has since
replaced the supervisor/guest split the original branch was built on with
the **isolation boundary architecture** (an in-workload `openshell-sandbox`
boundary plus a host-side `openshell-supervisor`; see
`crates/openshell-sandbox-backend`). The guest integration with that
architecture is the follow-up (below). What this branch demonstrates today,
on a local k3d cluster with nested KVM, is the **driver lifecycle**:

1. `openshell sandbox create` provisions a `kubevirt.io/v1` `VirtualMachine`
   (plus a bootstrap Secret and a workspace PVC) carrying the standard
   OpenShell ownership labels, and returns the VM UID as the runtime
   identity.
2. The guest boots from a purpose-built containerDisk (a small init script
   consumes the driver-rendered bootstrap config).
3. `sandbox stop` / `start` maps to the VM `runStrategy`
   (`Halted`/`Once`); start refreshes the guest's sandbox token from the
   gateway's launch authentication (the create-time token would expire under
   the JWT TTL), and a dead `Once` VMI is bounced through `Halted` so start
   actually restarts it.
4. Failed or completed guests surface as terminal `Error` conditions from
   the VMI phase instead of provisioning forever.
5. `sandbox delete` removes the VM, bootstrap Secret and workspace PVC with
   UID preconditions; orphaned companions are deleted and *waited for*
   before recreation.

Everything here is explicitly **proof-of-concept quality**: single shared
namespace mode, plaintext http gateway only, no mTLS, no GPU, no NetworkPolicy
coverage for virt-launcher pods, no migration handling, and no support for
arbitrary OCI sandbox images as VM disks (the guest image is purpose-built).

## How it works

```
┌──────────────┐   kube-rs (kubeconfig)    ┌─────────────────────────┐
│ openshell-   │──────────────────────────▶│ kvpoc k3d cluster       │
│ gateway      │                           │                         │
│ (host)       │◀──────────────────────────│ kubevirt.io/v1          │
│              │  supervisor relay (gRPC)  │  VirtualMachine         │
└──────────────┘        ┌──────────────────┘  └─▶ VMI ─▶ virt-launcher
                        │                        pod ─▶ QEMU/KVM guest
                        │                        (Alpine + openshell-
                        │                         sandbox, PID 1)
              ┌─────────┴──────────┐
              │ VM volumes:        │
              │  rootdisk     containerDisk (boot ext4 + kernel/initrd)
              │  cloudinit    bootstrap Secret (cloudInitNoCloud ISO)
              │  workspace    PVC (virtio-fs, mounted at /sandbox)
              └────────────────────┘
```

- **Driver change** (`crates/openshell-driver-kubernetes`): a new
  `runtime = "kubevirt"` mode in `[openshell.drivers.kubernetes]`. In this
  mode the driver creates `VirtualMachine` objects directly (the KubeVirt
  controller replaces the Agent Sandbox controller) and maps
  get/list/watch/stop/start/delete onto VM/VMI state. See
  `src/kubevirt.rs` and the branches in `src/driver.rs`.
- **Guest bootstrap**: the driver renders the supervisor environment
  (gateway endpoint, sandbox id/name, the gateway-minted sandbox JWT from
  `DriverSandboxSpec.sandbox_token`, main-process spec) as a
  shell-sourceable `userData` Secret, attached as a `cloudInitNoCloud`
  volume. `guest-init.sh` (baked into the image as `/opt/openshell/init`,
  booted via direct kernel boot `init=`) mounts the ISO, materialises the
  token at `/opt/openshell/auth/sandbox.jwt`, mounts the workspace PVC
  (virtio-fs) and execs the supervisor.
- **Readiness** stays gateway-owned: the driver reports VM/VMI conditions
  only; the sandbox becomes `Ready` when the supervisor session connects.

## Files

| File | Purpose |
|---|---|
| `build-guest-image.sh` | Builds the `kvpoc/openshell-sandbox-vm:dev` containerDisk (Alpine rootfs + static musl supervisor + guest init + kernel, repacked initramfs, KubeVirt's `container-disk` helper). |
| `guest-init.sh` | Guest PID-1 init (baked into the image). |
| `demo.sh` | Starts a host gateway configured for `runtime = "kubevirt"` against the `kvpoc` cluster; `--demo` then runs `run-demo.sh` against it. |
| `run-demo.sh` | The end-to-end demo: create → Ready → exec → stop/start → delete + teardown verification. |

## Reproducing

Prerequisites: a Linux host with KVM (`/dev/kvm`), docker, k3d, kubectl, and
this repo built (`cargo build --release -p openshell-server -p openshell-cli`;
the guest image build compiles the supervisor itself as static musl).

```shell
# 1. Cluster: k3d with /dev/kvm passed to the node and working DNS.
#    (KubeVirt >= 1.9's containerDisk flow needs the Kubernetes ImageVolume
#    feature, so use k8s >= 1.33 — k3s v1.35.x is known good. The static
#    resolv.conf works around docker handing the k3d node an unreachable
#    stub resolver on hosts that run systemd-resolved.)
printf 'nameserver 10.43.0.10\nnameserver 1.1.1.1\noptions ndots:0\n' > /tmp/kvpoc-resolv.conf
K3D_FIX_DNS=false k3d cluster create kvpoc \
  --image rancher/k3s:v1.35.9-k3s1 \
  --volume /dev/kvm:/dev/kvm@server:0 \
  --volume /tmp/kvpoc-resolv.conf:/etc/resolv.conf@server:0 \
  --k3s-arg "--disable=traefik@server:0"

# If virt-handler crash-loops with "Failed to create an inotify watcher:
# too many open files", raise the host limit:
#   sudo sysctl -w fs.inotify.max_user_instances=1024

# 2. KubeVirt (v1.9.1 known good) with virtio-fs PVC sharing enabled.
kubectl apply -f https://github.com/kubevirt/kubevirt/releases/download/v1.9.1/kubevirt-operator.yaml
kubectl apply -f https://github.com/kubevirt/kubevirt/releases/download/v1.9.1/kubevirt-cr.yaml
kubectl patch kubevirt kubevirt -n kubevirt --type merge \
  -p '{"spec":{"configuration":{"developerConfiguration":{"featureGates":["EnableVirtioFsStorageVolumes"]}}}}'
kubectl wait --for=condition=Available deployment/virt-api -n kubevirt --timeout=600s

# 3. Guest image.
e2e/kubevirt/build-guest-image.sh
k3d image import kvpoc/openshell-sandbox-vm:dev -c kvpoc

# 4. Run the demo (builds the gateway + CLI, starts the gateway, exercises
#    create/Ready/exec/stop/start/delete, tears the sandbox down).
e2e/kubevirt/demo.sh --demo
```

Expected output (excerpt):

```
=== 1/5 sandbox create (provisions a KubeVirt VirtualMachine) ===
=== 2/5 waiting for supervisor connection (gateway-composed Ready) ===
=== 3/5 exec inside the VM (via the supervisor relay) ===
hello from Linux 6.12.114-0-virt x86_64 (PID 1: openshell-sandb)
=== 4/5 stop / start (runStrategy Halted -> Once) ===
=== 5/5 delete and verify teardown ===
demo complete: sandbox ran in a KubeVirt VM and tore down cleanly
```

Run `e2e/kubevirt/demo.sh` (without `--demo`) to keep the gateway in the
foreground and drive the CLI by hand; it registers gateway `kvpoc`
(`http://127.0.0.1:18085`, plaintext) as the CLI's active gateway.

### Inspecting a running sandbox

```shell
kubectl get vm,vmi,pod,secret,pvc -n kvpoc-sandboxes
# serial console (guest boot + supervisor logs):
kubectl exec -n kvpoc-sandboxes $(kubectl get pod -l kubevirt.io=virt-launcher \
  -n kvpoc-sandboxes -o jsonpath='{.items[0].metadata.name}') -c compute -- \
  cat /var/run/kubevirt-private/$(kubectl get vmi default--kvpoc-demo \
  -n kvpoc-sandboxes -o jsonpath='{.metadata.uid}')/virt-serial0-log | tail -30
```

## Teardown

```shell
# remove sandbox resources (if any)
kubectl delete vm,secret,pvc -n kvpoc-sandboxes --all
# remove the whole cluster
k3d cluster delete kvpoc
```

The demo gateway state lives in `.cache/gateway-kubevirt/` (sqlite DB + TLS
material); delete it to fully reset.

## Known gaps / follow-ups

- **Identity**: the guest uses the gateway-minted sandbox JWT delivered via
  the bootstrap Secret. The production design (see
  `architecture/plans/kubevirt-compute-driver-design.md` in the main
  checkout) is Secret-bound SA tokens with gateway-side validation — this
  PoC deliberately reuses the pre-minted-token model the libkrun VM driver
  already uses.
- The bootstrap Secret carries the JWT in (base) plaintext `userData`;
  namespace-scoped but still worth hardening before any real use.
- Single (shared) workspace mode; managed/operator modes are rejected.
- No Helm chart / RBAC changes: the PoC gateway runs on the host with the
  cluster-admin kubeconfig. A real deployment needs VM/VMI/PVC/Secret rules
  added to the chart's Role/ClusterRole.
- Networking: masquerade pod network only; the guest clamps its MTU to 1400
  in `guest-init.sh` because the k3d flannel-vxlan pod network is 1450.
- No GPU, no live-migration/VMI-replacement semantics, no
  NetworkPolicy for virt-launcher pods, plaintext gateway only.
- Upstream context: kubernetes-sigs/agent-sandbox#1157 proposes a
  `runtimeBackend: VirtualMachine` Sandbox CR extension (unmerged POC fork).
  If that lands, the driver could switch from direct VM creation back to the
  Sandbox CR with a VM workload.

## Follow-up: guest integration with main's boundary architecture

The original branch's guest model (a supervisor binary as guest PID 1
connecting to the gateway with the pre-minted sandbox JWT, plus a per-sandbox
network-enforcement proxy pod) no longer exists on main. The remaining work,
modeled on how `crates/openshell-driver-vm` now runs its host supervisor:

- The guest runs the in-workload `openshell-sandbox` boundary; the driver
  injects a `BoundaryConfig` (listener TLS material + connection details)
  through the bootstrap channel instead of raw environment.
- A supervisor runs in a **pod** (per-sandbox, or the gateway-shared
  supervisor) with `--role=isolation-backend`, `--backend-descriptor-file`
  and `--auth-bundle-file` mounted from driver-created Secrets — the token
  then never appears in a pod spec and refreshes by patching the Secret,
  matching main's `SharedSupervisorConfig` token flow.
- The network-enforcement proxy (L4/L7 policy, TLS interception) lives with
  that supervisor, its CA delivered to the guest through the boundary
  channel, replacing the original branch's per-sandbox proxy pod, its
  NetworkPolicy fence, and the `PROXY_BIND_ADDR` standalone-bind mode.
- DNS tunnelling through kube-dns and the deliberate workload-to-gateway
  egress allowance are inherited concerns of that topology and must be
  documented with it.
