# KubeVirt compute-driver proof of concept

This directory contains the proof of concept for running OpenShell sandboxes as
**KubeVirt VirtualMachines** through the Kubernetes compute driver, instead of
`agents.x-k8s.io` Sandbox CRs reconciled into pods. It is built on current
`main`'s **isolation boundary architecture** (an in-workload
`openshell-sandbox` boundary plus its supervisor; see
`crates/openshell-sandbox-backend`) using the **Option A** boundary delivery:
the pod path's boundary artifacts ride the cloud-init channel into the guest.
The KubeVirt runtime supports the pod path's two supervisor topologies with
the same defaults: **a dedicated supervisor Pod per sandbox** (the default,
`supervisor_topology` unset) or the **gateway-shared supervisor**
(`supervisor_topology = "gateway"`).

What this branch demonstrates, on a local k3d cluster with nested KVM:

1. `openshell sandbox create` reaches **Ready**: the driver creates the
   `VirtualMachine` (Halted first, so its UID is captured for the claims),
   the supervisor generation — a dedicated supervisor Pod with its
   per-sandbox Service and bootstrap Secret (the default), or the
   shared-supervisor instance registration — the egress-fence
   NetworkPolicy, the bootstrap Secret, and the workspace PVC; the guest
   boots, its boundary dials the supervisor over the runtime connection,
   and the supervisor session establishes.
2. **Exec in the VM** as the zero-capability workload user; the workload
   environment carries no gateway material (no token, no endpoint).
3. **L4/L7 network policy** enforced by the shared supervisor: allowed and
   denied raw-TCP connects, and HTTPS GET allowed / POST denied through the
   L7 proxy — the GET proves the per-sandbox proxy CA is delivered into the
   guest.
4. **Egress fencing**: the virt-launcher pod's NetworkPolicy allows only the
   supervisor Service and cluster DNS; direct egress from the guest is
   blocked at both the boundary (seccomp broker) and the pod network
   (fence).
5. **Stop/start**: `Halted`/`Once` run-strategy mapping with the VM UID and
   fence UID stable across generations; start re-renders the boundary
   bootstrap for the new generation, re-registers the shared instance
   (gateway topology) or provisions a fresh supervisor Pod whose new UID the
   re-rendered bootstrap claims (dedicated topology).
6. **Fence verification** on every start: the expected fence (UID + rendered
   spec) is persisted on the VM; a spec edit fails start with a drift
   Precondition, a label-only edit is accepted.
7. A guest whose runtime exits powers off (no kernel panic, no hung QEMU):
   the VMI completes and the driver maps it to the terminal `VMFailed`
   condition; a main-process exit surfaces `Error` with the exit code.
8. `sandbox delete` removes the VM, companions, and fence with zero
   leftovers, and withdraws the shared-supervisor instance.

