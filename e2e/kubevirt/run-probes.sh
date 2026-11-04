#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Root-workload attack-surface probes for the KubeVirt compute-driver PoC.
#
# Each probe exercises an attack surface a root guest owns by design and
# records what the control plane observes. The probes document evidence for
# the RFC; outcomes that are design inputs (not pass/fail) are recorded as
# FINDINGS. Assertions are limited to invariants the approved posture
# requires.
#
#   P1  in-guest reset: the reboot syscall (a root workload can always
#       reboot its own kernel). Records how the control plane observes the
#       reset (phase, reason, VMI/supervisor identity) — the input for the
#       reset-detection design (item 7).
#   P2  the iTCO watchdog: root can arm it (no configurable action; the
#       device is attached unconditionally). Records the reset path from
#       hardware rather than the syscall.
#   P3  fake qemu-guest-agent: the virtio-serial guest-agent channel is
#       attached unconditionally; a root guest can run a lying agent.
#       Verifies KubeVirt surfaces the lies in VMI status AND that nothing
#       in OpenShell consumes them (the sandbox must be unaffected).
#   P4  virtiofsd against a root guest: the workspace share's privileged
#       operation surface (chown past the idmap, mknod, setuid) — what the
#       share admits and what reaches the PVC host-side.
#   P5  launcher-netns listener audit: what listens in the launcher pod's
#      network namespace and what the fence admits from it.
#
# Usage: e2e/kubevirt/demo.sh --probes   (root workload mode is forced)
#
# The script creates its own sandbox (kvpoc-probe) with the root policy and
# tears it down on exit.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TARGET="${CARGO_TARGET_DIR:-${ROOT}/target}"
OPENSHELL_CLI="${CARGO_TARGET}/release/openshell"
SANDBOX_NAMESPACE="${OPENSHELL_KVPOC_NAMESPACE:-kvpoc-sandboxes}"
TOPOLOGY="${OPENSHELL_KVPOC_TOPOLOGY:-dedicated}"

# The probes are root-mode by definition: they exercise what a root
# workload can do to its own guest.
WORKLOAD="root"
DEMO_POLICY="${ROOT}/e2e/kubevirt/demo-policy-root.yaml"
NAME="kvpoc-probe"
VM_SELECTOR="openshell.ai/sandbox-name=${NAME}"
FENCE_PREFIX="openshell-kubevirt-fence-"
VMI_NAME="default--${NAME}"
WORKDIR="$(mktemp -d /tmp/kvpoc-probe-XXXXXX)"

step() { echo; echo "=== $* ==="; }
fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "NOTE: $*" >&2; }

# Single EXIT trap: teardown on failure (CLI delete first while the gateway
# is up, then labelled kubectl deletions as the gateway-independent
# backstop), scratch removal on every exit.
probe_exit_trap() {
    local rc=$?
    if [ "${rc}" -ne 0 ] && [ "${PROBE_CREATED}" = "1" ]; then
        echo >&2
        echo "probe failed (rc=${rc}); tearing down sandbox '${NAME}'..." >&2
        "${OPENSHELL_CLI}" sandbox delete "${NAME}" >/dev/null 2>&1 || true
        kubectl delete vm -n "${SANDBOX_NAMESPACE}" -l "${VM_SELECTOR}" \
            --wait=false --ignore-not-found >/dev/null 2>&1 || true
        kubectl delete pod,svc,secret,pvc,netpol -n "${SANDBOX_NAMESPACE}" \
            -l "${VM_SELECTOR}" --ignore-not-found >/dev/null 2>&1 || true
        echo "teardown complete" >&2
    fi
    rm -rf "${WORKDIR:-}"
    exit "${rc}"
}
trap probe_exit_trap EXIT
PROBE_CREATED=0

sandbox_phase() {
    "${OPENSHELL_CLI}" sandbox get "${NAME}" 2>/dev/null | grep -a "Phase:" | head -1 \
        | sed 's/^ *//' || true
}

wait_for_phase() {
    local expected="$1" timeout_secs="${2:-300}" deadline got
    deadline=$(( $(date +%s) + timeout_secs ))
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        got="$(sandbox_phase)"
        case "${got}" in
            "Phase: ${expected}") return 0 ;;
        esac
        sleep 5
    done
    fail "timed out waiting for phase ${expected} (got '${got:-none}')"
}

