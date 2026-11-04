// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// dm-verity device setup for the OpenShell KubeVirt guest (static musl
// build, shipped in the repacked initramfs).
//
// The guest's rootfs block device carries an appended dm-verity hash tree
// (no superblock); the trusted geometry and root hash arrive on the kernel
// command line (driver-rendered). This helper creates the verity mapping
// through the device-mapper ioctl interface — no libdevmapper, no
// veritysetup, no shared libraries: the initramfs stays self-contained.
//
// The kernel verifies every read of the mapped device against the hash
// tree; a mismatch returns EIO. The init powers the guest off on that
// path (a tampered root is a dead sandbox, not a silent downgrade).
//
// Usage:
//   verity-open <name> <data_dev> <num_data_blocks> <hash_start_block> <root_hash>
//
// The table (dm-verity version 1, sha256, 4096-byte blocks, empty salt):
//   0 <num_data_blocks*8> verity 1 <dev> <dev> 4096 4096
//       <num_data_blocks> <hash_start_block> sha256 <root_hash> -
//
// Exits non-zero on any ioctl failure with a diagnostic on stderr.

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/sysmacros.h>
#include <sys/stat.h>
#include <unistd.h>
#include <linux/dm-ioctl.h>

#define CONTROL_DEV "/dev/mapper/control"

