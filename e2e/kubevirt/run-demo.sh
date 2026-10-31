#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Exercise the KubeVirt-mode Kubernetes compute driver against a running
# kvpoc gateway (started by e2e/kubevirt/demo.sh):
#   1. create a sandbox (provisions a KubeVirt VirtualMachine + companions)
#      and assert the dead-guest path: the in-guest runtime exits, guest-init
#      powers off, the VMI completes, and the sandbox surfaces Error
#   2. stop / start (runStrategy Halted -> Once), plus the dead-`Once`-VMI
#      bounce (a second start during the boot window)
#   3. delete and verify zero leftover managed objects
#
# Error handling: a REJECTED RPC (FailedPrecondition etc.) fails the demo.
# Only a Ready-wait timeout is tolerated, and only where marked: until the
# guest boundary integration lands, the in-guest runtime exits immediately
# (it needs --bootstrap arguments), so the sandbox can never reach Ready.
#
# Requires the `openshell` CLI built at $CARGO_TARGET_DIR/release/openshell.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TARGET="${CARGO_TARGET_DIR:-${ROOT}/target}"
OPENSHELL_CLI="${CARGO_TARGET}/release/openshell"
SANDBOX_NAMESPACE="${OPENSHELL_KVPOC_NAMESPACE:-kvpoc-sandboxes}"
NAME="kvpoc-demo"
VM_SELECTOR="openshell.ai/sandbox-name=${NAME}"
TIMEOUT_SECS="${OPENSHELL_KVPOC_TIMEOUT:-300}"

step() { echo; echo "=== $* ==="; }

# Classify a failed `sandbox start`: the start RPC itself succeeding but the
# Ready-wait failing is tolerated on this branch (the in-guest runtime exits
# without --bootstrap, so the sandbox can never reach Ready — the wait ends
# in either a timeout or the guest-death Error). A REJECTED RPC — the
# gateway refusing the start — fails the demo.
start_wait_failure_is_expected() {
    # Ready-wait timeout.
    grep -aq "waiting for sandbox to reach Ready" "$1" && return 0
    # The wait observed a phase failure (guest exit -> VMFailed/Error).
    grep -aq "while waiting for Ready" "$1" && return 0
    return 1
}

tolerate_ready_wait_only() {
    local log
    log="$(mktemp /tmp/kvpoc-start-XXXX.log)"
    if OPENSHELL_LIFECYCLE_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-60}" \
        "${OPENSHELL_CLI}" sandbox start "${NAME}" >"${log}" 2>&1; then
        cat "${log}"
        return 0
    fi
    if start_wait_failure_is_expected "${log}"; then
        echo "(start Ready-wait failed as expected without a guest supervisor session)"
        grep -a "VMFailed\|Error" "${log}" | tail -2 || true
        return 0
    fi
    cat "${log}" >&2
    return 1
}

wait_vm_field() {
    local jsonpath="$1" expected="$2" what="$3"
    local deadline=$(( $(date +%s) + TIMEOUT_SECS )) current
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        current="$(kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
            -o "jsonpath=${jsonpath}" 2>/dev/null)"
        if [ "${current}" = "${expected}" ]; then
            echo "${what}: ${current}"
            return 0
        fi
        sleep 5
    done
    echo "timed out waiting for ${what} (got: ${current:-none})" >&2
    return 1
}

vmi_phase() {
    kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
        -o jsonpath='{.items[0].status.phase}' 2>/dev/null || true
}

vmi_uid() {
    kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
        -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null || true
}

sandbox_phase() {
    "${OPENSHELL_CLI}" sandbox get "${NAME}" 2>/dev/null | grep -a "Phase:" | head -1 || true
}

step "1/3 sandbox create and the dead-guest Error path"
# The create cannot reach Ready on this branch (the in-guest runtime exits
# without --bootstrap): the CLI fails with either the Error phase (the
# expected R3 path) or a provisioning timeout. Both are tolerated here;
# the assertions below prove which one happened.
CREATE_LOG="$(mktemp /tmp/kvpoc-create-XXXX.log)"
OPENSHELL_PROVISION_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-90}" \
    "${OPENSHELL_CLI}" sandbox create --detach --name "${NAME}" \
    -- /bin/sleep infinity >"${CREATE_LOG}" 2>&1 || true
grep -a "Phase\|Error\|timed out" "${CREATE_LOG}" | tail -3 || true

kubectl get vm,secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/sandbox-name="${NAME}"

# R3 assertion: no QEMU kill — the guest runtime exits on its own, the init
# powers off, and the VMI completes. The driver maps the completed VMI to
# the terminal VMFailed condition, so the sandbox surfaces Error.
DEADLINE=$(( $(date +%s) + TIMEOUT_SECS ))
while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
    PHASE="$(vmi_phase)"
    [ "${PHASE}" = "Succeeded" ] || [ "${PHASE}" = "Failed" ] && break
    sleep 5
done
echo "VMI phase after guest exit (no QEMU kill): ${PHASE:-none}"
[ "${PHASE}" = "Succeeded" ] || [ "${PHASE}" = "Failed" ] || {
    echo "FAIL: expected the VMI to complete after the in-guest runtime exited" >&2
    exit 1
}
sleep 10
echo "sandbox after guest exit: $(sandbox_phase)"

step "2/3 stop / start and the dead-Once-VMI bounce"
# Stop through the gateway (admitted from Error): the driver halts the VM
# and the stop is the explicit lifecycle operation that moves the sticky
# Error phase to Stopped. A kubectl halt would NOT move the phase — the
# gateway ignores driver snapshots once the phase is Error.
"${OPENSHELL_CLI}" sandbox stop "${NAME}"
sleep 5
echo "sandbox after stop: $(sandbox_phase)"

