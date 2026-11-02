#!/usr/bin/env bash
# Round-1 regression probe (M6): every start must persist a runtime
# identity the next start can parse. Create, then stop/start three times;
# each start must reach Ready with exec working. Also prints the image the
# boundary claims next to the image the VM spec boots.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-id}"
create "$NAME"; wait_phase "$NAME" Ready 300
claimed_image() {
  kubectl get secret -n "$NS" "default--$NAME-bootstrap" -o jsonpath='{.data.userData}' | base64 -d \
    | grep '^OPENSHELL_BOUNDARY_CONFIG_B64=' | cut -d= -f2- | base64 -d \
    | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["resource_claims"].get("kubevirt.image_identity"))'
}
boot_image() { kubectl get vm -n "$NS" "default--$NAME" -o jsonpath='{.spec.template.spec.domain.firmware.kernelBoot.container.image}'; }
log "created: claimed=$(claimed_image) boots=$(boot_image)"
for i in 1 2 3; do
  "$OS" sandbox stop "$NAME" >/dev/null 2>&1 || log "stop $i failed"
  out="$("$OS" sandbox start "$NAME" 2>&1 || true)"
  grep -q "Started" <<<"$out" || log "start $i FAILED: $(tr -s ' \n' ' ' <<<"$out" | cut -c1-200)"
  wait_phase "$NAME" Ready 300 || true
  E=$(timeout 40 "$OS" sandbox exec "$NAME" -- echo ok 2>&1 | grep -ac '^ok' || true)
  log "start $i: phase=$(phase "$NAME") exec=$E claimed=$(claimed_image)"
done
"$OS" sandbox delete "$NAME" >/dev/null 2>&1 || true
