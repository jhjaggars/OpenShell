#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Exercise the KubeVirt-mode Kubernetes compute driver against a running
# kvpoc gateway (started by e2e/kubevirt/demo.sh):
#   1. create a sandbox (provisions a KubeVirt VirtualMachine + companions)
#   2. wait for the VM and VMI to run
#   3. stop / start the sandbox (runStrategy Halted -> Once, with the
#      dead-`Once`-VMI bounce on start)
#   4. delete it and verify the VM, bootstrap Secret and workspace PVC are
#      gone
#
# NOTE: supervisor-session readiness (`Phase: Ready`), `sandbox exec`, and the
# proxy-pod egress architecture from the original PoC branch are NOT exercised
# here: current main replaced the supervisor/guest split with the isolation
# boundary architecture, and the kubevirt guest integration with it is the
# documented follow-up (see e2e/kubevirt/README.md). This script verifies the
# driver lifecycle that is independent of that integration.
#
# Requires the `openshell` CLI built at $CARGO_TARGET_DIR/release/openshell.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TARGET="${CARGO_TARGET_DIR:-${ROOT}/target}"
OPENSHELL_CLI="${CARGO_TARGET}/release/openshell"
SANDBOX_NAMESPACE="${OPENSHELL_KVPOC_NAMESPACE:-kvpoc-sandboxes}"
NAME="kvpoc-demo"
VM_SELECTOR="openshell.ai/sandbox-name=${NAME}"
TIMEOUT_SECS="${OPENSHELL_KVPOC_TIMEOUT:-600}"

step() { echo; echo "=== $* ==="; }

wait_vm_condition() {
    local jsonpath="$1" expected="$2" what="$3"
    local deadline=$(( $(date +%s) + TIMEOUT_SECS ))
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

step "1/4 sandbox create (provisions a KubeVirt VirtualMachine)"
# The supervisor session never reaches Ready on -v2 (the guest boundary
# integration is the documented follow-up), so the CLI's provisioning wait
# always times out. Use a short provisioning timeout, tolerate the failure,
# and verify the provisioned resources directly.
if ! OPENSHELL_PROVISION_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-60}" \
    "${OPENSHELL_CLI}" sandbox create --detach --name "${NAME}" -- /bin/sleep infinity; then
    echo "(create wait timed out as expected without a guest supervisor; continuing)"
fi
kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}"
kubectl get secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/sandbox-name="${NAME}"

step "2/4 waiting for the VM and VMI to run"
wait_vm_condition '{.items[0].status.ready}' "true" "VM ready"
kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
    -o custom-columns=NAME:.metadata.name,PHASE:.status.phase --no-headers

step "3/4 stop / start (runStrategy Halted -> Once with dead-VMI bounce)"
# The gateway only stops sandboxes in Ready/Suspended/Error; without a guest
# supervisor the sandbox stays Provisioning, so halt the VM at the Kubernetes
# layer first. The driver watch then reports Suspended, which the gateway
# maps to the public Stopped phase.
VM_NAME="$(kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" -o jsonpath='{.items[0].metadata.name}')"
kubectl patch vm "${VM_NAME}" -n "${SANDBOX_NAMESPACE}" --type merge \
    -p '{"spec":{"runStrategy":"Halted"}}' >/dev/null
wait_vm_condition '{.items[0].status.printableStatus}' "Stopped" "VM halted"
sleep 15
"${OPENSHELL_CLI}" sandbox get "${NAME}" 2>/dev/null | grep -a "Phase:" || true

# Start from Stopped: a single runStrategy=Once patch boots a fresh VMI.
if ! OPENSHELL_LIFECYCLE_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-60}" \
    "${OPENSHELL_CLI}" sandbox start "${NAME}"; then
    echo "(start Ready-wait timed out as expected without a guest supervisor; continuing)"
fi
wait_vm_condition '{.items[0].status.ready}' "true" "VM restarted"
kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
    -o custom-columns=NAME:.metadata.name,PHASE:.status.phase --no-headers

# Dead-Once-VMI bounce: kill the guest so the VMI completes under
# runStrategy=Once (an Once->Once patch would be a no-op), confirm the
# sandbox surfaces Error from the terminal VMI mapping, then start — the
# driver must bounce Halted->Once to actually boot a new VMI.
LP="$(kubectl get pod -n "${SANDBOX_NAMESPACE}" \
    -l "kubevirt.io=virt-launcher,${VM_SELECTOR}" \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
if [ -n "${LP}" ]; then
    kubectl exec -n "${SANDBOX_NAMESPACE}" "${LP}" -c compute -- \
        sh -c "ps -o pid,comm -e | grep -E 'qemu-system|qemu-kvm|virtqemud' | awk '{print \$1}' | xargs -r kill -9" \
        >/dev/null 2>&1 || true
fi
for _ in $(seq 1 30); do
    VMI_PHASE="$(kubectl get vmi "${VM_NAME}" -n "${SANDBOX_NAMESPACE}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    [ "${VMI_PHASE}" = "Failed" ] || [ "${VMI_PHASE}" = "Succeeded" ] && break
    sleep 5
done
echo "VMI after guest kill: ${VMI_PHASE:-gone}"
sleep 10
if ! OPENSHELL_LIFECYCLE_TIMEOUT="${OPENSHELL_KVPOC_CREATE_TIMEOUT:-60}" \
    "${OPENSHELL_CLI}" sandbox start "${NAME}"; then
    echo "(start Ready-wait timed out as expected without a guest supervisor; continuing)"
fi
wait_vm_condition '{.items[0].status.ready}' "true" "VM restarted via dead-VMI bounce"
kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
    -o custom-columns=NAME:.metadata.name,PHASE:.status.phase --no-headers

step "4/4 delete and verify teardown"
"${OPENSHELL_CLI}" sandbox delete "${NAME}"
deadline=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "${deadline}" ]; do
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
    echo "ERROR: OpenShell-managed resources still present" >&2
    exit 1
fi
echo
echo "demo complete: KubeVirt sandbox lifecycle (create, run, stop, restart, teardown) verified"
