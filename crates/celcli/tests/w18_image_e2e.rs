//! W18.4 — end-to-end test of the image-backed VM lifecycle through
//! the `celctl` binary.
//!
//! Exercises the full operator path:
//!
//!   1. Synthesise a 4 KiB raw disk image filled with a known pattern.
//!   2. `celctl vm create --image <path> --label e2e` writes the row.
//!   3. `celctl vm start /vms/0` triggers
//!      [`celcli::boot::stage_boot_blob`] before the state transition.
//!   4. Inspect the persisted JSON state file and the staged
//!      `boot.blob` to confirm the digest matches what the operator
//!      would compute by hand.
//!
//! Uses `CARGO_BIN_EXE_celctl` (auto-set by cargo for the binary
//! defined in this same crate) so the test is hermetic and doesn't
//! rely on `PATH` lookups.

use std::path::Path;
use std::process::Command;

/// Run `celctl <args...>` from `cwd` and assert exit 0. Returns the
/// captured stdout (bytes — callers can parse as utf-8 if they want).
fn run_celctl(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let exe = env!("CARGO_BIN_EXE_celctl");
    let out = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn celctl");
    assert!(
        out.status.success(),
        "celctl {:?} failed: status={:?}\nstdout={}\nstderr={}",
        args,
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

#[test]
fn create_then_start_with_image_stages_boot_blob_and_records_digest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path();

    // (1) Synth a 4 KiB raw image. Pattern 0xC3 (== x86 `ret`) is
    // arbitrary but deterministic so we can recompute the expected
    // CRC-32C without a fixture.
    let img_path = cwd.join("disk.img");
    std::fs::write(&img_path, vec![0xC3u8; 4096]).expect("write image");

    // (2) Create. We use a non-default `--state-file` so the test
    // doesn't depend on `default_state_path()` rooting at
    // `./build/celctl-state.json` (which would otherwise touch the
    // workspace's real `build/` directory).
    let state_file = cwd.join("state.json");
    run_celctl(
        cwd,
        &[
            "--state-file",
            state_file.to_str().unwrap(),
            "vm",
            "create",
            "--label",
            "e2e",
            "--image",
            img_path.to_str().unwrap(),
        ],
    );

    // (3) Start. This is where boot-blob staging fires.
    run_celctl(
        cwd,
        &[
            "--state-file",
            state_file.to_str().unwrap(),
            "vm",
            "start",
            "/vms/0",
        ],
    );

    // (4a) Validate persisted state: boot_blob_len/crc32c stamped.
    let raw = std::fs::read(&state_file).expect("read state file");
    let json: serde_json::Value =
        serde_json::from_slice(&raw).expect("parse state json");
    let rec = &json["slots"][0]
        .as_object()
        .expect("slot 0 should be populated");
    assert_eq!(rec["state"], "Halted");
    assert_eq!(rec["label"], "e2e");
    assert_eq!(
        rec["image_path"].as_str().unwrap(),
        img_path.display().to_string(),
    );
    assert_eq!(rec["boot_blob_len"].as_u64().unwrap(), 4096);
    let recorded_crc = rec["boot_blob_crc32c"].as_u64().unwrap() as u32;
    let expected_crc = celimage::crc32c(&[0xC3u8; 4096]);
    assert_eq!(recorded_crc, expected_crc);

    // (4b) Validate staged blob on disk. The CLI roots staging at
    // `cwd/build/stage` (via `vm::default_stage_root()`), so the
    // staged blob lives under `build/stage/vm-0/boot.blob`.
    let staged = cwd.join("build").join("stage").join("vm-0").join("boot.blob");
    let staged_bytes = std::fs::read(&staged).expect("read staged blob");
    assert_eq!(staged_bytes.len(), 4096);
    assert!(staged_bytes.iter().all(|&b| b == 0xC3));
    assert_eq!(celimage::crc32c(&staged_bytes), expected_crc);

    // (4c) `vm list` must surface the row without crashing. We don't
    // pin the exact text shape — that's covered by unit tests — but
    // the label MUST appear so we're sure we read the right state.
    let listed = run_celctl(
        cwd,
        &[
            "--state-file",
            state_file.to_str().unwrap(),
            "vm",
            "list",
        ],
    );
    let listed = String::from_utf8(listed).unwrap();
    assert!(listed.contains("e2e"), "vm list output missing label: {listed}");
    assert!(
        listed.contains("halted"),
        "vm list output missing state tag: {listed}",
    );
}

#[test]
fn create_with_missing_image_rejected_before_record_written() {
    // Validates that the create-time `celimage::inspect` guard fires
    // for a non-existent path, so the operator gets the error at
    // create time (not later at start time).
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path();
    let state_file = cwd.join("state.json");
    let missing = cwd.join("does-not-exist.img");

    let exe = env!("CARGO_BIN_EXE_celctl");
    let out = Command::new(exe)
        .args([
            "--state-file",
            state_file.to_str().unwrap(),
            "vm",
            "create",
            "--label",
            "bad",
            "--image",
            missing.to_str().unwrap(),
        ])
        .current_dir(cwd)
        .output()
        .expect("spawn celctl");
    assert!(
        !out.status.success(),
        "celctl vm create with missing image should fail; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // State file must not have been created — the failure happens
    // before any controller mutation reaches disk.
    assert!(
        !state_file.exists(),
        "state file must not be written when create fails",
    );
}
