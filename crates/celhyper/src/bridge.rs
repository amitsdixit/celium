//! W22-B-2 — kernel-side bridge main loop.
//!
//! Reads one [`wire::Request`] per NDJSON line off COM1 RX, dispatches
//! it into [`crate::manager`], encodes the resulting [`wire::Reply`],
//! and writes the line to COM1 TX. Runs forever; the only way out is
//! a panic or a power-cycle.
//!
//! # Framing
//!
//! * One JSON object per line, terminated by `\n` (CRLF tolerated).
//! * Maximum frame length [`MAX_FRAME_BYTES`] — must equal the host
//!   constant `celmesh::hyper_serial::MAX_FRAME_BYTES` (64 KiB). The
//!   kernel's static frame buffer is smaller because the bridge
//!   never *receives* a 64 KiB request (the largest request is
//!   `create` with a 32-byte label); the cap exists so a buggy peer
//!   spraying junk can't pin the kernel into an infinite read.
//!
//! # Error policy
//!
//! Wire-decode failures and host-side dispatch failures both surface
//! as a logged kernel line plus a connection-style teardown: the
//! kernel keeps running but stops processing the current "session"
//! by simply continuing to read frames. A future revision will gate
//! that with a periodic `HELLO`/`HELLO_OK` ping; v1 is best-effort.

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};
use crate::manager;
use crate::serial_io;
use crate::vm::{VmId, VmState};
use crate::wire::{
    decode_request, encode_reply, ErrorBuf, ImagePathBuf, LabelBuf, Reply, Request, Row, StateTag,
    IMAGE_PATH_MAX, LABEL_MAX, ROWS_MAX,
};

/// Maximum bytes one request line may consume.
///
/// The kernel sizes its RX buffer to comfortably fit the largest
/// possible W22-v1 request (`create` with the maximum label) plus
/// slop for whitespace and forward-compat fields. Anything bigger
/// is logged + dropped, not parsed.
pub const MAX_FRAME_BYTES: usize = 256;

/// Compile-time assertion that the wire's row capacity matches the
/// kernel's VM table size. If you bump `manager::MAX_VMS`, also bump
/// `wire::ROWS_MAX`.
const _: () = assert!(crate::wire::ROWS_MAX == crate::manager::MAX_VMS);

/// Static RX buffer. Bridge is single-threaded against the UART
/// mutex, so a static avoids any stack pressure in the boot path.
static mut RX_BUF: [u8; MAX_FRAME_BYTES] = [0; MAX_FRAME_BYTES];

/// Static TX buffer. Sized to fit the worst-case `Listed` reply:
/// `MAX_VMS` rows × ~280 bytes each (label + image_path + numeric
/// extras) + framing. Bumped from 1 KiB in W23-B when rows started
/// carrying the W22-v2 image / config fields.
const TX_CAP: usize = 2048;
static mut TX_BUF: [u8; TX_CAP] = [0; TX_CAP];

