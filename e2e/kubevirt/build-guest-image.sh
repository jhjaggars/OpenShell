#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build the KubeVirt guest containerDisk for the OpenShell kubevirt runtime
# mode PoC (see e2e/kubevirt/README.md).
#
# Produces a local image `kvpoc/openshell-sandbox-vm:dev` containing:
#   /disk/disk.qcow2       bootable ext4 rootfs (Alpine + openshell-sandbox
#                          supervisor + the guest init from guest-init.sh)
#   /boot/vmlinuz-virt     guest kernel (direct kernel boot)
#   /boot/initramfs-virt   guest initramfs
#
# Usage: e2e/kubevirt/build-guest-image.sh [image-tag]
#        (then: k3d image import <image-tag> -c kvpoc)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE_TAG="${1:-kvpoc/openshell-sandbox-vm:dev}"
WORKDIR="$(mktemp -d /tmp/kvpoc-guest.XXXXXX)"
ROOTFS="${WORKDIR}/rootfs"
CARGO_TARGET="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
# The guest is Alpine (musl): the supervisor must be the fully static musl
# build (same requirement as the published supervisor image, see
# .github/workflows/rust-native-build.yml).
SUPERVISOR="${CARGO_TARGET}/x86_64-unknown-linux-musl/release/openshell-sandbox"

KUBEVIRT_VERSION="${KUBEVIRT_VERSION:-v1.9.1}"

command -v mkfs.ext4 >/dev/null || { echo "mkfs.ext4 (e2fsprogs) is required" >&2; exit 1; }
command -v qemu-img >/dev/null || { echo "qemu-img is required" >&2; exit 1; }
# Always invoke cargo build: it is incremental, and a bare [ -x ] check
# silently ships a stale binary whenever a shared CARGO_TARGET_DIR already
# holds one from another checkout (which is exactly how the old-branch
# supervisor ended up in an image built from current main).
(cd "${REPO_ROOT}" && cargo build --release -p openshell-sandbox --target x86_64-unknown-linux-musl)
echo "==> guest supervisor: $("${SUPERVISOR}" --version)"

echo "==> workdir ${WORKDIR}"

# 1. Assemble the Alpine rootfs in a container. All shell setup runs in the
#    initial `docker run`; the container exits when done and `docker cp`
#    stages the host-built binaries into the stopped container's filesystem.
docker rm -f kvpoc-guest-rootfs >/dev/null 2>&1 || true
docker run --name kvpoc-guest-rootfs alpine:3.22 sh -c '
    set -eu
    apk add --no-cache linux-virt e2fsprogs iproute2 nftables iptables bash >/dev/null
    mkdir -p /opt/openshell /sandbox-seed /sandbox /usr/local/bin
    chown 10001:10001 /sandbox /sandbox-seed
    grep -q "^sandbox:" /etc/passwd ||
        printf "sandbox:x:10001:10001:OpenShell Sandbox:/sandbox:/bin/sh\n" >> /etc/passwd
    grep -q "^sandbox:" /etc/group ||
        printf "sandbox:x:10001:\n" >> /etc/group
'
docker cp "${REPO_ROOT}/e2e/kubevirt/guest-init.sh" kvpoc-guest-rootfs:/opt/openshell/init
docker cp "${SUPERVISOR}" kvpoc-guest-rootfs:/usr/local/bin/openshell-sandbox
docker export kvpoc-guest-rootfs -o "${WORKDIR}/rootfs.tar"
docker rm kvpoc-guest-rootfs >/dev/null

# 2. Unpack the exported rootfs.
mkdir -p "${ROOTFS}"
tar -xpf "${WORKDIR}/rootfs.tar" -C "${ROOTFS}"
chmod 0755 "${ROOTFS}/usr/local/bin/openshell-sandbox" "${ROOTFS}/opt/openshell/init"

# Usr-merge /bin and /sbin into /usr: the supervisor's default Landlock
# policy allows exec under /usr (glibc images are usr-merged; Alpine is not,
# so busybox in /bin would be denied with EACCES).
merge_into() {
    local src="$1" dest="$2"
    mkdir -p "${dest}"
    cp -a "${src}/." "${dest}/"
    rm -rf "${src}"
    ln -s "usr/$(basename "${dest}")" "${src}"
}
merge_into "${ROOTFS}/bin" "${ROOTFS}/usr/bin"
merge_into "${ROOTFS}/sbin" "${ROOTFS}/usr/sbin"

# 3. Render the ext4 rootfs disk.
#    ~700 MiB: Alpine base + kernel + supervisor with headroom for runtime.
mkfs.ext4 -F -q -d "${ROOTFS}" "${WORKDIR}/disk.raw" 700m

# 4. Compress to qcow2 and stage the kernel artifacts.
qemu-img convert -c -O qcow2 "${WORKDIR}/disk.raw" "${WORKDIR}/disk.qcow2"
cp "${ROOTFS}/boot/vmlinuz-virt" "${WORKDIR}/vmlinuz-virt"

# Alpine 3.22's stock initramfs-virt is missing the /sysroot mountpoint its
# own init mounts the root filesystem on, so root mounting fails with
# ENOENT. Repack the initramfs with the directory added.
mkdir -p "${WORKDIR}/initrd"
(
    cd "${WORKDIR}/initrd"
    zcat "${ROOTFS}/boot/initramfs-virt" | cpio -idm --quiet
    mkdir -p sysroot
    find . | cpio -o -H newc --quiet | gzip -9 > "${WORKDIR}/initramfs-virt"
)

# 5. Assemble the containerDisk image. KubeVirt >= 1.9 runs each
#    containerDisk image as a virt-launcher init container executing
#    /container-disk-binary/usr/bin/container-disk, so the helper binary
#    (taken from the matching virt-launcher image) must ship inside the
#    containerDisk image itself.
docker pull "quay.io/kubevirt/virt-launcher:${KUBEVIRT_VERSION}" >/dev/null 2>&1 || true
docker rm -f kvpoc-vl-ref >/dev/null 2>&1 || true
docker create --name kvpoc-vl-ref "quay.io/kubevirt/virt-launcher:${KUBEVIRT_VERSION}" >/dev/null
docker cp kvpoc-vl-ref:/usr/bin/container-disk "${WORKDIR}/container-disk"
docker rm kvpoc-vl-ref >/dev/null

cat > "${WORKDIR}/Dockerfile" <<EOF
# syntax=docker/dockerfile:1.4
FROM scratch
COPY disk.qcow2 /disk/disk.qcow2
COPY vmlinuz-virt /boot/vmlinuz-virt
COPY initramfs-virt /boot/initramfs-virt
COPY container-disk /container-disk-binary/usr/bin/container-disk
EOF
docker build -t "${IMAGE_TAG}" "${WORKDIR}"

echo "==> built ${IMAGE_TAG}"
docker image inspect "${IMAGE_TAG}" --format 'size: {{.Size}} bytes'
echo "==> import into the cluster with: k3d image import ${IMAGE_TAG} -c kvpoc"
