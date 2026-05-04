# 00 — Global Conventions for Celium

This file applies to every component in Celium. It establishes the coding, repository, and operational conventions that hold across the entire project.

## Languages
- **Rust** for all components (CelHyper, CelMesh, CelVault, CLI, test harness). Rust is chosen for memory safety in security-critical code, performance comparable to C, and a modern ecosystem.
- **C** only where unavoidable (early boot assembly in CelLoader).
- **Python** for test orchestration, chaos scripts, and developer utilities.

## Repository layout
celium/
├── README.md
├── Cargo.toml
├── crates/
│   ├── celhyper/               # micro-hypervisor (bare-metal Rust)
│   ├── celmesh/                # gossip + plasticity fabric
│   ├── celvault/               # secure storage + networking
│   ├── celcli/                 # operator CLI
│   ├── celtest/                # test harness + chaos
│   └── celcommon/              # shared types, errors, tracing, metrics
├── bootloader/                 # CelLoader (UEFI stage-0)
├── docs/                       # architecture, ADRs, runbooks
├── tests/                      # integration + chaos tests
└── deploy/                     # first-boot and cluster bootstrap scripts


## Versioning, Error handling, Logging, Observability, Concurrency, Testing, Build
(Same strict rules as before — Result<T, CelError>, tracing, prometheus, tokio, no unwrap/panic in production, etc.)

## What “done” means
A component is done when it passes its acceptance criteria, has integration tests, survives failure injection, has full rustdoc, and has a runbook.

**This file is non-negotiable.**