/// Enter the bridge loop. Never returns under normal operation.
///
/// # Panics
///
/// Does not panic on parse failure (those become logged errors).
/// Will panic via the kernel `panic_handler` if [`manager`] returns
/// a class of error the bridge cannot translate into a wire reply;
/// today every manager error is converted to a "session teardown".
pub fn run() -> ! {
    // Initialise the dedicated bridge UART (COM2, 0x2F8). The kernel
    // logger lives on COM1; the bridge gets its own port so a host
    // SerialHyperLink can connect over QEMU's `-serial tcp:` without
    // having to demux log noise off the wire.
    serial_io::init();
    crate::logger::log("celhyper: bridge ready on COM2 (NDJSON, celhyper-ipc/1)");
    loop {
        // SAFETY: this function is the unique caller and runs on a
        // single thread (the BSP). The static buffers are not
        // shareable from any other code path.
        let n = match serial_io::read_line(unsafe { &mut RX_BUF[..] }) {
            Ok(n) => n,
            Err(_) => {
                crate::logger::log("celhyper: bridge: oversized RX line; resync");
                continue;
            }
        };
        // SAFETY: same as above; n ≤ MAX_FRAME_BYTES by construction.
        let frame = unsafe { &RX_BUF[..n] };
        if frame.iter().all(|b| matches!(*b, b' ' | b'\t')) {
            continue; // empty keep-alive
        }
        let req = match decode_request(frame) {
            Ok(r) => r,
            Err(e) => {
                // The host sent us bytes it expects a reply for —
                // surface a structured error so its `call()` doesn't
                // time out. The session continues; this is *not* a
                // teardown.
                log_err("bridge: decode", &e);
                let err_reply = Reply::Error {
                    message: ErrorBuf::from_slice_truncating(error_message(&e)),
                };
                match encode_reply(&err_reply, unsafe { &mut TX_BUF[..] }) {
                    Ok(n) => serial_io::write_all(unsafe { &TX_BUF[..n] }),
                    Err(ee) => log_err("bridge: encode-of-decode-err", &ee),
                }
                continue;
            }
        };
        let reply = match dispatch(req) {
            Ok(r) => r,
            Err(e) => {
                // W23-B: surface dispatch errors as a structured
                // `Reply::Error` instead of silently dropping the
                // call. Without this the host SerialHyperLink would
                // time out after `CALL_TIMEOUT` (1s) every time the
                // kernel rejected an op (e.g. Delete on a non-
                // terminal VM), with no diagnostic.
                log_err("bridge: dispatch", &e);
                Reply::Error { message: ErrorBuf::from_slice_truncating(error_message(&e)) }
            }
        };
        // SAFETY: same as above.
        match encode_reply(&reply, unsafe { &mut TX_BUF[..] }) {
            Ok(n) => {
                // SAFETY: same as above; n ≤ TX_CAP.
                serial_io::write_all(unsafe { &TX_BUF[..n] });
            }
            Err(e) => log_err("bridge: encode", &e),
        }
    }
}

fn dispatch(req: Request<'_>) -> HyperResult<Reply> {
    match req {
        Request::Create { label, image_path, cpu_count, memory_mib, boot_blob_crc32c } => {
            if label.len() > LABEL_MAX {
                return Err(HyperError::Invalid("bridge: label > 32 chars"));
            }
            if let Some(p) = image_path {
                if p.len() > IMAGE_PATH_MAX {
                    return Err(HyperError::Invalid("bridge: image_path > 128 chars"));
                }
            }
            // Labels + image metadata are stored in a side table
            // indexed by slot id so List replies can echo them back.
            // The kernel `manager` itself never sees these fields —
            // keeping its struct surface minimal is a deliberate W22
            // goal. The image is *not* loaded into the guest today;
            // the kernel still runs the canned HELLO bring-up
            // template. The metadata path closes the host→kernel
            // drift-detection loop ahead of the loader landing.
            let req = manager::CreateVmRequest::hello();
            let id = manager::create_vm(&req)?;
            remember_label(id, label)?;
            remember_extras(id, image_path, cpu_count, memory_mib, boot_blob_crc32c)?;
            Ok(Reply::Created { vm_id: id.0 })
        }
        Request::Start { vm_id } => {
            let id = VmId(vm_id);
            manager::start_vm(id)?;
            let state = state_tag(manager::vm_state(id)?);
            let last_exit = manager::vm_last_exit(id)?;
            Ok(Reply::State { vm_id, state, last_exit })
        }
        Request::Stop { vm_id } => {
            let id = VmId(vm_id);
            manager::stop_vm(id)?;
            let state = state_tag(manager::vm_state(id)?);
            let last_exit = manager::vm_last_exit(id)?;
            Ok(Reply::State { vm_id, state, last_exit })
        }
        Request::Delete { vm_id } => {
            let id = VmId(vm_id);
            manager::delete_vm(id)?;
            forget_label(id);
            forget_extras(id);
            Ok(Reply::Deleted { vm_id })
        }
        Request::List => Ok(Reply::Listed { rows: build_rows() }),
    }
}

fn state_tag(s: VmState) -> StateTag {
    match s {
        VmState::Created => StateTag::Created,
        VmState::Running => StateTag::Running,
        VmState::Halted  => StateTag::Halted,
        VmState::Stopped => StateTag::Stopped,
        VmState::Faulted => StateTag::Faulted,
    }
}

// ---------------------------------------------------------------------------
// Label side-table
// ---------------------------------------------------------------------------

static LABELS: spin::Mutex<[LabelBuf; ROWS_MAX]> =
    spin::Mutex::new([LabelBuf::empty(); ROWS_MAX]);

