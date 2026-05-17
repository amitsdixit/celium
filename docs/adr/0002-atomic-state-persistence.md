# ADR 0002 — Atomic Controller State Persistence

Status: Accepted (2026-05-17, W19 Phase B)

## Context

`celcli::vm::Controller` persists its slot table to a JSON state file
that the operator points at via `--state-file`. Until W19 Phase B the
write path was a naïve `fs::write` of the serialised state. That is
*not* crash-safe:

* A power loss between `open(O_WRONLY|O_TRUNC)` and `write` leaves an
  empty file — every VM slot disappears.
* A power loss midway through `write` leaves a torn file — JSON
  deserialisation fails and the operator sees `controller state:
  malformed json` with no way to recover.
* Parallel test runs that share a state path can race a partial
  truncate-then-write against a concurrent read.

The boot-blob staging path (`celcli::boot::stage_boot_blob`) has the
same problem with higher consequences: a torn `boot.blob` produces a
bogus CRC-32C on the next start and trips W19-A's drift detector
even though the source image is unchanged.

## Decision

Both write paths adopt the same **write-to-tmp → fsync → rename**
pattern:

1. `let tmp = path.with_extension(format!("tmp.{}", process::id()))`
   — a per-PID sidecar in the destination directory so parallel
   tests never collide.
2. Create + write + `File::sync_all()` against `tmp`.
3. Drop the file handle (Windows refuses to rename open files).
4. `fs::rename(&tmp, path)`. POSIX `rename(2)` and Windows
   `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` are both atomic against
   existing destinations on every supported platform.
5. On rename failure the tmp sidecar is `remove_file`'d so we never
   leak it.

The per-PID suffix is the load-bearing detail: two controllers
pointed at the same path from different test processes each have
their own sidecar, and the final atomic rename guarantees one wins
cleanly without the other observing a half-written file.

## Consequences

* `Controller::save` and `boot::stage_boot_blob` are now crash-safe:
  every observable state on disk is either "the previous good
  version" or "the new good version", never something in between.
* W19 Phase B added 4 dedicated tests asserting that a torn body or
  a missing rename leaves the previous file intact, and that a
  fresh controller process picks up the durable state and refuses
  to start a drifted VM.
* Parallel-test flakiness on the volume supervisor test
  (`supervisor_preserves_volume_attachments_across_restart`) is
  reduced — the controller no longer races on the state file. The
  remaining Windows-only flake under high parallelism is a
  membership-timing artefact, not a persistence one.
* Marginal cost: one extra `create` + `fsync` + `rename` per save.
  At our save rate (operator actions, not hot-path) the overhead is
  invisible.

## Alternatives considered

* **`OpenOptions::truncate(true)` + `fsync`** — easier, but the
  truncate-then-write window remains; a crash there empties the
  file.
* **`std::fs::write` + a sibling lock file** — adds locking
  complexity and does nothing to protect against partial writes.
* **A small embedded database** — overkill for a slot table of size
  4 and would have to be vendored or pinned to a specific Rust
  toolchain.

## References

* W19 Phase B test set: `crates/celcli/src/vm.rs::tests` —
  `save_is_crash_safe_against_torn_tmp_file`,
  `crash_safe_save_preserves_drift_detection_across_processes`.
* `crates/celcli/src/boot.rs::stage_boot_blob` for the staging
  half of the same pattern.
