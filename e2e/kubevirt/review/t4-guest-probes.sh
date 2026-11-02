#!/usr/bin/env bash
# Escape probes from inside the guest, as the zero-capability workload user,
# through `sandbox exec`. Each probe prints its raw outcome; nothing here
# prints secret material (only sizes, owners, and first-byte checks).
source "$(dirname "$0")/lib.sh"
NAME="${1:-rv-t4}"
if [ "$(phase "$NAME")" != "Ready" ]; then create "$NAME"; wait_phase "$NAME" Ready 300; fi
x() { echo "--- $1"; "$OS" sandbox exec "$NAME" -- sh -c "$2" 2>&1 | grep -v 'could not be delivered intact' | head -${3:-8} || true; }
x "identity" 'id; cat /proc/self/status | grep -E "^(Cap(Inh|Prm|Eff|Bnd|Amb)|NoNewPrivs|Seccomp):"'
x "config ISO block device (holds the boundary private key)" 'ls -l /dev/vd* 2>&1; head -c 16 /dev/vdb >/dev/null 2>&1 && echo "READ /dev/vdb: OK (BAD)" || echo "read /dev/vdb: denied"'
x "staged bootstrap dir (should be empty: boundary deletes after load)" 'ls -la /.openshell/state/bootstrap 2>&1'
x "boundary process: owner of /proc/<pid>/mem (root => non-dumpable)" 'for p in $(pgrep -f openshell-sandbox); do echo "pid $p $(cat /proc/$p/comm) mem-owner=$(stat -c %U /proc/$p/mem) environ-readable=$(head -c1 /proc/$p/environ >/dev/null 2>&1 && echo yes || echo no)"; done'
x "PID 1 environ (root init)" 'head -c1 /proc/1/environ >/dev/null 2>&1 && echo "readable (BAD)" || echo denied'
x "kernel cmdline" 'cat /proc/cmdline'
x "raw/ICMP socket" 'ping -c1 -W2 1.1.1.1 2>&1 | tail -2'
x "IPv6 addresses / route" 'cat /proc/net/if_inet6; cat /proc/net/ipv6_route | grep -v "^0\{32\} 00 0\{32\} 00 0\{32\} ffffffff" | head -3'
x "IPv6 connect to a public v6 address" 'nc -z -w5 2606:4700:4700::1111 443 && echo "CONNECTED (BAD)" || echo blocked'
x "Kubernetes API 10.43.0.1:443" 'nc -z -w5 10.43.0.1 443 && echo "CONNECTED (BAD)" || echo blocked'
x "cluster DNS 10.43.0.10:53 (tcp)" 'nc -z -w5 10.43.0.10 53 && echo "CONNECTED (BAD)" || echo blocked'
x "link-local metadata 169.254.169.254:80" 'nc -z -w5 169.254.169.254 80 && echo "CONNECTED (BAD)" || echo blocked'
x "launcher-side gateway 10.0.2.2 / host bridge 10.89.0.1:18185" 'ip route 2>/dev/null | head -2; nc -z -w5 10.89.0.1 18185 && echo "gateway CONNECTED (BAD)" || echo "gateway blocked"'
x "DNS relay: resolve a name NOT in policy (exfil channel?)" 'cat /etc/resolv.conf | head -1; nslookup rv-exfil-$(date +%s).example.net 2>&1 | tail -3'
x "DNS relay: resolve kubernetes.default.svc.cluster.local" 'nslookup kubernetes.default.svc.cluster.local 2>&1 | tail -3'
x "udp to 1.1.1.1:53 direct" 'echo x | nc -u -w2 1.1.1.1 53 && echo "sent (check)" || echo blocked'
x "write the rootfs / boundary binary" 'touch /usr/local/bin/x 2>&1; touch /opt/openshell/x 2>&1; touch /etc/x 2>&1; echo done'
x "bind a listener on 0.0.0.0:8080 (ingress via masquerade?)" '(timeout 3 nc -l -p 8080 >/dev/null 2>&1; echo "listen rc=$?") '
