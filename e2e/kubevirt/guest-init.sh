#!/bin/sh
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# KubeVirt guest init for OpenShell sandboxes (proof of concept).
#
# Runs as PID 1 (kernel arg init=/opt/openshell/init inside the purpose-built
# containerDisk from e2e/kubevirt/build-guest-image.sh). Modeled on the libkrun
# init (crates/openshell-driver-vm/scripts/openshell-vm-sandbox-init.sh) but
# for KubeVirt hardware:
#
#   /dev/vda  rootfs      (containerDisk, read-only in practice: writes are
#                          lost when the VMI goes away)
#   /dev/vdb  config disk (cloudInitNoCloud ISO rendered from the driver's
#                          bootstrap Secret; shell-sourceable user-data)
#   workspace  virtio-fs share backed by the per-sandbox PVC (filesystem
#              PVCs are not exposed as block disks by KubeVirt), mounted at
#              /sandbox and seeded from the image on first boot
#
# After setup it execs the static openshell-sandbox supervisor, which becomes
# PID 1 and owns the gateway relay, exec, policy, and logs from there on.

set -eu

# Every exit of PID 1 must power the guest off: a dead init means either a
# kernel panic (QEMU serves a dead kernel, the VM reports ready forever) or
# a hung guest. The trap makes any `set -e` failure path, and the script's
# normal end, end the VMI so the driver maps the sandbox to Error. The
# normal poweroff path below fires the trap too; poweroff -f is idempotent
# enough at that point (the machine is already going down).
trap 'ts "init exiting (rc=$?); powering off"; sync; poweroff -f' EXIT

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root
export TERM=xterm

BOOT_START=$(date +%s 2>/dev/null || echo 0)
ts() {
    now=$(date +%s 2>/dev/null || echo 0)
    echo "[kvpoc-init $((now - BOOT_START))s] $*"
}

mount_fs() {
    mount -t proc proc /proc 2>/dev/null || true
    mount -t sysfs sysfs /sys 2>/dev/null || true
    # The Alpine initramfs does not mount devtmpfs on the real root, so /dev
    # is the image's static directory (no ptmx/null). The supervisor's exec
    # relay needs PTY allocation, which requires /dev/ptmx from devtmpfs.
    mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
    mkdir -p /dev/pts /dev/shm
    mount -t devpts -o gid=5,mode=620 devpts /dev/pts 2>/dev/null || true
    [ -e /dev/ptmx ] || ln -sf pts/ptmx /dev/ptmx
    mount -t tmpfs tmpfs /run 2>/dev/null || true
    mount -t tmpfs tmpfs /tmp 2>/dev/null || true
    # The Alpine initramfs mounts the rootfs read-only; the containerDisk
    # block device is writable for the lifetime of the VMI (writes are
    # ephemeral — durable state lives on the workspace PVC).
    mount -o remount,rw / 2>/dev/null || true
}

configure_network() {
    ip link set lo up 2>/dev/null || true
    ip link set eth0 up
    # The k3d pod network runs flannel-vxlan (MTU 1450); a guest eth0 at the
    # default 1500 black-holes full-size TCP segments — small unary RPCs work
    # but large stream frames (SSH KEX, gRPC session) stall. Clamp below the
    # launcher-side MTU before DHCP so the advertised MSS fits.
    ip link set dev eth0 mtu 1400 2>/dev/null || true
    # KubeVirt masquerade networking runs dnsmasq in the virt-launcher pod:
    # DHCP hands out the pod-network address plus cluster DNS.
    if udhcpc -i eth0 -q -n -t 30 -T 2 2>/dev/null; then
        ts "network configured via DHCP (mtu $(cat /sys/class/net/eth0/mtu 2>/dev/null || echo '?'))"
    else
        ts "WARNING: DHCP failed; supervisor will not reach the gateway"
    fi
    # udhcpc's default script maintains /etc/resolv.conf when present.
    if [ ! -s /etc/resolv.conf ]; then
        printf 'nameserver 10.43.0.10\n' > /etc/resolv.conf 2>/dev/null || true
    fi
}

