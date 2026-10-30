#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Exercise the KubeVirt-mode Kubernetes compute driver end to end against a
# running kvpoc gateway (started by e2e/kubevirt/demo.sh):
#   1. create a sandbox (provisions a KubeVirt VirtualMachine)
#   2. wait for the in-guest supervisor to connect to the gateway (Ready)
#   3. run a command inside the VM through the supervisor relay
#   4. stop / start the sandbox (runStrategy Halted / Once)
#   5. delete it and verify the VM, bootstrap Secret and workspace PVC are gone
#
# Requires the `openshell` CLI built at $CARGO_TARGET_DIR/release/openshell.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TARGET="${CARGO_TARGET_DIR:-${ROOT}/target}"
OPENSHELL_CLI="${CARGO_TARGET}/release/openshell"
SANDBOX_NAMESPACE="${OPENSHELL_KVPOC_NAMESPACE:-kvpoc-sandboxes}"
NAME="kvpoc-demo"
# Sandboxes render into the shared namespace with `{workspace}--{name}`
# resource names; select ours by the sandbox-name label instead.
VM_SELECTOR="openshell.ai/sandbox-name=${NAME}"
TIMEOUT_SECS="${OPENSHELL_KVPOC_TIMEOUT:-600}"

step() { echo; echo "=== $* ==="; }

wait_ready() {
    local deadline=$(( $(date +%s) + TIMEOUT_SECS ))
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        # Note: no early `exit` in awk — closing the pipe mid-print makes the
        # CLI panic on SIGPIPE (broken pipe) and fail the pipeline.
        phase="$("${OPENSHELL_CLI}" sandbox get "${NAME}" 2>/dev/null \
            | awk '/Phase/ {print $2}')"
        case "${phase}" in
            Ready) return 0 ;;
            Error) echo "sandbox entered Error phase" >&2; return 1 ;;
        esac
        sleep 5
    done
    echo "timed out after ${TIMEOUT_SECS}s waiting for Ready (phase: ${phase:-unknown})" >&2
    return 1
}

step "1/5 sandbox create (provisions a KubeVirt VirtualMachine)"
"${OPENSHELL_CLI}" sandbox create --name "${NAME}" -- /bin/sleep infinity
kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}"

step "2/5 waiting for supervisor connection (gateway-composed Ready)"
wait_ready
"${OPENSHELL_CLI}" sandbox get "${NAME}"
kubectl get vmi -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}"

step "3/5 exec inside the VM (via the supervisor relay)"
"${OPENSHELL_CLI}" sandbox exec --name "${NAME}" -- sh -c \
    'echo "hello from $(uname -srm) (PID 1: $(cat /proc/1/comm))"'

step "4/5 stop / start (runStrategy Halted -> Once)"
"${OPENSHELL_CLI}" sandbox stop "${NAME}"
kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" \
    -o jsonpath='{.spec.runStrategy}{"\n"}'
"${OPENSHELL_CLI}" sandbox start "${NAME}"
wait_ready

step "5/5 delete and verify teardown"
"${OPENSHELL_CLI}" sandbox delete "${NAME}"
deadline=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "${deadline}" ]; do
    if ! kubectl get vm -l "${VM_SELECTOR}" -n "${SANDBOX_NAMESPACE}" >/dev/null 2>&1; then
        break
    fi
    sleep 5
done
kubectl get vm,vmi,secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/managed-by=openshell 2>/dev/null || true
remaining="$(kubectl get vm,vmi,secret,pvc -n "${SANDBOX_NAMESPACE}" \
    -l openshell.ai/managed-by=openshell --no-headers 2>/dev/null | wc -l)"
if [ "${remaining}" != "0" ]; then
    echo "ERROR: OpenShell-managed resources still present" >&2
    exit 1
fi
echo
echo "demo complete: sandbox ran in a KubeVirt VM and tore down cleanly"
