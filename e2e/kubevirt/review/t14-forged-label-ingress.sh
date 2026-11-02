#!/usr/bin/env bash
# L2: does a pod with forged pair/workload labels reach the launcher (guest)
# or the supervisor on the boundary port? A REJECT from policy and an RST
# from the guest both read as "refused", so guest reachability is measured
# by the guest kernel's own TCP OutRsts counter (no listener on 5500 in the
# guest: every SYN that arrives is answered with an RST).
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-l2}"; PORT=5500; N=20
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; LC="$(tr '[:upper:]' '[:lower:]' <<<"$ID")"
LIP="$(kubectl get vmi -n "$NS" "default--$NAME" -o jsonpath='{.status.interfaces[0].ipAddress}')"
SIP="$(kubectl get pod -n "$NS" "os-supervisor-$LC" -o jsonpath='{.status.podIP}' 2>/dev/null || true)"
[ -n "$SIP" ] || SIP="$(kubectl get pod -n openshell -l app.kubernetes.io/component=supervisor -o jsonpath='{.items[0].status.podIP}')"
mk() { # name labels...
  kubectl run "$1" -n "$NS" --image=docker.io/library/busybox:1.36 --restart=Never ${2:+--labels="$2"} --command -- sleep 900 >/dev/null
  kubectl wait -n "$NS" "pod/$1" --for=condition=Ready --timeout=120s >/dev/null; }
mk rv-forged "openshell.ai/boundary-pair=$LC,openshell.ai/boundary-role=workload"
mk rv-plain ""
rsts() { timeout 40 "$OS" sandbox exec "$NAME" -- sh -c "awk '/^Tcp:/{n++; if(n==1)for(i=1;i<=NF;i++)h[i]=\$i; else for(i=1;i<=NF;i++) if(h[i]==\"OutRsts\") print \$i}' /proc/net/snmp" 2>/dev/null | grep -E '^[0-9]+$' | tail -1; }
hit() { # pod ip port -> count of connects that completed (CONNECTED) or were refused
  kubectl exec -n "$NS" "$1" -- sh -c "c=0; r=0; for i in \$(seq 1 $N); do if nc -w2 $2 $3 </dev/null >/dev/null 2>&1; then c=\$((c+1)); else r=\$((r+1)); fi; done; echo connected=\$c failed=\$r"; }
log "launcher(guest) $LIP, supervisor $SIP, port $PORT"
for p in rv-plain rv-forged; do
  b="$(rsts)"; out="$(hit "$p" "$LIP" "$PORT")"; a="$(rsts)"
  log "$p -> launcher:$PORT  $out  guest OutRsts delta=$(( ${a:-0} - ${b:-0} )) (≈$N means the SYNs reached the guest)"
  log "$p -> supervisor:$PORT  $(kubectl exec -n "$NS" "$p" -- sh -c "nc -w3 $SIP $PORT </dev/null >/dev/null 2>&1 && echo CONNECTED || echo blocked")"
done
kubectl delete pod -n "$NS" rv-forged rv-plain --now >/dev/null 2>&1 || true
"$OS" sandbox delete "$NAME" >/dev/null 2>&1 || true
