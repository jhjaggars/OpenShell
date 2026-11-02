#!/usr/bin/env bash
# (a) run-demo.sh step 6's "selective" check: replay its exact logic against
#     the shared supervisor IP from a dedicated-topology launcher, and show
#     the raw probe result next to the verdict it would print.
# (b) An additional NetworkPolicy selecting the launcher (additive allow)
#     reopens egress; start-time fence verification still passes.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-np}"
create "$NAME"; wait_phase "$NAME" Ready 300
ID="$(sbx_id "$NAME")"; L="$(launcher "$ID")"
SIP="$(kubectl get svc -n openshell kvpoc-openshell-supervisor -o jsonpath='{.spec.clusterIP}')"
raw="$(launcher_probe "$L" "$SIP:5500")"
if kubectl exec -n "$NS" "$L" -c compute -- timeout 8 openssl s_client -connect "$SIP:5500" >/tmp/rv-fc.$$ 2>&1 && grep -q "Connection refused" /tmp/rv-fc.$$; then v="FAIL (fence too narrow)"; else v="reachable (selective)"; fi
log "(a) launcher -> shared supervisor $SIP:5500 raw=$raw ; run-demo.sh step-6 logic prints: $v"
rm -f /tmp/rv-fc.$$
log "(b) baseline egress 1.1.1.1:443 -> $(launcher_probe "$L" 1.1.1.1:443)"
kubectl apply -n "$NS" -f - >/dev/null <<YAML
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: rv-additive-allow}
spec:
  podSelector: {matchLabels: {kubevirt.io: virt-launcher}}
  policyTypes: [Egress]
  egress: [{}]
YAML
sleep 8; log "(b) with additive allow-all policy: egress 1.1.1.1:443 -> $(launcher_probe "$L" 1.1.1.1:443)"
"$OS" sandbox stop "$NAME" >/dev/null 2>&1; "$OS" sandbox start "$NAME" 2>&1 | tail -1 || true
wait_phase "$NAME" Ready 300 && log "(b) start passed fence verification with the additive policy in place"
kubectl delete netpol -n "$NS" rv-additive-allow >/dev/null
"$OS" sandbox delete "$NAME" >/dev/null