fn remember_label(id: VmId, label: &[u8]) -> HyperResult<()> {
    let i = id.0 as usize;
    if i >= ROWS_MAX {
        return Err(HyperError::Denied("bridge: VmId out of range"));
    }
    let buf = LabelBuf::from_slice(label)
        .ok_or(HyperError::Invalid("bridge: label > 32 chars"))?;
    LABELS.lock()[i] = buf;
    Ok(())
}

fn forget_label(id: VmId) {
    let i = id.0 as usize;
    if i < ROWS_MAX {
        LABELS.lock()[i] = LabelBuf::empty();
    }
}

// ---------------------------------------------------------------------------
// Image / config side-table (W23-B)
//
// Mirrors `LABELS` but carries the optional VM configuration the
// controller knows about: the host-side image path, vCPU count,
// guest memory in MiB, and the staged boot-blob CRC32C. These ride
// in `Reply::Listed` rows so a controller running `celctl cluster
// vms` against a live kernel sees the same metadata it gossips
// between host nodes.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Extras {
    image_path: ImagePathBuf,
    cpu_count: Option<u32>,
    memory_mib: Option<u64>,
    boot_blob_crc32c: Option<u32>,
}

impl Extras {
    const fn empty() -> Self {
        Self {
            image_path: ImagePathBuf::empty(),
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        }
    }
}

static EXTRAS: spin::Mutex<[Extras; ROWS_MAX]> =
    spin::Mutex::new([const { Extras::empty() }; ROWS_MAX]);

fn remember_extras(
    id: VmId,
    image_path: Option<&[u8]>,
    cpu_count: Option<u32>,
    memory_mib: Option<u64>,
    boot_blob_crc32c: Option<u32>,
) -> HyperResult<()> {
    let i = id.0 as usize;
    if i >= ROWS_MAX {
        return Err(HyperError::Denied("bridge: VmId out of range"));
    }
    let image_path = match image_path {
        None => ImagePathBuf::empty(),
        Some(p) => ImagePathBuf::from_slice(p)
            .ok_or(HyperError::Invalid("bridge: image_path > 128 chars"))?,
    };
    EXTRAS.lock()[i] = Extras { image_path, cpu_count, memory_mib, boot_blob_crc32c };
    Ok(())
}

fn forget_extras(id: VmId) {
    let i = id.0 as usize;
    if i < ROWS_MAX {
        EXTRAS.lock()[i] = Extras::empty();
    }
}

fn build_rows() -> [Option<Row>; ROWS_MAX] {
    let (entries, _) = manager::list_vms();
    let labels = *LABELS.lock();
    let extras = *EXTRAS.lock();
    let mut out: [Option<Row>; ROWS_MAX] = [None; ROWS_MAX];
    for (i, entry) in entries.iter().enumerate() {
        if let Some(e) = entry {
            out[i] = Some(Row {
                vm_id: e.id.0,
                label: labels[i],
                state: state_tag(e.state),
                last_exit: e.last_exit,
                image_path: extras[i].image_path,
                cpu_count: extras[i].cpu_count,
                memory_mib: extras[i].memory_mib,
                boot_blob_crc32c: extras[i].boot_blob_crc32c,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Logging helper
// ---------------------------------------------------------------------------

fn log_err(prefix: &str, e: &HyperError) {
    let msg = match e {
        HyperError::InvalidHandoff(m)
        | HyperError::UnsupportedCpu(m)
        | HyperError::Hardware(m)
        | HyperError::Exhausted(m)
        | HyperError::Denied(m)
        | HyperError::Invalid(m)
        | HyperError::Unimplemented(m)
        | HyperError::Internal(m) => *m,
    };
    crate::logger::log(prefix);
    crate::logger::log(msg);
}

/// Map a [`HyperError`] to the wire-side `"kind: payload"` byte
/// slice used in `Reply::Error`. Lives in the kernel because we
/// don't have `format!` available; the kind tags are static.
fn error_message(e: &HyperError) -> &'static [u8] {
    // We can't allocate a "kind: payload" string in `no_std` without
    // an allocator, so the payload alone is the message. Hosts log
    // it as `hyper: kernel: <payload>` which is sufficient for
    // operator-facing diagnostics.
    let msg = match e {
        HyperError::InvalidHandoff(m)
        | HyperError::UnsupportedCpu(m)
        | HyperError::Hardware(m)
        | HyperError::Exhausted(m)
        | HyperError::Denied(m)
        | HyperError::Invalid(m)
        | HyperError::Unimplemented(m)
        | HyperError::Internal(m) => *m,
    };
    msg.as_bytes()
}
