#!/usr/bin/env bash
# Round-1 checks for M1 (running-state fence reconciler) and the H1
# namespace default-deny floor: delete the per-sandbox fence under a Ready
# sandbox, probe immediately (floor), wait for the reconciler, check status
# and exec; then delete the floor itself and watch whether it comes back.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-m1}"
FLOOR=openshell-kubevirt-launcher-isolation
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; L="$(launcher "$ID")"; FN="openshell-kubevirt-fence-$ID"
fuid() { kubectl get netpol -n "$NS" "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null || echo none; }
ex() { timeout 40 "$OS" sandbox exec "$NAME" -- echo exec-ok 2>&1 | grep -ac exec-ok || true; }
log "Ready; fence uid=$(fuid "$FN") floor uid=$(fuid "$FLOOR") egress=$(launcher_probe "$L" 1.1.1.1:443)"
kubectl delete netpol -n "$NS" "$FN" >/dev/null; T0=$(date +%s); sleep 6
log "fence deleted: egress=$(launcher_probe "$L" 1.1.1.1:443) (floor should hold) phase=$(phase "$NAME")"
until [ "$(fuid "$FN")" != none ] || [ $(( $(date +%s)-T0 )) -gt 120 ]; do sleep 2; done
log "fence back after $(( $(date +%s)-T0 ))s uid=$(fuid "$FN"); egress=$(launcher_probe "$L" 1.1.1.1:443) exec=$(ex)"
log "status conditions: $("$OS" sandbox get "$NAME" 2>&1 | grep -a -iE 'fence|restor' | tr -s ' \n' ' ')"
log "--- deleting the namespace floor policy"
kubectl delete netpol -n "$NS" "$FLOOR" >/dev/null; sleep 70
log "floor after 70s: uid=$(fuid "$FLOOR")"
kubectl delete netpol -n "$NS" "$FN" >/dev/null; sleep 6
log "floor gone + fence deleted: egress=$(launcher_probe "$L" 1.1.1.1:443) k8sapi=$(launcher_probe "$L" 10.43.0.1:443)"
until [ "$(fuid "$FN")" != none ] || [ $(( $(date +%s)-T0 )) -gt 400 ]; do sleep 2; done
log "fence restored again: egress=$(launcher_probe "$L" 1.1.1.1:443); floor uid=$(fuid "$FLOOR")"
"$OS" sandbox stop "$NAME" >/dev/null 2>&1; "$OS" sandbox start "$NAME" 2>&1 | tail -2 || true
log "after stop/start: floor uid=$(fuid "$FLOOR") phase=$(phase "$NAME")"