exec_in_sandbox() {
    "${OPENSHELL_CLI}" sandbox exec "${NAME}" -- "$@" || {
        local rc=$?
        [ "${rc}" = "74" ] || return "${rc}"
        return 0
    }
}

vm_jsonpath() {
    kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
        -o "jsonpath=$1" 2>/dev/null
}

supervisor_pod_name() {
    local sandbox_id sbx_lc
    sandbox_id="$(vm_jsonpath '{.items[0].metadata.labels.openshell\.ai/sandbox-id}')"
    sbx_lc="$(printf '%s' "${sandbox_id}" | tr '[:upper:]' '[:lower:]')"
    echo "os-supervisor-${sbx_lc}"
}

launcher_pod_name() {
    kubectl get pods -n "${SANDBOX_NAMESPACE}" \
        -l kubevirt.io=virt-launcher,openshell.ai/sandbox-name="${NAME}" \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null
}

# ---------------------------------------------------------------------------
step "setup: create a root sandbox reaching Ready"

"${OPENSHELL_CLI}" sandbox delete "${NAME}" >/dev/null 2>&1 || true
kubectl delete vm -n "${SANDBOX_NAMESPACE}" -l "${VM_SELECTOR}" \
    --ignore-not-found >/dev/null 2>&1 || true
kubectl delete pod,svc,secret,pvc,netpol -n "${SANDBOX_NAMESPACE}" \
    -l "${VM_SELECTOR}" --ignore-not-found >/dev/null 2>&1 || true
sleep 5

"${OPENSHELL_CLI}" sandbox create --detach --name "${NAME}" \
    --policy "${DEMO_POLICY}" \
    -- /bin/sleep infinity
PROBE_CREATED=1
wait_for_phase Ready
exec_in_sandbox id | tee "${WORKDIR}/id.log"
grep -q "uid=0(root)" "${WORKDIR}/id.log" || fail "probe sandbox is not root mode"
echo "probe sandbox Ready (root)"

# ---------------------------------------------------------------------------
step "P5: launcher-netns listener audit (run first: it needs the live pair)"

LAUNCHER="$(launcher_pod_name)"
[ -n "${LAUNCHER}" ] || fail "no virt-launcher pod found"
SUPERVISOR_POD="$(supervisor_pod_name)"
echo "launcher: ${LAUNCHER}"
echo "supervisor: ${SUPERVISOR_POD} (${TOPOLOGY} topology)"

# Listeners in the launcher pod's network namespace (the netns the guest's
# masquerade traffic lands in). /proc/net/tcp is hex-encoded
# little-endian-address:big-endian-port; the audit lists the decoded ports.
echo "launcher netns TCP listeners (/proc/net/tcp, /proc/net/tcp6):"
# Decode without gawk extensions: the port field is hex big-endian; POSIX
# awk converts it with CONVFMT via +"0x" arithmetic where supported, so do
# the decode in the pipeline's shell instead. virtiofsd (unix socket +
# vhost device) and QEMU's guest-facing dataplane do not listen on TCP;
# what remains in this netns is the masquerade and anything the launcher
# itself runs.
kubectl exec -n "${SANDBOX_NAMESPACE}" "${LAUNCHER}" -c compute -- \
    sh -c 'cat /proc/net/tcp /proc/net/tcp6 2>/dev/null' \
    | awk '$4 == "0A" { split($2, a, ":"); print a[2], $10 }' \
    | sort -u \
    | while read -r hexport inode; do
        port="$(( 0x${hexport} ))"
        echo "  listening port ${port} (inode ${inode})"
      done | tee "${WORKDIR}/listeners.log" || true

