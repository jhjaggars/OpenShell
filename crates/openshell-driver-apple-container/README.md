# openshell-driver-apple-container

In-process `ComputeDriver` backend that manages sandbox containers using
Apple's [`container`](https://github.com/apple/container) CLI on macOS
with Apple Silicon. Each sandbox runs as a lightweight Linux VM via the
macOS Virtualization framework.

## Prerequisites

- **macOS 26 (Tahoe)** or later on Apple Silicon
- Apple `container` CLI installed from the
  [GitHub releases](https://github.com/apple/container/releases)
- System service running: `container system start`
- A Linux arm64 `openshell-sandbox` binary (cross-compile with
  `aarch64-unknown-linux-musl` target)

## Configuration

Configure via `[openshell.drivers.apple-container]` in the gateway TOML:

```toml
[openshell.drivers.apple-container]
supervisor_bin   = "/path/to/openshell-sandbox"  # Required
default_image    = "ubuntu:latest"
host_gateway_ip  = "192.168.64.1"                # vmnet gateway default
```

The `supervisor_bin` field is required and must point to a statically
linked Linux arm64 binary. Cross-compile with:

```shell
cargo build --target aarch64-unknown-linux-musl -p openshell-sandbox --release
```

This requires the `musl-cross` toolchain (`brew install filosottile/musl-cross/musl-cross`).

## Architecture

The driver shells out to the `container` CLI for all lifecycle operations.
Apple's tool communicates internally via XPC — there is no REST API.

| Module | Purpose |
|--------|---------|
| `cli.rs` | Typed wrapper around the `container` binary |
| `config.rs` | `AppleContainerComputeConfig` (gateway TOML) |
| `driver.rs` | `AppleContainerComputeDriver` lifecycle operations |
| `grpc.rs` | `ComputeDriverService` implementing the `ComputeDriver` trait |
| `watcher.rs` | Poll-based sandbox state observation (3s interval) |

## How it works

1. The driver runs `container run -d` with OpenShell management labels
   and bind-mounts the supervisor binary at `/openshell-sandbox`.
2. The supervisor starts inside the VM, creates network namespaces for
   policy enforcement, and connects back to the gateway via the
   `ConnectSupervisor` gRPC relay at the `host_gateway_ip`.
3. The gateway promotes the sandbox to Ready once the relay is established.
4. Deletion calls `container stop` then `container rm`.
