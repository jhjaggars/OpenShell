#!/usr/bin/env bash
# guest-init.sh runs `chown -R 10001:10001 /sandbox` as root on every boot,
# over a workspace the workload controls. Plant symlinks to root-owned files
# and check whether the next boot's chown follows them.
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-ws}"
create "$NAME"; wait_phase "$NAME" Ready 300
x() { timeout 60 "$OS" sandbox exec "$NAME" -- sh -c "$1" 2>&1 | grep -v 'delivered intact' || true; }
log "before: $(x 'ls -ln /etc/shadow /usr/local/bin/openshell-sandbox /opt/openshell/init | awk "{print \$3\":\"\$4, \$9}" | tr "\n" " "')"
x 'ln -s /etc/shadow /sandbox/l-shadow; ln -s /usr/local/bin/openshell-sandbox /sandbox/l-bin; ln -s /opt/openshell/init /sandbox/l-init; mkdir -p /sandbox/d; ln -s /etc /sandbox/d/l-etc; ls -l /sandbox | grep -c " -> "'
"$OS" sandbox stop "$NAME" >/dev/null 2>&1; "$OS" sandbox start "$NAME" >/dev/null 2>&1 || true; wait_phase "$NAME" Ready 300 || true
log "after reboot: $(x 'ls -ln /etc/shadow /usr/local/bin/openshell-sandbox /opt/openshell/init /etc/passwd | awk "{print \$3\":\"\$4, \$9}" | tr "\n" " "')"
log "symlinks themselves: $(x 'ls -ln /sandbox | grep " -> " | awk "{print \$3\":\"\$4, \$9}" | tr "\n" " "')"
log "can workload read /etc/shadow now? $(x 'head -c1 /etc/shadow >/dev/null 2>&1 && echo YES || echo no')"
"$OS" sandbox delete "$NAME" >/dev/null
