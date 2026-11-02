#!/usr/bin/env bash
# Finding: start performs its generation side effects (dedicated: delete and
# recreate the supervisor Pod; both: re-render the bootstrap Secret,
# re-register/release, prune Secrets) BEFORE kubevirt_start_plan decides the
# VM is AlreadyRunning. Calls `sandbox start` on a Ready sandbox and checks
# whether the running guest is still usable. Also times stop vs. VMI exit.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-t2}"
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; LC="$(tr '[:upper:]' '[:lower:]' <<<"$ID")"
sup_uid() { kubectl get pod -n "$NS" "os-supervisor-$LC" -o jsonpath='{.metadata.uid}' 2>/dev/null || echo none; }
sec_rv() { kubectl get secret -n "$NS" "default--$NAME-bootstrap" -o jsonpath='{.metadata.resourceVersion}'; }
log "Ready. supervisor-pod-uid=$(sup_uid) bootstrap-secret-rv=$(sec_rv)"
"$OS" sandbox exec "$NAME" -- echo exec-before-ok 2>&1 | grep -a exec-before || true
log "calling start on a Ready sandbox"
"$OS" sandbox start "$NAME" 2>&1 | tail -3 || true
log "after start: phase=$(phase "$NAME") supervisor-pod-uid=$(sup_uid) bootstrap-secret-rv=$(sec_rv)"
sleep 20
log "t+20s phase=$(phase "$NAME")"
timeout 60 "$OS" sandbox exec "$NAME" -- echo exec-after-ok 2>&1 | tail -2 || log "exec after start failed rc=$?"
log "--- stop timing"
VMI="default--$NAME"; T0=$(date +%s)
"$OS" sandbox stop "$NAME" 2>&1 | tail -1 || true
log "stop returned after $(( $(date +%s)-T0 ))s; vmi phase now: $(kubectl get vmi -n "$NS" "$VMI" -o jsonpath='{.status.phase}' 2>/dev/null || echo gone)"
until ! kubectl get vmi -n "$NS" "$VMI" >/dev/null 2>&1; do sleep 1; [ $(( $(date +%s)-T0 )) -gt 400 ] && break; done
log "VMI gone $(( $(date +%s)-T0 ))s after stop was issued"