read_bootstrap_config() {
    mkdir -p /config
    if ! mount -t iso9660 -o ro /dev/vdb /config 2>/dev/null; then
        mount -o ro /dev/vdb /config 2>/dev/null || {
            ts "FATAL: cannot mount bootstrap config disk"
            return 1
        }
    fi
    # The bootstrap payload is the shell-sourceable file rendered by the
    # Kubernetes driver's kubevirt runtime mode from the bootstrap Secret.
    # The Secret key is KubeVirt's cloudInitNoCloud convention (`userData`);
    # the file name inside the generated NoCloud ISO is the cloud-init
    # datasource convention (`user-data`). Accept either.
    if [ -f /config/user-data ]; then
        # shellcheck disable=SC1091
        # `set -a` exports every assignment while sourcing so the exec'd
        # supervisor inherits them (the libkrun path passes these as a real
        # VM environment; here they arrive as a file).
        set -a
        . /config/user-data
        set +a
    elif [ -f /config/userData ]; then
        # shellcheck disable=SC1091
        set -a
        . /config/userData
        set +a
    else
        ts "FATAL: no bootstrap payload on config disk"
        ls -la /config || true
        return 1
    fi
    ts "bootstrap config loaded (sandbox ${OPENSHELL_SANDBOX:-unknown})"
    umount /config
}

materialize_token() {
    mkdir -p /opt/openshell/auth /run/openshell
    if [ -n "${OPENSHELL_SANDBOX_TOKEN:-}" ]; then
        old_umask=$(umask)
        umask 077
        printf '%s' "$OPENSHELL_SANDBOX_TOKEN" > /opt/openshell/auth/sandbox.jwt
        umask "$old_umask"
        unset OPENSHELL_SANDBOX_TOKEN
        ts "sandbox token materialized"
    else
        ts "WARNING: no pre-minted sandbox token in bootstrap config"
    fi
}

mount_workspace() {
    mkdir -p /sandbox
    # The workspace PVC arrives as a virtio-fs share (filesystem PVCs are not
    # exposed as block disks by KubeVirt). virtiofs is a kernel module in the
    # Alpine -virt kernel.
    modprobe virtiofs 2>/dev/null || true
    if ! mount -t virtiofs workspace /sandbox 2>/dev/null; then
        ts "WARNING: workspace virtio-fs share not available"
        return 0
    fi
    if [ ! -f /sandbox/.openshell-seeded ] && [ -d /sandbox-seed ]; then
        cp -a /sandbox-seed/. /sandbox/ 2>/dev/null || true
        touch /sandbox/.openshell-seeded
    fi
    chown -R 10001:10001 /sandbox 2>/dev/null || true
    ts "workspace PVC mounted (virtio-fs)"
}

main() {
    ts "OpenShell KubeVirt guest init starting"
    mount_fs
    if [ -c /dev/ptmx ]; then
        ts "/dev/ptmx present"
    else
        ts "WARNING: /dev/ptmx missing — exec PTYs will fail"
    fi
    configure_network
    # The capability-free sandbox owns a loopback-only DNS relay on
    # 127.0.0.53:53. Guest init grants the low port before handing control
    # to the zero-capability workload UID (mirrors the libkrun VM driver's
    # init and the container runtimes' sysctl).
    if ! echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start; then
        ts "FATAL: failed to permit the unprivileged DNS relay to bind port 53"
        return 1
    fi
    read_bootstrap_config
    materialize_token
    mount_workspace
    ts "starting openshell-sandbox"
    # Run the sandbox runtime as a child rather than exec'ing it: when it
    # exits (crash, refusal to boot, or a clean shutdown), PID 1 must power
    # the guest off so the VMI completes instead of leaving QEMU serving a
    # dead kernel (which keeps VM.status.ready true and the sandbox
    # provisioning forever).
    # OPENSHELL_SUPERVISOR_ARGS is word-split onto the command line: the
    # boundary binary's hidden subcommands (capability-probe-launch,
    # launch-capability-free) are dispatched before clap parsing.
    # shellcheck disable=SC2086
    /usr/local/bin/openshell-sandbox ${OPENSHELL_SUPERVISOR_ARGS:-} &
    SANDBOX_PID=$!
    # `wait` returns the child's exit status, and this script runs under
    # `set -e`: an unguarded failing wait would exit PID 1 (kernel panic)
    # before the poweroff below.
    SANDBOX_RC=0
    wait "${SANDBOX_PID}" || SANDBOX_RC=$?
    ts "openshell-sandbox exited with code ${SANDBOX_RC}; powering off"
    sync
    # -f skips the shutdown script hierarchy (there is none here): the
    # kernel powers the machine off immediately, QEMU exits, and the VMI
    # reaches Succeeded so the driver maps the sandbox to Error.
    poweroff -f
    # poweroff -f does not return; sleep forever as a fallback so PID 1
    # never exits into a kernel panic if the syscall fails.
    while true; do sleep 3600; done
}

main
