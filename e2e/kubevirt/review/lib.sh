#!/usr/bin/env bash
# Shared helpers for the KubeVirt PoC review probes. Expects KUBECONFIG and
# an `os` CLI wrapper (OPENSHELL_CLI) pointed at a running kvpoc gateway.
set -euo pipefail
NS="${OPENSHELL_KVPOC_NAMESPACE:-kvpoc-sandboxes}"
OS="${OPENSHELL_CLI:?set OPENSHELL_CLI to the openshell CLI}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
POLICY="${ROOT}/e2e/kubevirt/demo-policy.yaml"
ts() { date -u +%H:%M:%S; }
log() { echo "[$(ts)] $*"; }
phase() { "$OS" sandbox get "$1" 2>/dev/null | grep -a "Phase:" | head -1 | sed 's/^ *Phase: //' || true; }
wait_phase() { # name phase timeout
  local d=$(( $(date +%s) + ${3:-300} )) p
  while [ "$(date +%s)" -lt "$d" ]; do p="$(phase "$1")"; [ "$p" = "$2" ] && return 0; sleep 3; done
  log "timeout waiting for $1 -> $2 (last: ${p:-none})"; return 1
}
sbx_id() { kubectl get vm -n "$NS" -l "openshell.ai/sandbox-name=$1" -o jsonpath='{.items[0].metadata.labels.openshell\.ai/sandbox-id}'; }
launcher() { kubectl get pod -n "$NS" -l "kubevirt.io=virt-launcher,openshell.ai/sandbox-id=$1" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
create() { "$OS" sandbox create --detach --name "$1" --policy "$POLICY" -- /bin/sleep infinity >/dev/null; }
leftovers() { kubectl get vm,vmi,pod,svc,secret,pvc,netpol -n "$NS" -l "openshell.ai/sandbox-id=$1" --no-headers 2>/dev/null | wc -l || true; }
# Raw TCP probe from the launcher pod's network namespace (outside the guest
# boundary): prints CONNECTED / REFUSED / TIMEOUT.
launcher_probe() { # launcher-pod host:port
  local out
  out="$(kubectl exec -n "$NS" "$1" -c compute -- timeout 5 openssl s_client -connect "$2" </dev/null 2>&1 || true)"
  if grep -q "CONNECTED" <<<"$out"; then echo CONNECTED
  elif grep -qi "refused" <<<"$out"; then echo REFUSED
  else echo "TIMEOUT/OTHER"; fi
}
