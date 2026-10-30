#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Start a standalone OpenShell gateway on the host whose Kubernetes compute
# driver runs in `runtime = "kubevirt"` mode against the local `kvpoc` k3d
# cluster (see e2e/kubevirt/README.md for the full PoC walkthrough).
#
# Mirrors tasks/scripts/gateway-vm.sh (the libkrun VM driver dev gateway) but
# provisions KubeVirt VirtualMachines instead. The gateway runs on the host
# with the k3d kubeconfig; sandbox guests reach it over the k3d docker bridge
# (http://<bridge-gateway>:<port>).
#
# Prerequisites (see README.md):
#   - kvpoc k3d cluster with KubeVirt deployed and the guest image imported
#   - e2e/kubevirt/build-guest-image.sh has been run
#
# Usage: e2e/kubevirt/demo.sh [--demo]
#   --demo  after the gateway starts, run the create/exec/teardown demo
#           (requires the gateway to stay in the foreground; the demo runs
#           against it from a background shell)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT="${OPENSHELL_SERVER_PORT:-18085}"
GATEWAY_NAME="kvpoc"
STATE_DIR="${ROOT}/.cache/gateway-kubevirt"
LOG_LEVEL="${OPENSHELL_LOG_LEVEL:-info}"
CARGO_TARGET="${CARGO_TARGET_DIR:-${ROOT}/target}"
GATEWAY_BIN="${CARGO_TARGET}/release/openshell-gateway"
OPENSHELL_CLI="${CARGO_TARGET}/release/openshell"
SANDBOX_NAMESPACE="kvpoc-sandboxes"
GUEST_IMAGE="${OPENSHELL_KVPOC_GUEST_IMAGE:-kvpoc/openshell-sandbox-vm:dev}"

RUN_DEMO=0
[ "${1:-}" = "--demo" ] && RUN_DEMO=1

# The k3d docker bridge gateway is the host address guests use to reach the
# gateway (k3d prints it at cluster creation; derive it from the network).
BRIDGE_IP="$(docker network inspect k3d-kvpoc -f '{{(index .IPAM.Config 0).Gateway}}')"
GRPC_ENDPOINT="http://${BRIDGE_IP}:${PORT}"

echo "==> Building openshell-gateway and openshell CLI"
cargo build --release -p openshell-server -p openshell-cli

TLS_DIR="${STATE_DIR}/tls"
if [ ! -f "${TLS_DIR}/jwt/signing.pem" ]; then
    echo "==> Generating local gateway credentials"
    "${GATEWAY_BIN}" generate-certs --output-dir "${TLS_DIR}"
fi

mkdir -p "${STATE_DIR}"
# The demo is single-shot: drop any sandbox records left by a previous run
# so `sandbox create --name kvpoc-demo` is reproducible. Nothing durable is
# kept in this gateway.
rm -f "${STATE_DIR}"/gateway.db*
CONFIG_PATH="${STATE_DIR}/gateway.toml"
cat >"${CONFIG_PATH}" <<EOF
# OpenShell gateway for the KubeVirt compute-driver PoC (e2e/kubevirt/).
[openshell]
version = 1

[openshell.gateway]
compute_drivers = ["kubernetes"]
disable_tls = true
# 0.0.0.0: guests reach the gateway through the k3d docker bridge
# (http://172.18.0.1:PORT); the CLI uses 127.0.0.1.
bind_address = "0.0.0.0:${PORT}"
health_bind_address = "127.0.0.1:$((PORT + 1))"

[openshell.gateway.auth]
allow_unauthenticated_users = true

[openshell.gateway.gateway_jwt]
signing_key_path = "${TLS_DIR}/jwt/signing.pem"
public_key_path = "${TLS_DIR}/jwt/public.pem"
kid_path = "${TLS_DIR}/jwt/kid"
gateway_id = "kvpoc"
ttl_secs = 3600

[openshell.drivers.kubernetes]
runtime = "kubevirt"
gateway_id = "kvpoc"
namespace = "${SANDBOX_NAMESPACE}"
grpc_endpoint = "${GRPC_ENDPOINT}"

[openshell.drivers.kubernetes.kubevirt]
guest_image = "${GUEST_IMAGE}"
guest_image_pull_policy = "IfNotPresent"
cpu_cores = 1
memory = "1Gi"
EOF

# Register the gateway for the CLI (plaintext, like gateway-vm.sh).
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
GATEWAY_DIR="${CONFIG_HOME}/openshell/gateways/${GATEWAY_NAME}"
mkdir -p "${GATEWAY_DIR}"
cat >"${GATEWAY_DIR}/metadata.json" <<EOF
{
  "name": "${GATEWAY_NAME}",
  "gateway_endpoint": "http://127.0.0.1:${PORT}",
  "is_remote": false,
  "gateway_port": ${PORT},
  "auth_mode": "plaintext"
}
EOF
printf '%s' "${GATEWAY_NAME}" >"${CONFIG_HOME}/openshell/active_gateway"

# Sandboxes render into a dedicated namespace.
kubectl create namespace "${SANDBOX_NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

cleanup() {
    if [ "${RUN_DEMO}" = "1" ] && [ -n "${GATEWAY_PID:-}" ]; then
        kill "${GATEWAY_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo
echo "Starting KubeVirt-mode Kubernetes gateway..."
echo "  gateway:    ${GATEWAY_NAME} (http://127.0.0.1:${PORT})"
echo "  guest view: ${GRPC_ENDPOINT}"
echo "  namespace:  ${SANDBOX_NAMESPACE}"
echo "  guest disk: ${GUEST_IMAGE}"
echo

"${GATEWAY_BIN}" \
    --config "${CONFIG_PATH}" \
    --port "${PORT}" \
    --log-level "${LOG_LEVEL}" \
    --drivers kubernetes \
    --db-url "sqlite:${STATE_DIR}/gateway.db?mode=rwc" \
    --disable-tls &
GATEWAY_PID=$!

# Wait for the gateway to accept requests.
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$((PORT + 1))/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if [ "${RUN_DEMO}" = "1" ]; then
    "${ROOT}/e2e/kubevirt/run-demo.sh" || {
        kill "${GATEWAY_PID}" 2>/dev/null || true
        exit 1
    }
    kill "${GATEWAY_PID}" 2>/dev/null || true
    trap - EXIT
    wait "${GATEWAY_PID}" 2>/dev/null || true
else
    wait "${GATEWAY_PID}"
fi