Everything here is explicitly **proof-of-concept quality**: single shared
workspace mode, no GPU, no migration handling, and no support for arbitrary
OCI sandbox images as VM disks (the guest image is purpose-built). See
[Known gaps](#known-gaps--follow-ups).

## How it works

```
┌──────────────┐   kube-rs (kubeconfig)    ┌─────────────────────────┐
│ openshell-   │◀═══ assignment stream ═══▶│ kvpoc k3d cluster       │
│ gateway      │   (TLS, supervisor token) │                         │
│ (host, TLS)  │                           │ kubevirt.io/v1          │
│              │   kube-rs                 │  VirtualMachine         │
└──────┬───────┘──────────────────────────▶└─▶ VMI ─▶ virt-launcher
       │                                     pod ─▶ QEMU/KVM guest
       │ routed runtime connection (TLS)            (Alpine + openshell-
       │         ┌──────────────────────┐            sandbox boundary)
       ╚════════▶│ supervisor           │   ▲
                 │ (dedicated: per-     │   │ fence NetworkPolicy:
                 │  sandbox Pod+Service │   │  launcher egress → only
                 │  in kvpoc-sandboxes; │   │  this sandbox's supervisor
                 │  gateway: Helm-shared │   │
                 │  Deployment+Service) │   │
                 └──────────────────────┘   │
                                            │
              VM volumes:                   │
               rootdisk     containerDisk (boot ext4 + kernel/initrd)
               cloudinit    bootstrap Secret (cloudInitNoCloud ISO)
               workspace    PVC (virtio-fs, mounted at /sandbox)
```

- **Driver change** (`crates/openshell-driver-kubernetes`): a
  `runtime = "kubevirt"` mode in `[openshell.drivers.kubernetes]` with the
  pod path's two supervisor topologies: the default provisions one
  dedicated supervisor Pod per sandbox (reusing the pod path's
  `supervisor_pod`, `boundary_service`, `supervisor_bootstrap_secret`, and
  control-policy renderers with `VirtualMachine` owner references), and
  `supervisor_topology = "gateway"` plus a `shared_supervisor` config
  places each generation on the gateway's shared supervisor. The driver
  creates `VirtualMachine` objects directly (the KubeVirt controller
  replaces the Agent Sandbox controller) and maps
  get/list/watch/stop/start/delete onto VM/VMI state. See `src/kubevirt.rs`,
  `src/kubevirt_boundary.rs`, and the branches in `src/driver.rs`.
- **Boundary delivery (Option A)**: the driver renders the pod path's
  boundary artifacts (`boundary.json` + session TLS, from the same
  `render_boundary_bootstrap` shared helper) into a shell-sourceable
  cloud-init `userData` (base64 lines, no gateway material). The guest init
  stages them on a tmpfs and runs
  `openshell-sandbox launch-capability-free <uid> <gid> <config>`; the
  boundary dials the supervisor's Service — the per-sandbox
  `os-supervisor-<id>` Service of a dedicated generation, or the shared
  supervisor's Service with the sandbox's route handle.
- **Guest image** (`build-guest-image.sh`): Alpine + static-musl
  `openshell-sandbox` + the init script, booted via direct kernel boot. The
  guest's resolver is the boundary's loopback DNS relay (`127.0.0.53`).
- **Readiness** stays gateway-owned: the driver reports VM/VMI conditions
  only; the sandbox becomes `Ready` when the supervisor session connects.

## Files

| File | Purpose |
|---|---|
| `build-guest-image.sh` | Builds the `kvpoc/openshell-sandbox-vm:dev` containerDisk (Alpine rootfs + static musl supervisor + guest init + kernel, repacked initramfs, KubeVirt's `container-disk` helper). |
| `guest-init.sh` | Guest PID-1 init (baked into the image). |
| `demo.sh` | Starts a host gateway (TLS) against the `kvpoc` cluster and registers it as the CLI's mTLS gateway; `--demo` then runs `run-demo.sh` against it. Defaults to the dedicated supervisor topology; `OPENSHELL_KVPOC_TOPOLOGY=gateway` selects the shared one. |
| `run-demo.sh` | The full acceptance demo (10 steps, no failure tolerance): create → Ready, fence check, exec, env scrub, L4/L7 policy, bypass blocked, stop/start with stable VM/fence UIDs (dedicated: a rotated and re-claimed supervisor Pod UID), fence drift verification, main-process exit → Error, delete with zero leftovers. Topology-aware via `OPENSHELL_KVPOC_TOPOLOGY`. |
| `demo-policy.yaml` | The acceptance policy: L4 raw-TCP allow/deny and L7 HTTPS GET-allow/POST-deny. |
| `supervisor-values.yaml` | Renders the shared supervisor (Deployment, Service, NetworkPolicy, token Secret) from the OpenShell Helm chart, pointing at the demo gateway. |

## Reproducing

Prerequisites: a Linux host with KVM (`/dev/kvm`), docker, k3d, kubectl,
helm, and this repo built (`cargo build --release -p openshell-gateway -p
openshell-cli`; the guest image build compiles the supervisor itself as
static musl).

E2e prerequisites (discovered the hard way):

- `OPENSHELL_BIN=$CARGO_TARGET_DIR/debug/openshell` — the sandbox_lifecycle
  tests spawn the CLI via this env var (default is `target/debug/openshell`
  relative to the repo, which misses when `CARGO_TARGET_DIR` is redirected)
- `util-linux-script` installed — the TTY tests run the CLI under a PTY via
  `/usr/bin/script`; on minimal Fedora installs this is a separate subpackage
- `docker pull public.ecr.aws/docker/library/python:3.13-slim` — the
  `driver_config_volume` suite builds an OCI test image from this base

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

# 3. Guest image, then the shared supervisor image (both from this branch).
e2e/kubevirt/build-guest-image.sh
k3d image import kvpoc/openshell-sandbox-vm:dev -c kvpoc
mise run build:docker:supervisor   # builds openshell/supervisor:dev (podman)
# import it into the cluster's docker+k3d (docker load from a podman save):
podman save openshell/supervisor:dev -o /tmp/supervisor.tar
docker load -i /tmp/supervisor.tar
docker tag localhost/openshell/supervisor:dev kvpoc/openshell-supervisor:dev
k3d image import kvpoc/openshell-supervisor:dev -c kvpoc

# 4. Shared supervisor (only needed for OPENSHELL_KVPOC_TOPOLOGY=gateway):
#    render the supervisor templates from the OpenShell Helm chart into the
#    `openshell` namespace, pointing at the demo gateway. The values file's
#    grpc_endpoint placeholder must be substituted with the k3d bridge IP
#    (the host address pods use to reach the host-run gateway). The demo
#    gateway mints the supervisor token into the rendered token Secret and
#    syncs the gateway CA into the Secret the supervisor mounts. The
#    dedicated topology needs no cluster-side supervisor: the driver
#    creates one Pod per sandbox from the locally imported
#    kvpoc/openshell-supervisor:dev image.
kubectl create namespace openshell
BRIDGE_IP="$(docker network inspect k3d-kvpoc -f '{{(index .IPAM.Config 0).Gateway}}')"
sed "s/172.18.0.1/${BRIDGE_IP}/" e2e/kubevirt/supervisor-values.yaml \
    > /tmp/kvpoc-supervisor-values.yaml
helm template kvpoc deploy/helm/openshell -n openshell \
    -f /tmp/kvpoc-supervisor-values.yaml \
    -s templates/supervisor.yaml -s templates/supervisor-service.yaml \
    -s templates/supervisor-networkpolicy.yaml -s templates/supervisor-token-secret.yaml \
    | kubectl apply -f -
kubectl rollout status deployment/kvpoc-openshell-supervisor -n openshell --timeout=120s

# 5. Run the demo (builds the gateway + CLI, starts the gateway, runs the
#    full acceptance, tears the sandbox down). The default topology is
#    dedicated; run both for the full acceptance:
e2e/kubevirt/demo.sh --demo
OPENSHELL_KVPOC_TOPOLOGY=gateway e2e/kubevirt/demo.sh --demo

# 5b. (Optional) prove the chart's RBAC is sufficient: render the chart's
#     ServiceAccount and Roles for the kubevirt runtime, mint a kubeconfig
#     from the gateway ServiceAccount token, and run the gateway on it
#     (the demo's own kubectl scaffolding keeps the ambient KUBECONFIG):
helm template kvpoc deploy/helm/openshell -n openshell \
    --set agentSandbox.preflight.enabled=false \
    --set gatewayConfig."openshell\.drivers\.kubernetes"."runtime"=kubevirt \
    --set server.sandboxNamespace=kvpoc-sandboxes \
    --set supervisorTopology=gateway \
    -s templates/serviceaccount.yaml -s templates/role.yaml \
    -s templates/rolebinding.yaml -s templates/clusterrole.yaml \
    -s templates/clusterrolebinding.yaml -s templates/peer-role.yaml \
    | kubectl apply -f -
SA=kvpoc-openshell  # match the chart's gateway ServiceAccount name (release-openshell)
TOKEN="$(kubectl create token "${SA}" -n openshell --duration=2h)"
SERVER="$(kubectl config view -o jsonpath='{.clusters[0].cluster.server}')"
CA="$(kubectl config view -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')"
cat > /tmp/kvpoc-sa-kubeconfig <<EOF
apiVersion: v1
kind: Config
clusters: [{name: kvpoc, cluster: {server: ${SERVER}, certificate-authority-data: ${CA}}}]
contexts: [{name: kvpoc, context: {cluster: kvpoc, user: ${SA}}}]
current-context: kvpoc
users: [{name: ${SA}, user: {token: ${TOKEN}}}]
EOF
OPENSHELL_KVPOC_GATEWAY_KUBECONFIG=/tmp/kvpoc-sa-kubeconfig e2e/kubevirt/demo.sh --demo
```

The demo gateway's PKI lives in `.cache/gateway-kubevirt/tls/pki/`; deleting
it (or running from a fresh checkout) mints a new CA, which demo.sh syncs into
the supervisor's `kvpoc-server-tls` Secret. The supervisor reads that CA at
startup, so an already-running supervisor pod predating the new PKI keeps
failing to attach (`could not open the supervisor host stream`) — restart it
after wiping the gateway state:

```shell
kubectl rollout restart deployment/kvpoc-openshell-supervisor -n openshell
```

Actual output of one clean `run-demo.sh` run (every step fails the demo on
error — there is no Ready-wait tolerance):

```
=== 1/10 create reaching Ready (boundary dials the shared supervisor) ===
Created sandbox: kvpoc-demo
objects:
virtualmachine.kubevirt.io/default--kvpoc-demo   Running   True
virtualmachineinstance.kubevirt.io/default--kvpoc-demo   Running   10.42.0.60   True
secret/default--kvpoc-demo-bootstrap   Opaque   1   19s
persistentvolumeclaim/default--kvpoc-demo-workspace   Bound   2Gi   RWO   local-path
=== 2/10 fence NetworkPolicy and its podSelector ===
fence: openshell-kubevirt-fence-cb8d091f-... (uid 56d1d4ae-...)
exactly one virt-launcher pod matches the fence podSelector
=== 3/10 exec inside the VM ===
Linux (none) 6.12.114-0-virt #1-Alpine SMP PREEMPT_DYNAMIC x86_64 Linux
uid=10001(sandbox) gid=10001(sandbox)
init
=== 4/10 no gateway material in the workload environment ===
environment clean (no BOUNDARY/PRIVATE/TOKEN/OPENSHELL_ENDPOINT vars)
=== 5/10 L4 allow/deny and L7 HTTPS GET allow / POST deny ===
L4 ALLOW: connected
L4 DENY: connect to example.org:80 blocked
<!doctype html>...(Example Domain page over HTTPS)...
L7 GET over HTTPS allowed (per-sandbox proxy CA trusted in the guest)
L7 POST denied (HTTP 403)
=== 6/10 direct egress from the guest is blocked ===
direct egress to 1.1.1.1:443 blocked (no policy allow; the fence would also block it at the launcher pod)
=== 7/10 stop -> start -> Ready with VM and fence UIDs unchanged ===
✓ Stopped sandbox kvpoc-demo
✓ Started sandbox kvpoc-demo
VM uid unchanged (12611f43-...); fence uid unchanged (56d1d4ae-...)
one current-generation instance Secret (previous generation pruned)
=== 8/10 fence verification: spec drift fails start, label-only edit passes ===
✓ Stopped sandbox kvpoc-demo
spec drift rejected: ... spec drifted from the rendered shape (podSelector, policyTypes, or egress rules changed)
✓ Started sandbox kvpoc-demo
label-only fence edit accepted (resourceVersion moved, spec unchanged)
=== 9/10 main-process exit -> Error ===
  Last exit code: 137
main process exit observed through the boundary session (Error, exit code 137)
=== 10/10 delete -> zero leftover managed objects ===
✓ Sandbox kvpoc-demo deletion accepted; cleanup is pending
zero leftover vm/vmi/secret/pvc/netpol for sandbox cb8d091f-...
demo complete: Ready, exec, L4/L7 policy, bypass blocked, stop/start,
fence verification, dead-runtime Error, and clean teardown all verified
```

Stop semantics: stop waits for the VM's ready flag to clear; the VMI and
virt-launcher pod can take another ~30 seconds to terminate, during which
the fence (owner-referenced to the VM) still applies.

Start semantics: the plan is decided from the run strategy, the VM status,
and the paired VMI's phase. A halted VM boots with a single `Once` patch
(`PatchOnce`). A live or booting VMI makes start idempotent
(`AlreadyRunning`). A terminal VMI under `Once` is bounced through
`Halted` first. The create holds the VM `Halted` while it captures the UID
and builds the companions (fence, Secret, instance registration) — during
that window the VM carries the `openshell.ai/kubevirt-create-in-flight`
annotation so the watch reports `Bootstrapping` (Provisioning) rather than
`Suspended` (Stopped, which would withdraw the just-registered instance).

### The demo policy and the busybox binary-identity gotcha

`demo-policy.yaml` names `/usr/bin/busybox` in its `binaries` lists, not
`/usr/bin/nc` or `/usr/bin/wget`: busybox applets all execute as
`/usr/bin/busybox`, so a binary-scoped rule must name the resolved
executable path, and naming busybox grants every applet the file-system
policy allows to run. This is the KubeVirt guest image's version of the
binary-identity caveat from the first review.

Run `e2e/kubevirt/demo.sh` (without `--demo`) to keep the gateway in the
foreground and drive the CLI by hand; it registers gateway `kvpoc`
(`https://127.0.0.1:18085`, mTLS) as the CLI's active gateway.

### Inspecting a running sandbox

```shell
kubectl get vm,vmi,pod,secret,pvc -n kvpoc-sandboxes
# serial console (guest boot + boundary logs):
kubectl exec -n kvpoc-sandboxes $(kubectl get pod -l kubevirt.io=virt-launcher \
  -n kvpoc-sandboxes -o jsonpath='{.items[0].metadata.name}') -c compute -- \
  cat /var/run/kubevirt-private/$(kubectl get vmi default--kvpoc-demo \
  -n kvpoc-sandboxes -o jsonpath='{.metadata.uid}')/virt-serial0-log | tail -30
```

## Teardown

```shell
# remove sandbox resources (if any)
kubectl delete vm,secret,pvc,netpol -n kvpoc-sandboxes --all
# remove the shared supervisor
kubectl delete namespace openshell
# remove the whole cluster
k3d cluster delete kvpoc
```

The demo gateway state lives in `.cache/gateway-kubevirt/` (sqlite DB + TLS
material); delete it to fully reset.

## Known gaps / follow-ups

- **Image identity (digest pinning, option A)**: the first boot uses the
  configured `guest_image` tag (the digest does not exist until something
  pulls it — the documented first-generation window). Once the launcher
  Pod has pulled the guest image, the driver pins the VM spec's
  containerDisk and kernelBoot images to the resolved
  `repo@sha256:<manifest digest>` on its 30-second reconcile tick, so
  starts after a tag move boot and claim the same digest, and a new
  create after the retag gets the new image. Side-loaded images (the
  k3d/ctr import path this demo uses) report a bare `sha256:` config
  digest with no repository, which is not pullable: those are never
  pinned — the tag stays and the sandbox surfaces an
  `ImagePin: False (NoRepositoryDigest)` condition. Registry-backed
  guest images (pulled from a real registry) pin cleanly.
- An unpullable guest image maps the sandbox to `Error` only after two
  minutes of pull backoff (`ImagePullBackOff` with a stale VMI
  not-ready condition): kubelet retries with exponential backoff, so an
  earlier verdict would flip slow-but-working registry pulls to `Error`.
  Below the threshold the state stays transient.
- The dedicated topology creates the namespace-wide supervisor egress
  policy (`openshell-sandbox-supervisors`, the supervisor half of the pod
  path's workload fence) once per namespace; it is shared infrastructure
  and is deliberately not deleted with any one sandbox. The per-sandbox
  virt-launcher fence plays the workload role, so the namespace-wide
  workload policy is not created for KubeVirt sandboxes.
- Single (shared) workspace mode; managed/operator modes are rejected.
- No Helm chart / RBAC changes for the gateway itself: the PoC gateway runs
  on the host with the cluster-admin kubeconfig. A real deployment needs
  VM/VMI/PVC/Secret/NetworkPolicy rules added to the chart's
  Role/ClusterRole (the shared supervisor *is* rendered from the chart).
- Networking: masquerade pod network only; the guest clamps its MTU to 1400
  in `guest-init.sh` because the k3d flannel-vxlan pod network is 1450.
- No GPU, no live-migration/VMI-replacement semantics.
- A create interrupted between the Halted VM creation and its `Once` patch
  leaves the VM with the in-flight annotation; after ten minutes the driver
  maps it to a terminal create timeout (public `Error`), so it is visible
  and stoppable even when the gateway was restarted from a clean database
  and its own provisioning-deadline reclaim has no record to act on (the
  stop clears the annotation).
- Upstream context: kubernetes-sigs/agent-sandbox#1157 proposes a
  `runtimeBackend: VirtualMachine` Sandbox CR extension (unmerged POC fork).
  If that lands, the driver could switch from direct VM creation back to the
  Sandbox CR with a VM workload.

## Claims model (option A, user decision)

The KubeVirt boundary claims model follows the VM driver's approach:

- **Claimed:** `kubevirt.vm_uid` (the VirtualMachine's real UID — the
  create builds the VM `Halted` first, captures its UID, and only then
  renders the boundary claims), `kubevirt.generation`, and
  `kubevirt.image_identity`. The dedicated topology additionally claims
  `kubevirt.supervisor_pod_uid` — the dedicated supervisor Pod's UID,
  mirroring the pod path's `kubernetes.supervisor_pod_uid` and re-bound on
  every start (each generation gets a fresh Pod). No control-policy UID is
  claimed in either topology.
- **Evidence:** the virt-launcher egress NetworkPolicy's UID and
  resourceVersion, projected against the generation via
  `ProvisioningEvidence::from_enforcement_evidence`. The expected fence
  (UID + rendered spec) is persisted on the VM at create; every start
  re-reads the NetworkPolicy and fails when the fence was deleted,
  replaced (UID changed), or its spec drifted. A resourceVersion-only
  change is accepted and re-captured.
- **Why the virt-launcher pod UID cannot be claimed:** KubeVirt creates
  that pod asynchronously from the VM template, so its UID does not exist
  when the boundary config is rendered.

Who verifies the fence:

- the driver attests it by re-reading the NetworkPolicy each generation
  (`verify_fence` / `check_fence`), and the watch re-verifies every running
  sandbox on a 30-second interval, re-creating a drifted, replaced, or
  deleted fence from the persisted expectation and surfacing a
  `FenceRestored` condition;
- a namespace-wide default-deny NetworkPolicy
  (`openshell-kubevirt-launcher-isolation`) selects every virt-launcher pod
  in the sandbox namespace, so isolation never depends on a per-sandbox
  object's lifecycle — the per-sandbox fence and control policy only add
  allowances on top of that floor;
- inherent limitation: Kubernetes NetworkPolicy is additive, so an
  out-of-band policy that selects the launcher pod and allows more egress
  reopens egress, and a start still passes (the verification checks this
  driver's fence, not every policy in the namespace). The default-deny
  floor and the reconciler narrow the window to deliberate action; policy
  governance over the namespace is the production answer;
- because the expected fence spec is re-rendered from the *current*
  supervisor Service selector and port, a Helm upgrade that changes the
  supervisor's labels or port makes every existing KubeVirt sandbox fail
  start with a spec-drift Precondition error — fail-closed by design, so
  stop and restart sandboxes after such an upgrade;
- the boundary checks the evidence's generation binding
  (`contract.rs:254`);
- descriptor/config consistency is enforced at
  `isolation_protocol.rs:100` and on confirmation (`runtime.rs:757`);
- neither the boundary nor the supervisor can independently query the
  cluster.
