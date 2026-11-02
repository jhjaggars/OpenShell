#!/usr/bin/env bash
# Dedicated topology: delete the supervisor Pod, then the per-sandbox
# Service, out from under a running sandbox; report status, exec, recovery
# via stop/start, and leftovers.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-d9}"
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; LC="$(tr '[:upper:]' '[:lower:]' <<<"$ID")"; SP="os-supervisor-$LC"
ex() { timeout 40 "$OS" sandbox exec "$NAME" -- echo exec-ok 2>&1 | grep -ac exec-ok || true; }
log "Ready; exec=$(ex)"
kubectl delete pod -n "$NS" "$SP" --wait=true >/dev/null; log "supervisor Pod deleted"
for i in 1 2 3 4; do sleep 15; log "t+$((i*15))s phase=$(phase "$NAME") supervisor-pod=$(kubectl get pod -n "$NS" "$SP" --no-headers 2>/dev/null | awk '{print $3}' || true) exec=$(ex)"; done
log "recover with stop/start"
"$OS" sandbox stop "$NAME" 2>&1 | tail -1 || true
"$OS" sandbox start "$NAME" 2>&1 | tail -2 || true
wait_phase "$NAME" Ready 300 || true; log "after restart: phase=$(phase "$NAME") exec=$(ex)"
kubectl delete svc -n "$NS" "$SP" >/dev/null; log "per-sandbox Service deleted"
for i in 1 2 3; do sleep 15; log "t+$((i*15))s phase=$(phase "$NAME") exec=$(ex)"; done
"$OS" sandbox stop "$NAME" 2>&1 | tail -1 || true
"$OS" sandbox start "$NAME" 2>&1 | tail -2 || true
wait_phase "$NAME" Ready 300 || true; log "after restart (Service recreated?): phase=$(phase "$NAME") svc=$(kubectl get svc -n "$NS" "$SP" --no-headers 2>/dev/null | wc -l || true) exec=$(ex)"
"$OS" sandbox delete "$NAME" >/dev/null
S=$(date +%s); until [ "$(leftovers "$ID")" = 0 ] || [ $(( $(date +%s)-S )) -gt 240 ]; do sleep 3; done
log "leftovers after delete: $(leftovers "$ID")"