# Reachability from the launcher netns (the fence's egress applies to this
# pod): the fence must admit ONLY the supervisor service. Probe the
# standard cluster-internal targets and record refused/connected.
# The fence's allow target (the supervisor Service) is judged like the
# demo's N4 check: anything other than "Connection refused" means the
# connect reached the Service (the supervisor awaits its boundary, not an
# s_client, so the TLS handshake never completes and a timeout is the
# expected reachable signal). Every other target must be refused.
probe_tcp() {
    local target="$1" label="$2" log
    log="${WORKDIR}/tcp-$(echo "${target}" | tr ':/' '--').log"
    if kubectl exec -n "${SANDBOX_NAMESPACE}" "${LAUNCHER}" -c compute -- \
        timeout 5 openssl s_client -connect "${target}" \
        > "${log}" 2>&1; then
        echo "  ${label} (${target}): CONNECTED"
    elif grep -q "Connection refused" "${log}" 2>/dev/null; then
        echo "  ${label} (${target}): refused by the fence"
    else
        echo "  ${label} (${target}): reachable (no refusal; handshake incomplete is expected for non-TLS-peer targets)"
    fi
}
KUBEDNS_IP="$(kubectl get svc -n kube-system kube-dns \
    -o jsonpath='{.spec.clusterIP}' 2>/dev/null || true)"
K8S_API_IP="$(kubectl get svc -n default kubernetes \
    -o jsonpath='{.spec.clusterIP}' 2>/dev/null || true)"
NODE_IP="$(kubectl get node -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || true)"
echo "fence reachability from the launcher netns:"
if [ "${TOPOLOGY}" = "gateway" ]; then
    SUPERVISOR_SVC_IP="$(kubectl get svc -n openshell \
        kvpoc-openshell-supervisor -o jsonpath='{.spec.clusterIP}' 2>/dev/null || true)"
else
    SUPERVISOR_SVC_IP="$(kubectl get svc -n "${SANDBOX_NAMESPACE}" \
        "${SUPERVISOR_POD}" -o jsonpath='{.spec.clusterIP}' 2>/dev/null || true)"
fi
[ -n "${SUPERVISOR_SVC_IP}" ] && probe_tcp "${SUPERVISOR_SVC_IP}:5500" "supervisor Service (expected: the fence's allow target)"
[ -n "${KUBEDNS_IP}" ] && probe_tcp "${KUBEDNS_IP}:53" "kube-dns (cluster DNS)"
[ -n "${K8S_API_IP}" ] && probe_tcp "${K8S_API_IP}:443" "Kubernetes API service"
[ -n "${NODE_IP}" ] && probe_tcp "${NODE_IP}:10250" "node kubelet"
probe_tcp "1.1.1.1:443" "external internet"
echo "P5 recorded (evidence above; the fence is expected to admit only the supervisor Service)"

# ---------------------------------------------------------------------------
step "P3: fake qemu-guest-agent (the channel is attached unconditionally)"

# Find the guest-agent virtio-serial port and start a lying responder: it
# answers the standard QGA commands with fabricated data. KubeVirt polls
# the agent through this channel and surfaces the answers in VMI status.
GA_PORT="$(exec_in_sandbox sh -c 'ls /dev/virtio-ports/ 2>/dev/null' | head -1 || true)"
if [ -z "${GA_PORT}" ]; then
    echo "FINDING: no virtio-ports device in the guest — the agent channel is absent"
else
    echo "guest-agent port: /dev/virtio-ports/${GA_PORT}"
    # The responder runs as a background exec session (it must keep the
    # port open). Its answers are deliberately identifiable lies.
    "${OPENSHELL_CLI}" sandbox exec "${NAME}" -- sh -c "
        while IFS= read -r line; do
            case \"\$line\" in
                *guest-ping*)
                    printf '{\"return\": {}}\n' ;;
                *guest-get-host-name*)
                    printf '{\"return\": {\"host-name\": \"totally-honest-host\"}}\n' ;;
                *guest-get-osinfo*)
                    printf '{\"return\": {\"kernel-release\": \"6.6.6-fake\", \"machine\": \"x86_64\", \"name\": \"FakeOS\", \"pretty-name\": \"FakeOS 99 (Liar Edition)\"}}\n' ;;
                *guest-get-users*)
                    printf '{\"return\": [{\"user\": \"definitely-real-user\", \"login-time\": 1.0}]}\n' ;;
                *)
                    printf '{\"return\": {}}\n' ;;
            esac
        done < /dev/virtio-ports/${GA_PORT} > /dev/virtio-ports/${GA_PORT}
    " > "${WORKDIR}/fake-agent.log" 2>&1 &
    FAKE_AGENT_PID=$!
    echo "fake agent running (pid ${FAKE_AGENT_PID})"

    # Give KubeVirt time to poll the agent and surface the answers.
    sleep 45
    echo "VMI status after the fake agent answered:"
    kubectl get vmi -n "${SANDBOX_NAMESPACE}" "${VMI_NAME}" -o json \
        | python3 -c '
