#!/bin/sh
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# OpenShell KubeVirt initramfs init (trusted boot stage).
#
# This script is packed into the initramfs (e2e/kubevirt/build-guest-image.sh)
# and runs as PID 1 BEFORE the real root is mounted. The workload can never
# write it: the initramfs is freed after switch_root and re-read from the
# (sealed) image at every boot.
#
# Boot plan (root-workload VM PoC, commit 6):
#
#   /dev/vda  containerDisk ext4 carrying the rootfs in the first
#             num_data_blocks 4096-byte blocks and the dm-verity hash tree
#             appended after it (no superblock). The kernel verifies every
#             read of the mapped device against the tree; corruption is EIO.
#   overlay   tmpfs upper + work over the read-only verity lower: the
#             workload gets a writable / in RAM, the lower stays pristine,
#             and every boot starts from the verified image.
#   trusted   the guest init and the boundary binary are bind-mounted from
#   paths     the verity lower (inode-pinned, read-only): the workload can
#             shadow them in the overlay, but what actually runs is the
#             verified bytes.
#
# The verity geometry and root hash arrive on the kernel command line
# (driver-rendered, trusted): openshell_verity=<blocks>:<hashstart>:<hash>.
# A missing or failing verity setup powers the guest off — a tampered root
# is a dead sandbox, not a silent downgrade.
#
# After the mounts are assembled this init execs the guest init (from the
# trusted bind path), which continues as PID 1 in the overlay root.

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# The stock initramfs exposes only /bin/sh; install every busybox applet
# symlink so insmod/sleep/mount/switch_root resolve as commands.
/bin/busybox --install -s /bin

fatal() {
    echo "[verity-init FATAL] $*" >&2
    echo "[verity-init FATAL] powering off" >&2
    sync
    poweroff -f
    while true; do sleep 3600; done
}

mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true

# The stock initramfs's kmod modprobe resolves the module tree: the
# virtio block device (the verity target), and the device-mapper modules
# (added to the tree by the image build). The verity target is a module
# in the Alpine -virt kernel.
modprobe virtio_blk 2>/dev/null || true
modprobe virtio_net 2>/dev/null || true
modprobe dm_mod 2>/dev/null || true
modprobe dm_verity 2>/dev/null || true
[ -e /dev/vda ] \
    || fatal "/dev/vda missing (virtio_blk did not load?)"
[ -e /dev/mapper/control ] \
    || fatal "device-mapper control node missing (dm_mod did not load?)"

# Parse the verity parameters from the trusted command line.
VERITY_ARGS=""
for arg in $(cat /proc/cmdline); do
    case "${arg}" in
        openshell_verity=*) VERITY_ARGS="${arg#openshell_verity=}" ;;
    esac
done
[ -n "${VERITY_ARGS}" ] || fatal "no openshell_verity= on the kernel command line"

NUM_DATA_BLOCKS="${VERITY_ARGS%%:*}"
REST="${VERITY_ARGS#*:}"
HASH_START_BLOCK="${REST%%:*}"
ROOT_HASH="${REST#*:}"
case "${ROOT_HASH}" in
    *:*) fatal "malformed openshell_verity (expected blocks:hashstart:hash)" ;;
esac

echo "[verity-init] opening dm-verity: blocks=${NUM_DATA_BLOCKS}" \
     "hash_start=${HASH_START_BLOCK}"
DM_DEV=/dev/mapper/openshell-root
# The helper ships at the initramfs root; the root is not in PATH.
/verity-open openshell-root /dev/vda "${NUM_DATA_BLOCKS}" \
    "${HASH_START_BLOCK}" "${ROOT_HASH}" > /dev/null \
    || fatal "dm-verity setup failed (helper error, bad root hash, or" \
             "geometry) — the image does not match the trusted hash;" \
             "powering off"

# The verity mapping: the kernel checks the tree on READ, so the first
# mount fails with EIO when the rootfs blocks are corrupted.
mkdir -p /lower
# noload: no journal replay on the read-only verified image (mkfs-clean);
# corrupted blocks surface as EIO from the verity layer.
mount -t ext4 -o ro,noload /dev/mapper/openshell-root /lower \
    || fatal "mounting the verity root failed (EIO = corrupted rootfs);" \
             "powering off"

# The writable root: a tmpfs-backed overlay over the verified lower.
mkdir -p /ovl/upper /ovl/work /sysroot
mount -t tmpfs -o mode=0755,nosuid tmpfs /ovl
mkdir -p /ovl/upper /ovl/work
mount -t overlay overlay \
    -o "lowerdir=/lower,upperdir=/ovl/upper,workdir=/ovl/work" /sysroot \
    || fatal "overlayfs mount failed"

# Trusted bind paths: pin the verified init and boundary binaries into the
# new root. The workload may shadow /opt/openshell/init and
# /usr/local/bin/openshell-sandbox in the overlay, but /run/openshell/*
# resolve to the pinned read-only inodes from the verity device.
mkdir -p /sysroot/run/openshell
# A bind mount needs an existing target file; the bound inode is what
# resolves afterwards (the workload shadowing these paths in the overlay
# cannot change what the pinned inodes serve).
touch /sysroot/run/openshell/init /sysroot/run/openshell/openshell-sandbox
mount --bind /lower/opt/openshell/init /sysroot/run/openshell/init \
    || fatal "bind-mounting the trusted guest init failed"
mount --bind /lower/usr/local/bin/openshell-sandbox \
    /sysroot/run/openshell/openshell-sandbox \
    || fatal "bind-mounting the trusted boundary binary failed"

# The overlay holds references to the lower and /ovl superblocks; moving
# both mounts into the new root keeps them alive across switch_root (which
# otherwise frees the initramfs).
mkdir -p /sysroot/run/lower /sysroot/run/ovl
mount --move /lower /sysroot/run/lower || fatal "moving the lower mount failed"
mount --move /ovl /sysroot/run/ovl || fatal "moving the overlay tmpfs failed"

# /dev, /proc, /sys move with switch_root's -c or land via the guest init's
# own mounts; busybox switch_root moves them when they are mounted at the
# standard paths.
echo "[verity-init] switching to the verified overlay root"
exec switch_root /sysroot /run/openshell/init
