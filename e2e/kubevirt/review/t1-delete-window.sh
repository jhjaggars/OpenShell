#!/usr/bin/env bash
# Finding: delete removes the per-sandbox fence immediately after an
# unwaited VM delete, while the VMI is still in graceful shutdown. In the
# gateway topology no other policy selects the launcher pod, so its network
# namespace (the guest's egress path) is unfenced for the rest of the grace
# period. Probes raw egress from the launcher pod every second after delete.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-t1}"
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; L="$(launcher "$ID")"
log "ready: id=$ID launcher=$L; baseline probe 1.1.1.1:443 -> $(launcher_probe "$L" 1.1.1.1:443)"
log "policies selecting the launcher before delete:"; kubectl get netpol -n "$NS" --no-headers
T0=$(date +%s); "$OS" sandbox delete "$NAME" >/dev/null; log "delete accepted"
while :; do
  E=$(( $(date +%s) - T0 ))
  F=$(kubectl get netpol -n "$NS" "openshell-kubevirt-fence-$ID" --no-headers 2>/dev/null | wc -l || true)
  P=$(kubectl get pod -n "$NS" "$L" -o jsonpath='{.status.phase}' 2>/dev/null || true)
  [ -z "$P" ] && { log "t+${E}s launcher pod gone"; break; }
  R="n/a"; [ "$F" = 0 ] && R="$(launcher_probe "$L" 1.1.1.1:443)"
  log "t+${E}s fence=$F launcher=$P egress(1.1.1.1:443)=$R"
  [ "$E" -gt 300 ] && break; sleep 1
done
log "leftovers: $(leftovers "$ID")"