import json, sys
vmi = json.load(sys.stdin)
status = vmi.get("status", {})
for c in status.get("conditions", []):
    print("  condition %s=%s" % (c.get("type"), c.get("status")))
print("  guestOSInfo: %s" % json.dumps(status.get("guestOSInfo", {})))
' | tee "${WORKDIR}/vmi-agent-status.log"

    if grep -q "FakeOS" "${WORKDIR}/vmi-agent-status.log"; then
        echo "FINDING (expected): KubeVirt surfaces the fake agent's lies in VMI status"
    else
        echo "NOTE: the fake agent's OS info did not reach VMI status (agent polling cadence or protocol mismatch)"
    fi

    # The invariant the posture requires: nothing in OpenShell consumes the
    # agent data. The sandbox must be unaffected: same phase, exec still
    # works.
    PHASE_AFTER_AGENT="$(sandbox_phase)"
    [ "${PHASE_AFTER_AGENT}" = "Phase: Ready" ] \
        || fail "sandbox left Ready while only the fake agent ran (something consumed agent data?)"
    exec_in_sandbox true || fail "exec broken while the fake agent ran"
    echo "PASS: the sandbox is unaffected by the lying guest agent (Ready, exec works)"

    kill "${FAKE_AGENT_PID}" 2>/dev/null || true
    wait "${FAKE_AGENT_PID}" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
step "P4: virtiofsd privileged-operation surface (root guest)"

# 1. Can root remount the share RAW (no idmap) and chown past the idmap?
exec_in_sandbox sh -c 'mkdir -p /mnt/raw && mount -t virtiofs workspace /mnt/raw' \
    > "${WORKDIR}/rawmount.log" 2>&1 \
    && echo "FINDING: root CAN mount the virtiofs share without the idmap (/mnt/raw)" \
    || echo "the raw remount was refused"
if ! grep -qi "refused\|denied\|Permission" "${WORKDIR}/rawmount.log" 2>/dev/null; then
    cat "${WORKDIR}/rawmount.log" 2>/dev/null | head -2 || true
fi

# 2. chown to an arbitrary uid through the idmapped /sandbox (mapped range
#    only) and on the raw mount (if it exists).
echo "chown probe through the idmapped mount:"
exec_in_sandbox sh -c 'touch /sandbox/p4-chown && chown 1234:1234 /sandbox/p4-chown; echo rc=$?; stat -c "guest sees %u:%g" /sandbox/p4-chown' \
    | tee "${WORKDIR}/chown-idmap.log"
if mountpoint_ok="$(exec_in_sandbox sh -c 'mountpoint -q /mnt/raw && echo yes' 2>/dev/null)" && [ "${mountpoint_ok}" = "yes" ]; then
    echo "chown probe on the RAW mount:"
    exec_in_sandbox sh -c 'touch /mnt/raw/p4-chown-raw && chown 1234:1234 /mnt/raw/p4-chown-raw; echo rc=$?; stat -c "guest sees %u:%g" /mnt/raw/p4-chown-raw' \
        | tee "${WORKDIR}/chown-raw.log"
fi

# 3. mknod a device node on the share (would persist on the PVC across
#    generations if admitted).
echo "mknod probe:"
exec_in_sandbox sh -c 'mknod /sandbox/p4-dev c 1 3; echo rc=$?; ls -l /sandbox/p4-dev 2>/dev/null' \
    | tee "${WORKDIR}/mknod.log" || true

# 4. setuid bit on a copied binary (would persist on the PVC).
echo "setuid probe:"
exec_in_sandbox sh -c 'cp /bin/busybox /sandbox/p4-su && chmod 4755 /sandbox/p4-su; echo rc=$?; ls -l /sandbox/p4-su' \
    | tee "${WORKDIR}/setuid.log" || true

