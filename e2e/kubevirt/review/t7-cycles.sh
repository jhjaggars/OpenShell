#!/usr/bin/env bash
# Repeated stop/start cycles; after each, count the sandbox's labelled
# objects by kind and confirm exec still works. Then delete and check zero.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-cyc}"; CYCLES="${2:-6}"
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"
count() { for k in vm vmi pod svc secret pvc netpol; do printf '%s=%s ' "$k" "$(kubectl get "$k" -n "$NS" -l "openshell.ai/sandbox-id=$ID" --no-headers 2>/dev/null | wc -l || true)"; done; printf 'fence=%s' "$(kubectl get netpol -n "$NS" "openshell-kubevirt-fence-$ID" --no-headers 2>/dev/null | wc -l || true)"; }
log "cycle 0: $(count)"
for i in $(seq 1 "$CYCLES"); do
  S=$(date +%s)
  "$OS" sandbox stop "$NAME" >/dev/null 2>&1 || log "stop $i failed"
  "$OS" sandbox start "$NAME" > /tmp/rv-start.$$ 2>&1 || { log "start $i failed: $(tail -2 /tmp/rv-start.$$ | tr -s ' \n' ' ')"; }
  wait_phase "$NAME" Ready 300 || true
  E=$( "$OS" sandbox exec "$NAME" -- echo ok 2>&1 | grep -ac '^ok' || true)
  log "cycle $i ($(( $(date +%s)-S ))s) phase=$(phase "$NAME") exec=$E $(count)"
done
rm -f /tmp/rv-start.$$
"$OS" sandbox delete "$NAME" >/dev/null
S=$(date +%s); until [ "$(leftovers "$ID")" = 0 ] || [ $(( $(date +%s)-S )) -gt 240 ]; do sleep 3; done
log "after delete: $(count)"
