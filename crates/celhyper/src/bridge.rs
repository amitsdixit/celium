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
    decode_request, encode_reply, LabelBuf, Reply, Request, Row, StateTag,
    LABEL_MAX, ROWS_MAX,
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
/// `MAX_VMS` rows × ~80 bytes each + framing.
const TX_CAP: usize = 1024;
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
    crate::logger::log("celhyper: bridge ready on COM1 (NDJSON, celhyper-ipc/1)");
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
                log_err("bridge: decode", &e);
                continue;
            }
        };
        let reply = match dispatch(req) {
            Ok(r) => r,
            Err(e) => {
                log_err("bridge: dispatch", &e);
                continue;
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
        Request::Create { label } => {
            if label.len() > LABEL_MAX {
                return Err(HyperError::Invalid("bridge: label > 32 chars"));
            }
            // Labels are stored in a side table indexed by slot id so
            // List replies can echo them back. The kernel `manager`
            // itself never sees the label — keeping its struct
            // surface minimal is a deliberate W22 goal.
            let req = manager::CreateVmRequest::hello();
            let id = manager::create_vm(&req)?;
            remember_label(id, label)?;
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

fn build_rows() -> [Option<Row>; ROWS_MAX] {
    let (entries, _) = manager::list_vms();
    let labels = *LABELS.lock();
    let mut out: [Option<Row>; ROWS_MAX] = [None; ROWS_MAX];
    for (i, entry) in entries.iter().enumerate() {
        if let Some(e) = entry {
            out[i] = Some(Row {
                vm_id: e.id.0,
                label: labels[i],
                state: state_tag(e.state),
                last_exit: e.last_exit,
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