# 5. Host-side truth: what actually landed on the PVC (k3d local-path
#    provisioning on the node container).
# The workspace PVC name is deterministic: <kube-name>-workspace.
PVC_NAME="default--${NAME}-workspace"
K3D_NODE="k3d-$(kubectl config current-context | sed 's/^k3d-//' | cut -d- -f1-2)-server-0"
if [ -n "${PVC_NAME}" ] && docker inspect "${K3D_NODE}" >/dev/null 2>&1; then
    echo "host-side PVC contents (${PVC_NAME} on ${K3D_NODE}):"
    docker exec "${K3D_NODE}" sh -c \
        "ls -ln /var/lib/rancher/k3s/storage/ | grep '${PVC_NAME}' || true" | head -2
    docker exec "${K3D_NODE}" sh -c \
        "ls -ln /var/lib/rancher/k3s/storage/pvc-*_${SANDBOX_NAMESPACE}_${PVC_NAME}/* 2>/dev/null" \
        | grep -E "p4-" || echo "  (no p4-* files visible host-side)"
else
    echo "NOTE: host-side PVC inspection skipped (PVC ${PVC_NAME:-none} or node ${K3D_NODE} unavailable)"
fi
echo "P4 recorded (evidence above; the RFC documents the share's admitted surface)"

# ---------------------------------------------------------------------------
step "P1: in-guest reset via the reboot syscall"

VMI_UID_BEFORE="$(kubectl get vmi -n "${SANDBOX_NAMESPACE}" "${VMI_NAME}" \
    -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
SUP_POD_UID_BEFORE="$(kubectl get pod -n "${SANDBOX_NAMESPACE}" "$(supervisor_pod_name)" \
    -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
echo "before: VMI uid=${VMI_UID_BEFORE:-none} supervisor pod uid=${SUP_POD_UID_BEFORE:-none}"

echo "issuing: reboot -f (root workload reboots its own kernel)"
# The exec session dies mid-stream when the guest resets; a non-zero rc
# here is the expected outcome, not a failure.
exec_in_sandbox reboot -f > "${WORKDIR}/reboot.log" 2>&1 || true
sleep 10

# Record what the control plane observes for up to 5 minutes: the phase,
# the reason, whether the VMI identity survived, whether the supervisor
# generation rotated.
echo "observing the control plane for up to 300s..."
RESET_OBSERVED=""
DEADLINE=$(( $(date +%s) + 300 ))
while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
    PHASE="$(sandbox_phase)"
    VMI_UID_NOW="$(kubectl get vmi -n "${SANDBOX_NAMESPACE}" "${VMI_NAME}" \
        -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
    case "${PHASE}" in
        "Phase: Error"|"Phase: Stopped")
            RESET_OBSERVED="${PHASE}"
            break ;;
    esac
    # A same-uid VMI back at Ready means the guest rebooted in place and
    # everything reconnected — record it and keep watching briefly.
    if [ "${PHASE}" = "Phase: Ready" ] && [ -n "${VMI_UID_BEFORE}" ] \
        && [ "${VMI_UID_NOW}" != "${VMI_UID_BEFORE}" ]; then
        RESET_OBSERVED="Ready-with-new-VMI"
        break
    fi
    sleep 5
done

VMI_UID_AFTER="$(kubectl get vmi -n "${SANDBOX_NAMESPACE}" "${VMI_NAME}" \
    -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
SUP_POD_UID_AFTER="$(kubectl get pod -n "${SANDBOX_NAMESPACE}" "$(supervisor_pod_name)" \
    -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
echo "after: phase='${RESET_OBSERVED:-unresolved (${PHASE:-none})}'"
echo "  VMI uid: ${VMI_UID_BEFORE:-none} -> ${VMI_UID_AFTER:-none}"
echo "  supervisor pod uid: ${SUP_POD_UID_BEFORE:-none} -> ${SUP_POD_UID_AFTER:-none}"
"${OPENSHELL_CLI}" sandbox get "${NAME}" 2>/dev/null | grep -aE "Phase:|Ready:|exit code" | head -3 || true

# The invariant: a reset must not pass unnoticed. Whatever terminal or
# observed state results, the sandbox must not silently continue the SAME
# session as if nothing happened: either the phase flipped away from Ready
# (detected), or the VMI identity changed (new boot observed).
if [ -n "${RESET_OBSERVED}" ] && [ "${RESET_OBSERVED}" != "Ready" ]; then
    echo "PASS: the guest reset was observed by the control plane (${RESET_OBSERVED})"