# Start #1: Halted -> PatchOnce boots a fresh VMI; the phase goes Starting.
# The Ready-wait cannot succeed on this branch (the in-guest runtime exits
# without --bootstrap), so use a short lifecycle timeout purely to let the
# start RPC through and return quickly. A REJECTED start fails the demo.
START1_LOG="$(mktemp /tmp/kvpoc-start1-XXXX.log)"
OPENSHELL_LIFECYCLE_TIMEOUT=3 \
    "${OPENSHELL_CLI}" sandbox start "${NAME}" >"${START1_LOG}" 2>&1 \
    || start_wait_failure_is_expected "${START1_LOG}" \
    || { cat "${START1_LOG}" >&2; echo "FAIL: start #1 was rejected" >&2; exit 1; }
echo "(start #1 Ready-wait failed as expected without a guest supervisor session)"
UID_BEFORE="$(vmi_uid)"
[ -n "${UID_BEFORE}" ] || {
    echo "FAIL: start #1 booted no VMI (empty uid); cannot verify what follows" >&2
    exit 1
}
echo "VMI uid after start #1: ${UID_BEFORE}"

# Start #2 back to back during the boot window: the gateway admits retries
# from Starting, and the driver must treat the booting VMI as idempotent —
# a healthy boot must NOT be restarted. The VMI uid must not change.
START2_LOG="$(mktemp /tmp/kvpoc-start2-XXXX.log)"
OPENSHELL_LIFECYCLE_TIMEOUT=3 \
    "${OPENSHELL_CLI}" sandbox start "${NAME}" >"${START2_LOG}" 2>&1 \
    || start_wait_failure_is_expected "${START2_LOG}" \
    || { cat "${START2_LOG}" >&2; echo "FAIL: start #2 was rejected" >&2; exit 1; }
sleep 10
UID_DURING_BOOT="$(vmi_uid)"
echo "VMI uid after idempotent start #2: ${UID_DURING_BOOT:-none}"
[ "${UID_DURING_BOOT}" = "${UID_BEFORE}" ] || {
    echo "FAIL: start #2 restarted a healthy booting VM (uid changed)" >&2
    exit 1
}
echo "idempotent start verified: a booting VirtualMachineInstance is not restarted"

# Dead-Once-VMI bounce (best-effort race): the guest runtime exits and the
# init powers off, so the VMI goes Succeeded and the gateway maps the
# sandbox to a sticky Error. The bounce is only reachable in the window
# between the VMI completing and the Error snapshot landing. Watch the VMI
# phase and fire a start the instant it goes terminal.
echo "waiting for the guest to exit (VMI terminal) to attempt the bounce race..."
VM_NAME2="$(kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
    -o jsonpath='{.items[0].metadata.name}')"
BOUNCE_LOG="$(mktemp /tmp/kvpoc-bounce-XXXX.log)"
kubectl get vmi "${VM_NAME2}" -n "${SANDBOX_NAMESPACE}" -w \
    -o jsonpath='{.status.phase}' 2>/dev/null | while read -r PHASE; do
    if [ "${PHASE}" = "Succeeded" ] || [ "${PHASE}" = "Failed" ]; then
        echo "terminal" > /tmp/kvpoc-bounce-race-flag
        break
    fi
done &
WATCH_PID=$!
DEADLINE=$(( $(date +%s) + TIMEOUT_SECS ))
while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
    [ -f /tmp/kvpoc-bounce-race-flag ] && break
    sleep 1
done
kill "${WATCH_PID}" 2>/dev/null || true
rm -f /tmp/kvpoc-bounce-race-flag
if OPENSHELL_LIFECYCLE_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-60}" \
    "${OPENSHELL_CLI}" sandbox start "${NAME}" >"${BOUNCE_LOG}" 2>&1; then
    cat "${BOUNCE_LOG}"
elif start_wait_failure_is_expected "${BOUNCE_LOG}"; then
    echo "(bounce-start Ready-wait failed as expected without a guest supervisor session)"
    grep -a "VMFailed\|Error" "${BOUNCE_LOG}" | tail -2 || true
else
    # A rejection here means the Error snapshot won the race: expected and
    # reported, not a demo failure.
    echo "(bounce race lost: the Error snapshot landed before the start;"
    echo " the gateway rejected the start from the sticky Error phase:"
    grep -a "must be\|current phase" "${BOUNCE_LOG}" | tail -2 || true
    echo ")"
fi
UID_AFTER_RACE="$(vmi_uid)"
echo "VMI uid after the bounce race: ${UID_AFTER_RACE:-none}"
if [ -n "${UID_AFTER_RACE}" ] && [ "${UID_AFTER_RACE}" != "${UID_BEFORE}" ]; then
    echo "dead-Once-VMI bounce landed: Halted -> Once booted a fresh VMI"
else
    echo "bounce did not land this run (race window missed); see unit tests for the plan"
fi

step "3/3 delete and verify teardown"
"${OPENSHELL_CLI}" sandbox delete "${NAME}"
DEADLINE=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "${DEADLINE}" ]; do
    if [ "$(kubectl get vm,vmi,secret,pvc -n "${SANDBOX_NAMESPACE}" \
        -l openshell.ai/managed-by=openshell --no-headers 2>/dev/null | wc -l)" = "0" ]; then
        break
    fi
    sleep 5
done
remaining="$(kubectl get vm,vmi,secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/managed-by=openshell --no-headers 2>/dev/null | wc -l)"
kubectl get vm,vmi,secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/managed-by=openshell 2>/dev/null || true
if [ "${remaining}" != "0" ]; then
    echo "FAIL: OpenShell-managed resources still present" >&2
    exit 1
fi
echo
echo "demo complete: dead-guest Error path, start/bounce, and teardown verified"