static void dm_init_header(struct dm_ioctl *header, size_t payload_size) {
    memset(header, 0, sizeof(*header));
    // Request the base v4 ioctl interface: the kernel rejects a user
    // version NEWER than its own (compile-time headers can outrun the
    // running kernel — e.g. headers 4.49 vs the guest's 6.12 dm at 4.48),
    // while any minor at or below the kernel's is accepted. The dm_ioctl
    // layout and the commands used here are stable across the whole v4
    // line.
    header->version[0] = 4;
    header->version[1] = 0;
    header->version[2] = 0;
    header->data_size = sizeof(*header) + payload_size;
    header->data_start = sizeof(*header);
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr,
                "usage: verity-open <name> <data_dev> <num_data_blocks>"
                " <hash_start_block> <root_hash>\n");
        return 2;
    }
    const char *name = argv[1];
    const char *data_dev = argv[2];
    unsigned long num_data_blocks = strtoul(argv[3], NULL, 0);
    unsigned long hash_start_block = strtoul(argv[4], NULL, 0);
    const char *root_hash = argv[5];

    if (num_data_blocks == 0 || hash_start_block == 0) {
        fprintf(stderr, "implausible geometry: blocks=%lu hash_start=%lu\n",
                num_data_blocks, hash_start_block);
        return 2;
    }
    unsigned long sectors = num_data_blocks * (4096 / 512);

    // Resolve the data device to major:minor: the kernel's table parser
    // resolves paths against the caller's namespace, which is fragile
    // from an initramfs; the device numbers are unambiguous.
    struct stat dev_st;
    if (stat(data_dev, &dev_st) != 0 || !S_ISBLK(dev_st.st_mode)) {
        fprintf(stderr, "stat %s failed: %s\n", data_dev, strerror(errno));
        return 1;
    }
    unsigned int dev_major = major(dev_st.st_rdev);
    unsigned int dev_minor = minor(dev_st.st_rdev);
    char dev_number[32];
    snprintf(dev_number, sizeof(dev_number), "%u:%u", dev_major, dev_minor);

    int fd = open(CONTROL_DEV, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "open %s failed: %s (is dm_mod loaded?)\n",
                CONTROL_DEV, strerror(errno));
        return 1;
    }

    // 0. Version handshake: also validates the ioctl plumbing before the
    // real commands (a failure here means the control path itself).
    struct {
        struct dm_ioctl header;
        char payload[128];
    } version;
    dm_init_header(&version.header, sizeof(version.payload));
    if (ioctl(fd, DM_VERSION, &version) != 0) {
        fprintf(stderr, "DM_VERSION failed: %s (kernel %u.%u.%u wants"
                        " %u.%u.%u)\n",
                strerror(errno), version.header.version[0],
                version.header.version[1], version.header.version[2],
                DM_VERSION_MAJOR, DM_VERSION_MINOR, DM_VERSION_PATCHLEVEL);
        return 1;
    }

    // 1. Create the (inactive) device.
    char create_payload[DM_NAME_LEN + DM_UUID_LEN];
    struct {
        struct dm_ioctl header;
        char payload[sizeof(create_payload)];
    } create;
    dm_init_header(&create.header, sizeof(create_payload));
    create.header.flags = DM_READONLY_FLAG;
    if (strlen(name) >= sizeof(create.header.name)) {
        fprintf(stderr, "device name too long\n");
        return 2;
    }
    snprintf((char *)create.header.name, sizeof(create.header.name), "%s", name);
    if (ioctl(fd, DM_DEV_CREATE, &create) != 0) {
        // EEXIST is fine for an idempotent boot retry.
        if (errno != EEXIST) {
            fprintf(stderr, "DM_DEV_CREATE failed: %s\n", strerror(errno));
            return 1;
        }
    }

    // 2. Load the verity table.
    char table[512];
    // The table carries the fixed salt the image build formatted the
    // tree with (build-guest-image.sh: --salt=6f70656e7368656c6c): the
    // kernel interface requires a salt field, and the root hash is the
    // binding anyway — the salt is a mixing constant, not a secret.
    // The ioctl payload carries ONLY the target constructor arguments:
    // the start/length/type live in the dm_target_spec (unlike dmsetup's
    // table string, which prefixes them — passing the prefix makes the ctr
    // receive it as extra arguments and fail validation).
    int table_len = snprintf(table, sizeof(table),
                             "1 %s %s 4096 4096 %lu %lu"
                             " sha256 %s 6f70656e7368656c6c",
                             dev_number, dev_number, num_data_blocks,
                             hash_start_block, root_hash);
    if (table_len < 0 || (size_t)table_len >= sizeof(table)) {
        fprintf(stderr, "verity table too long\n");
        return 2;
    }
    struct {
        struct dm_ioctl header;
        struct dm_target_spec spec;
        char payload[512];
    } load;
    dm_init_header(&load.header, sizeof(load.spec) + sizeof(load.payload));
    load.header.target_count = 1;
    // The load (like the resume) addresses the device BY NAME: without it
    // the kernel cannot find the just-created device and fails with ENXIO.
    // DM_READONLY_FLAG is required here too: the verity target refuses a
    // writable table ("Device must be readonly").
    load.header.flags = DM_READONLY_FLAG;
    snprintf((char *)load.header.name, sizeof(load.header.name), "%s", name);
    load.spec.sector_start = 0;
    load.spec.length = sectors;
    load.spec.status = 0;
    snprintf((char *)load.spec.target_type, sizeof(load.spec.target_type),
             "verity");
    // The table string starts immediately after the (40-byte, 8-aligned)
    // dm_target_spec; `next` is the offset from this spec to the next.
    load.spec.next = (uint32_t)sizeof(struct dm_target_spec);
    memcpy((char *)&load.spec + sizeof(struct dm_target_spec), table,
           (size_t)table_len + 1);
    if (ioctl(fd, DM_TABLE_LOAD, &load) != 0) {
        fprintf(stderr, "DM_TABLE_LOAD failed: %s (bad root hash or"
                        " geometry?)\n",
                strerror(errno));
        return 1;
    }

    // 3. Activate the device: DM_DEV_SUSPEND without DM_SUSPEND_FLAG is
    // the resume ioctl (the kernel header has no separate DM_DEV_RESUME).
    struct {
        struct dm_ioctl header;
        char payload[DM_NAME_LEN + DM_UUID_LEN];
    } resume;
    dm_init_header(&resume.header, sizeof(resume.payload));
    resume.header.flags = DM_READONLY_FLAG;
    snprintf((char *)resume.header.name, sizeof(resume.header.name), "%s", name);
    if (ioctl(fd, DM_DEV_SUSPEND, &resume) != 0) {
        fprintf(stderr, "DM_DEV_SUSPEND(resume) failed: %s\n", strerror(errno));
        return 1;
    }

    // Create the device node: without udev, device-mapper devices do not
    // appear in devtmpfs. The kernel reports the assigned dev_t in the
    // ioctl response header.
    unsigned int dm_major = major(resume.header.dev);
    unsigned int dm_minor = minor(resume.header.dev);
    if (dm_major == 0) {
        fprintf(stderr, "kernel assigned no device number\n");
        return 1;
    }
    char node_path[256];
    snprintf(node_path, sizeof(node_path), "/dev/mapper/%s", name);
    unlink(node_path);
    if (mknod(node_path, S_IFBLK | 0600, makedev(dm_major, dm_minor)) != 0) {
        fprintf(stderr, "mknod %s (%u:%u) failed: %s\n", node_path,
                dm_major, dm_minor, strerror(errno));
        return 1;
    }
    close(fd);
    printf("%s\n", node_path);
    return 0;
}