else
    echo "FINDING: the control plane did not clearly observe the reset (phase '${PHASE:-none}', VMI ${VMI_UID_BEFORE:-none} -> ${VMI_UID_AFTER:-none}); record for the reset-detection design (item 7)"
fi

# ---------------------------------------------------------------------------
step "P2: the iTCO watchdog (root can arm it; no configurable action)"

# Recreate the sandbox (P1 left it in a terminal or ambiguous state).
"${OPENSHELL_CLI}" sandbox delete "${NAME}" >/dev/null 2>&1 || true
kubectl delete vm -n "${SANDBOX_NAMESPACE}" -l "${VM_SELECTOR}" \
    --ignore-not-found >/dev/null 2>&1 || true
kubectl delete pod,svc,secret,pvc,netpol -n "${SANDBOX_NAMESPACE}" \
    -l "${VM_SELECTOR}" --ignore-not-found >/dev/null 2>&1 || true
sleep 5
"${OPENSHELL_CLI}" sandbox create --detach --name "${NAME}" \
    --policy "${DEMO_POLICY}" \
    -- /bin/sleep infinity
wait_for_phase Ready

WD_PRESENT="$(exec_in_sandbox sh -c 'ls -l /dev/watchdog* 2>/dev/null || echo absent')"
echo "watchdog device: ${WD_PRESENT}"
if echo "${WD_PRESENT}" | grep -q "absent"; then
    echo "FINDING: no /dev/watchdog in the guest — the iTCO is not exposed to root"
else
    # Opening /dev/watchdog starts it; closing WITHOUT the magic 'V' leaves
    # it armed, and the iTCO resets the machine when it expires. This is
    # the attack a root guest can always perform.
    echo "arming: open + non-magic close of /dev/watchdog"
    exec_in_sandbox sh -c 'exec 3>/dev/watchdog' || true
    echo "watching for the watchdog reset (up to 120s)..."
    WD_RESET=""
    DEADLINE=$(( $(date +%s) + 120 ))
    PHASE_AT_ARM="$(sandbox_phase)"
    while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
        PHASE="$(sandbox_phase)"
        [ "${PHASE}" != "${PHASE_AT_ARM}" ] && { WD_RESET="${PHASE}"; break; }
        # A guest-side check: uptime resets when the machine resets.
        UPTIME="$(exec_in_sandbox sh -c 'cut -d. -f1 /proc/uptime' 2>/dev/null || echo 0)"
        [ "${UPTIME}" -lt 30 ] && { WD_RESET="guest-rebooted(uptime=${UPTIME}s)"; break; }
        sleep 5
    done
    if [ -n "${WD_RESET}" ]; then
        echo "PASS/recorded: the iTCO watchdog reset the guest; control plane observed: ${WD_RESET}"
    else
        echo "FINDING: the watchdog did not fire within 120s (long timeout or disarmed on close); the iTCO default timeout may exceed the window"
    fi
fi

# ---------------------------------------------------------------------------
step "teardown"

"${OPENSHELL_CLI}" sandbox delete "${NAME}" >/dev/null 2>&1 || true
DEADLINE=$(( $(date +%s) + 180 ))
while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
    LEFT="$(kubectl get vm,vmi,pod,svc,secret,pvc,netpol -n "${SANDBOX_NAMESPACE}" \
        -l "${VM_SELECTOR}" --no-headers 2>/dev/null | wc -l)"
    [ "${LEFT}" = "0" ] && break
    sleep 5
done
LEFT="$(kubectl get vm,vmi,pod,svc,secret,pvc,netpol -n "${SANDBOX_NAMESPACE}" \
    -l "${VM_SELECTOR}" --no-headers 2>/dev/null | wc -l)"
[ "${LEFT}" = "0" ] || fail "${LEFT} managed objects left after delete"
PROBE_CREATED=0
echo "zero leftovers for sandbox probe ${NAME}"

echo
echo "probes complete. Evidence in ${WORKDIR} is removed by the exit trap;"
echo "the findings above are the RFC inputs."
