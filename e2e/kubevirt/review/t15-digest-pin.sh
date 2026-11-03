#!/usr/bin/env bash
# Digest pinning (option A, pin after first boot), registry-backed guest
# image. After the first boot the VM spec must carry repo@sha256:<A>; after
# the tag moves to image B, a restart must still boot and claim A; a new
# create must get B. Needs REGISTRY_PUSH (host push ref of the tag) and
# IMAGE_B (a local image to push over the tag).
source "$(dirname "$0")/lib.sh"
: "${REGISTRY_PUSH:?}" "${IMAGE_B:?}"
spec() { kubectl get vm -n "$NS" "default--$1" -o jsonpath='kernelBoot={.spec.template.spec.domain.firmware.kernelBoot.container.image} rootdisk={.spec.template.spec.volumes[?(@.name=="rootdisk")].containerDisk.image}'; }
booted() { kubectl get pod -n "$NS" -l "kubevirt.io=virt-launcher,openshell.ai/sandbox-name=$1" -o jsonpath='{range .items[0].status.initContainerStatuses[?(@.name=="volumerootdisk")]}{.imageID}{end}'; }
claimed() { kubectl get secret -n "$NS" "default--$1-bootstrap" -o jsonpath='{.data.userData}' | base64 -d \
    | grep '^OPENSHELL_BOUNDARY_CONFIG_B64=' | cut -d= -f2- | base64 -d \
    | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["resource_claims"].get("kubevirt.image_identity"))'; }
create rv-pin; wait_phase rv-pin Ready 300
log "first boot: booted=$(booted rv-pin) claimed=$(claimed rv-pin)"
for i in $(seq 1 30); do spec rv-pin | grep -q '@sha256' && break; sleep 3; done
log "after reconcile: $(spec rv-pin)"
log "conditions: $("$OS" sandbox get rv-pin 2>&1 | grep -a -iE 'ImagePin' | tr -s ' \n' ' ')"
podman push --tls-verify=false "$IMAGE_B" "$REGISTRY_PUSH" >/dev/null 2>&1 && log "tag moved: $REGISTRY_PUSH -> $IMAGE_B"
"$OS" sandbox stop rv-pin >/dev/null 2>&1; "$OS" sandbox start rv-pin 2>&1 | tail -1; wait_phase rv-pin Ready 300 || true
log "restart after retag: phase=$(phase rv-pin) booted=$(booted rv-pin) claimed=$(claimed rv-pin) $(spec rv-pin)"
create rv-pin2; wait_phase rv-pin2 Ready 300 || true
log "new create after retag: booted=$(booted rv-pin2)"
for i in $(seq 1 30); do spec rv-pin2 | grep -q '@sha256' && break; sleep 3; done
log "new create pinned: $(spec rv-pin2)"
"$OS" sandbox delete rv-pin >/dev/null 2>&1; "$OS" sandbox delete rv-pin2 >/dev/null 2>&1 || true
