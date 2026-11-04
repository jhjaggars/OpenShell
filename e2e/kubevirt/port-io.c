// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Minimal x86 port-I/O helper for the e2e probes (static musl build,
// shipped in the guest image next to idmap-mount).
//
// /dev/port performs byte-granular IN/OUT, but emulated registers like the
// PCI config address (0xcf8) and the ICH9 TCO counters need word/dword
// access — byte writes are silently dropped by the emulation. This helper
// performs real OUT instructions at the requested width after iopl(3).
//
// Usage:
//   port-io read8  <port>          -> hex value on stdout
//   port-io read16 <port>
//   port-io read32 <port>
//   port-io write8  <port> <value>
//   port-io write16 <port> <value>
//   port-io write32 <port> <value>
//
// Ports and values are accepted in decimal or 0x-hex. Exits non-zero on
// argument or iopl failure.

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/io.h>

static unsigned long parse_num(const char *text) {
    return strtoul(text, NULL, 0);
}

static int fail_usage(void) {
    fprintf(stderr,
            "usage: port-io read8|read16|read32 <port>\n"
            "       port-io write8|write16|write32 <port> <value>\n");
    return 2;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        return fail_usage();
    }
    if (iopl(3) != 0) {
        fprintf(stderr, "iopl(3) failed: %s\n", strerror(errno));
        return 1;
    }
    const char *op = argv[1];
    unsigned long port = parse_num(argv[2]);
    if (port > 0xffff) {
        fprintf(stderr, "port out of range: %lu\n", port);
        return 2;
    }

    if (strcmp(op, "read8") == 0) {
        printf("0x%02x\n", inb((unsigned short)port));
        return 0;
    }
    if (strcmp(op, "read16") == 0) {
        printf("0x%04x\n", inw((unsigned short)port));
        return 0;
    }
    if (strcmp(op, "read32") == 0) {
        printf("0x%08x\n", inl((unsigned short)port));
        return 0;
    }
    if (argc < 4) {
        return fail_usage();
    }
    unsigned long value = parse_num(argv[3]);
    if (strcmp(op, "write8") == 0) {
        outb((uint8_t)value, (unsigned short)port);
        return 0;
    }
    if (strcmp(op, "write16") == 0) {
        outw((uint16_t)value, (unsigned short)port);
        return 0;
    }
    if (strcmp(op, "write32") == 0) {
        outl((uint32_t)value, (unsigned short)port);
        return 0;
    }
    return fail_usage();
}
