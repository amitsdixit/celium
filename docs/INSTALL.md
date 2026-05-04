# Celium — Installation Guide

This guide covers the development environment for the std workspace
(`celcommon`, `celmesh`, `celvault`, `celcli`, `celtest`). Bare-metal
crates (`celloader`, `celhyper`) require additional custom-target
toolchains and are covered at the end.

## 1. Prerequisites

| Requirement | Why |
| --- | --- |
| Rust **1.82+** | Workspace MSRV (`Cargo.toml -> rust-version`). |
| `rustup` | Toolchain & target management. The repo's `rust-toolchain.toml` pins exact components. |
| `git` | Source. |
| ~5 GB free disk | Workspace + `target/` debug & release artifacts. |

### Windows (recommended path tested in CI-equivalent dev)

The `x86_64-pc-windows-gnu` toolchain ships a broken `dlltool.exe`.
Install MinGW from Scoop and prepend it to `PATH`:

```powershell
scoop install mingw
$env:PATH = 'C:\Users\<user>\scoop\apps\mingw\current\bin;' + $env:PATH
$env:CARGO_INCREMENTAL = '0'   # avoids intermittent "file in use" on AV-scanned tmp files
```

### Linux / macOS

Standard `rustup default stable` works; no extra system packages are
required for the std workspace.

## 2. Clone and bootstrap

```bash
git clone <your fork or upstream> celium
cd celium
rustup show          # installs the toolchain pinned by rust-toolchain.toml
```

`rust-toolchain.toml` requests `rust-src` and the bare-metal targets
`x86_64-unknown-uefi` and `x86_64-unknown-none` automatically.

## 3. Build the std workspace

```bash
cargo build --workspace
```

Release build (produces `target/release/celctl(.exe)`):

```bash
cargo build --workspace --release
```

Expected size of `celctl` on Windows GNU + thin LTO ≈ **11.5 MiB**.

## 4. Run the test suite

```bash
cargo test --workspace -- --test-threads=1
```

Serial execution is recommended because the multi-node UDP tests bind
to ephemeral loopback ports and create transient sockets — running
them in parallel works on most hosts but can flake under heavy load.

A clean run of W12 reports **73 tests passing** across the workspace,
including:

* `celvault` — 4 unit tests (volume CRUD, range checks, monotonic ids).
* `celmesh::host` — 3 host model tests (incl. attach/detach round-trip).
* `celmesh` — 18 total (gossip, federation, RPC, supervisor).
* `celtest::multi_node_volume` — 3 real-UDP integration tests for
  W12 (live guest with persistent volume; supervisor preserves
  attachments across restart; cross-node volume CRUD).

## 5. Install `celctl` for local use

After a release build:

```powershell
# Windows
Copy-Item target\release\celctl.exe $env:USERPROFILE\bin\celctl.exe
$env:PATH = "$env:USERPROFILE\bin;" + $env:PATH
```

```bash
# Linux / macOS
install -Dm755 target/release/celctl ~/.local/bin/celctl
```

Verify:

```bash
celctl version
celctl --help
```

## 6. (Optional) Bare-metal crates

Both bare-metal packages are excluded from the std workspace and
build against custom targets.

```bash
# UEFI stage-0
cd bootloader/celloader
cargo build --target x86_64-unknown-uefi --release

# Hypervisor core
cd ../../crates/celhyper
cargo build --target x86_64-unknown-none --release
```

These are work-in-progress and have separate documentation in
[`docs/01_CELHYPER.md`](01_CELHYPER.md).

## 7. Troubleshooting

| Symptom | Fix |
| --- | --- |
| `dlltool.exe` `CreateProcess` errors on Windows | Install MinGW via Scoop and prepend its `bin` to `PATH` (see §1). |
| Intermittent "file in use" linker errors on Windows | `set CARGO_INCREMENTAL=0`. |
| `cargo test` flakes on multi-node UDP | Re-run with `--test-threads=1`. |
| `celctl cluster start` exits immediately | Default `--duration 0` runs until Ctrl-C; pass `--duration N` for a fixed lifetime. |
| Address-in-use on UDP bind | Use `--bind 127.0.0.1:0` to let the OS pick a port; read it from `cluster status` output. |
