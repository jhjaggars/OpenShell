#!/usr/bin/env bash
# Concurrent creates, then concurrent deletes; then create+delete in quick
# succession and delete while Bootstrapping/Provisioning. Checks leftovers.
source "$(dirname "$0")/lib.sh"
N="${1:-4}"
names=(); for i in $(seq 1 "$N"); do names+=("rv-c$i"); done
log "creating ${names[*]} concurrently"
for n in "${names[@]}"; do create "$n" & done; wait
ok=0; for n in "${names[@]}"; do wait_phase "$n" Ready 420 && ok=$((ok+1)); done
log "$ok/$N reached Ready"
ids=(); for n in "${names[@]}"; do ids+=("$(sbx_id "$n")"); done
for n in "${names[@]}"; do "$OS" sandbox exec "$n" -- echo "exec-$n" 2>&1 | grep -a "exec-$n" || log "exec failed in $n"; done
log "deleting concurrently"
for n in "${names[@]}"; do "$OS" sandbox delete "$n" >/dev/null 2>&1 & done; wait
sleep 60; for id in "${ids[@]}"; do log "leftovers $id: $(leftovers "$id")"; done
log "--- create then immediate delete (delete while Bootstrapping)"
create rv-quick & CP=$!; sleep 2; "$OS" sandbox delete rv-quick 2>&1 | tail -1; wait $CP || log "create exited rc=$?"
sleep 5; log "phase now: '$(phase rv-quick)'"
sleep 60; kubectl get vm,vmi,pod,secret,pvc,netpol -n "$NS" --no-headers 2>/dev/null | grep -a 'rv-quick' || log "rv-quick: no named leftovers"
log "fences without a VM:"; for f in $(kubectl get netpol -n "$NS" -o name | grep kubevirt-fence); do id="${f##*fence-}"; kubectl get vm -n "$NS" -l "openshell.ai/sandbox-id=$id" --no-headers 2>/dev/null | grep -q . || echo "  orphan $f"; done
