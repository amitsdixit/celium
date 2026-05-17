//! W22-B-2 — kernel-side decoder/encoder for the CelHyper bridge.
//!
//! Mirrors the host wire shape defined in
//! `celmesh::hyper_host::wire`: tag-driven JSON with `op`/`reply`
//! discriminants and a small closed set of fields. Hand-rolled
//! because pulling `serde` + `serde_json` into a `no_std` kernel
//! brings an allocator we don't have. The decoder is a single-pass
//! recursive-descent scanner over `&[u8]`; the encoder writes into
//! a caller-supplied `&mut [u8]`. No heap allocation anywhere.
//!
//! # Wire shape (W22 v1, extended W23-B)
//!
//! Requests:
//! * `{"op":"create","label":"..."}`         — label ≤ [`LABEL_MAX`]
//!   Create accepts these forward-compatible *optional* fields, all
//!   `#[serde(default)]` on the host side and `skip_value`-tolerated
//!   by legacy decoders:
//!   * `"image_path":"..."`         — host path ≤ [`IMAGE_PATH_MAX`]
//!   * `"cpu_count":N`              — 1…=64
//!   * `"memory_mib":N`             — 1…=1048576
//!   * `"boot_blob_crc32c":N`       — host-staged blob CRC
//! * `{"op":"start","vm_id":N}`
//! * `{"op":"stop","vm_id":N}`
//! * `{"op":"delete","vm_id":N}`
//! * `{"op":"list"}`
//!
//! Replies:
//! * `{"reply":"created","vm_id":N}`
//! * `{"reply":"state","vm_id":N,"state":"halted","last_exit":12}`
//!   — `last_exit` is omitted when `None`.
//! * `{"reply":"deleted","vm_id":N}`
//! * `{"reply":"listed","rows":[{...},...]}`
//!
//! # Why a hand-rolled JSON parser is acceptable
//!
//! The grammar is closed and tiny: every field name is known at
//! compile time, no field order is required, no escapes appear in any
//! known value (labels are ASCII-only by host-side validation, state
//! tags are static strings, numbers are decimal `u32`). The parser
//! rejects every input it does not recognise with
//! [`HyperError::Invalid`], so a malformed peer can never confuse the
//! kernel; it just gets a connection teardown.

#![allow(clippy::cast_possible_truncation)]

use crate::error::{HyperError, HyperResult};

/// Maximum label length the wire accepts. Must match the host
/// constant `celmesh::hyper_host::wire`'s validation in
/// `LoopbackHyperLink::apply` (`label.len() > 32`).
pub const LABEL_MAX: usize = 32;

/// Maximum host image path length the wire accepts. Sized so a
/// reasonably nested Windows or POSIX path fits without forcing the
/// host to truncate or shorten symlinks. Per-row storage cost in the
/// bridge side-table is `ROWS_MAX * IMAGE_PATH_MAX` bytes — small
/// enough that the buffers live in `.bss` rather than the heap.
pub const IMAGE_PATH_MAX: usize = 128;

/// Maximum number of VM rows in one [`Reply::Listed`].
///
/// Hard-coded rather than re-exported from [`crate::manager::MAX_VMS`]
/// because `manager` is `#[cfg(not(test))]` (it pulls in bare-metal
/// VMCS code) but `wire` must be testable under `cfg(test)`. The two
/// constants are asserted equal in a compile-time check inside the
/// kernel build via `bridge.rs`.
pub const ROWS_MAX: usize = 4;

/// One bridge call decoded off the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// Allocate a slot with `label` and optional configuration.
    Create {
        /// Label bytes. Guaranteed ≤ [`LABEL_MAX`] by the decoder.
        label: &'a [u8],
        /// Host-side image path the controller staged for this VM,
        /// if any. Guaranteed ≤ [`IMAGE_PATH_MAX`] by the decoder.
        /// The kernel does not *load* the image yet — it records the
        /// path so `List` rows can carry it back to the controller
        /// for gossip / drift detection.
        image_path: Option<&'a [u8]>,
        /// Requested vCPU count, if known. Recorded only; the
        /// kernel still runs the canned bring-up template.
        cpu_count: Option<u32>,
        /// Requested guest memory in MiB, if known. Recorded only.
        memory_mib: Option<u64>,
        /// CRC32C of the host-staged boot blob, if known. Recorded
        /// only; used by the controller to detect image drift across
        /// restarts.
        boot_blob_crc32c: Option<u32>,
    },
    /// `vmlaunch` slot `vm_id`.
    Start {
        /// Slot id.
        vm_id: u32,
    },
    /// Force slot `vm_id` to `stopped`.
    Stop {
        /// Slot id.
        vm_id: u32,
    },
    /// Free slot `vm_id` (terminal-only).
    Delete {
        /// Slot id.
        vm_id: u32,
    },
    /// Snapshot every slot.
    List,
}

/// One bridge reply ready to encode.
///
/// Owns its data by value because the kernel computes every field
/// before emitting; no borrowed references survive past `encode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Returned for [`Request::Create`].
    Created {
        /// Newly-assigned slot id.
        vm_id: u32,
    },
    /// Returned for [`Request::Start`] and [`Request::Stop`].
    State {
        /// Slot id whose state changed.
        vm_id: u32,
        /// New state tag.
        state: StateTag,
        /// Guest exit code if known.
        last_exit: Option<u32>,
    },
    /// Returned for [`Request::Delete`].
    Deleted {
        /// Slot id that was freed.
        vm_id: u32,
    },
    /// Returned for [`Request::List`].
    Listed {
        /// Live rows (only `len` are valid).
        rows: [Option<Row>; ROWS_MAX],
    },
}

/// State tag emitted by [`Reply::State`] and inside [`Row`].
///
/// Restricted to the closed set the host wire decoder expects so the
/// kernel can never emit a tag the host can't parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTag {
    /// VM is `Created`.
    Created,
    /// VM is `Running`.
    Running,
    /// VM is `Halted` (terminal).
    Halted,
    /// VM is `Stopped` (terminal).
    Stopped,
    /// VM is `Faulted` (terminal).
    Faulted,
}

impl StateTag {
    /// Static string form used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Halted  => "halted",
            Self::Stopped => "stopped",
            Self::Faulted => "faulted",
        }
    }
}

/// One row inside [`Reply::Listed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Slot id.
    pub vm_id: u32,
    /// Label bytes plus length. The kernel does not store labels —
    /// callers building rows pass an empty slice.
    pub label: LabelBuf,
    /// State tag.
    pub state: StateTag,
    /// Last guest exit code if known.
    pub last_exit: Option<u32>,
    /// Host-side image path recorded at create time, if any.
    pub image_path: ImagePathBuf,
    /// vCPU count recorded at create time, if any.
    pub cpu_count: Option<u32>,
    /// Guest memory in MiB recorded at create time, if any.
    pub memory_mib: Option<u64>,
    /// CRC32C of the host-staged boot blob recorded at create time,
    /// if any.
    pub boot_blob_crc32c: Option<u32>,
}

/// Fixed-size label buffer carried by [`Row`]. Avoids both `alloc`
/// and lifetime entanglement so `Reply` is `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelBuf {
    /// Bytes; only the first `len` are valid.
    pub bytes: [u8; LABEL_MAX],
    /// Valid prefix length.
    pub len: u8,
}

impl LabelBuf {
    /// Empty label.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bytes: [0; LABEL_MAX], len: 0 }
    }

    /// Copy `src` into a fresh buffer. Returns `None` if too long.
    #[must_use]
    pub fn from_slice(src: &[u8]) -> Option<Self> {
        if src.len() > LABEL_MAX {
            return None;
        }
        let mut out = Self::empty();
        out.bytes[..src.len()].copy_from_slice(src);
        out.len = src.len() as u8;
        Some(out)
    }

    /// Valid portion of the buffer.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Fixed-size image-path buffer carried by [`Row`]. Mirrors
/// [`LabelBuf`] but sized for paths rather than labels. An empty
/// buffer (`len == 0`) is the on-the-wire "no image" sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePathBuf {
    /// Bytes; only the first `len` are valid.
    pub bytes: [u8; IMAGE_PATH_MAX],
    /// Valid prefix length. Stored as `u16` because
    /// `IMAGE_PATH_MAX > 255`.
    pub len: u16,
}

impl ImagePathBuf {
    /// Empty path — the "no image staged" value.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bytes: [0; IMAGE_PATH_MAX], len: 0 }
    }

    /// Copy `src` into a fresh buffer. Returns `None` if too long.
    #[must_use]
    pub fn from_slice(src: &[u8]) -> Option<Self> {
        if src.len() > IMAGE_PATH_MAX {
            return None;
        }
        let mut out = Self::empty();
        out.bytes[..src.len()].copy_from_slice(src);
        out.len = src.len() as u16;
        Some(out)
    }

    /// Valid portion of the buffer.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// `true` if the buffer carries no path (the wire sentinel).
    #[must_use]
    pub const fn is_empty(&self) -> bool { self.len == 0 }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Parse one NDJSON request frame.
///
/// `frame` must be a complete JSON object; the caller has already
/// stripped the trailing newline (if any). Unknown fields are
/// tolerated and skipped — forward-compatible with W22-v2 senders
/// that add optional fields.
pub fn decode_request(frame: &[u8]) -> HyperResult<Request<'_>> {
    let mut s = Scanner::new(frame);
    s.expect(b'{')?;
    let mut op: Option<&[u8]> = None;
    let mut label: Option<&[u8]> = None;
    let mut vm_id: Option<u32> = None;
    let mut image_path: Option<&[u8]> = None;
    let mut cpu_count: Option<u32> = None;
    let mut memory_mib: Option<u64> = None;
    let mut boot_blob_crc32c: Option<u32> = None;
    loop {
        s.skip_ws();
        if s.peek() == Some(b'}') {
            s.bump();
            break;
        }
        let key = s.string()?;
        s.skip_ws();
        s.expect(b':')?;
        match key {
            b"op" => {
                s.skip_ws();
                op = Some(s.string()?);
            }
            b"label" => {
                s.skip_ws();
                label = Some(s.string()?);
            }
            b"vm_id" => {
                s.skip_ws();
                vm_id = Some(s.u32_value()?);
            }
            b"image_path" => {
                s.skip_ws();
                // Allow JSON `null` as the "no image" value so the
                // host doesn't have to omit the field conditionally.
                if s.try_consume_null() {
                    image_path = None;
                } else {
                    image_path = Some(s.string()?);
                }
            }
            b"cpu_count" => {
                s.skip_ws();
                if s.try_consume_null() {
                    cpu_count = None;
                } else {
                    cpu_count = Some(s.u32_value()?);
                }
            }
            b"memory_mib" => {
                s.skip_ws();
                if s.try_consume_null() {
                    memory_mib = None;
                } else {
                    memory_mib = Some(s.u64_value()?);
                }
            }
            b"boot_blob_crc32c" => {
                s.skip_ws();
                if s.try_consume_null() {
                    boot_blob_crc32c = None;
                } else {
                    boot_blob_crc32c = Some(s.u32_value()?);
                }
            }
            _ => s.skip_value()?, // forward-compat
        }
        s.skip_ws();
        if s.peek() == Some(b',') {
            s.bump();
            continue;
        }
        s.expect(b'}')?;
        break;
    }
    s.skip_ws();
    if !s.is_eof() {
        return Err(HyperError::Invalid("wire: trailing bytes after request"));
    }

    let op = op.ok_or(HyperError::Invalid("wire: request missing op"))?;
    match op {
        b"create" => {
            let lbl = label.ok_or(HyperError::Invalid("wire: create missing label"))?;
            if lbl.len() > LABEL_MAX {
                return Err(HyperError::Invalid("wire: label > 32 chars"));
            }
            if let Some(p) = image_path {
                if p.len() > IMAGE_PATH_MAX {
                    return Err(HyperError::Invalid("wire: image_path > 128 chars"));
                }
            }
            Ok(Request::Create {
                label: lbl,
                image_path,
                cpu_count,
                memory_mib,
                boot_blob_crc32c,
            })
        }
        b"start" => Ok(Request::Start {
            vm_id: vm_id.ok_or(HyperError::Invalid("wire: start missing vm_id"))?,
        }),
        b"stop" => Ok(Request::Stop {
            vm_id: vm_id.ok_or(HyperError::Invalid("wire: stop missing vm_id"))?,
        }),
        b"delete" => Ok(Request::Delete {
            vm_id: vm_id.ok_or(HyperError::Invalid("wire: delete missing vm_id"))?,
        }),
        b"list" => Ok(Request::List),
        _ => Err(HyperError::Invalid("wire: unknown op")),
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Encode `reply` into `out` as a single NDJSON line *including* the
/// trailing `\n`. Returns the number of bytes written, or
/// [`HyperError::Exhausted`] if `out` is too small.
pub fn encode_reply(reply: &Reply, out: &mut [u8]) -> HyperResult<usize> {
    let mut w = Writer::new(out);
    match reply {
        Reply::Created { vm_id } => {
            w.put_str(br#"{"reply":"created","vm_id":"#)?;
            w.put_u32(*vm_id)?;
            w.put_u8(b'}')?;
        }
        Reply::State { vm_id, state, last_exit } => {
            w.put_str(br#"{"reply":"state","vm_id":"#)?;
            w.put_u32(*vm_id)?;
            w.put_str(br#","state":""#)?;
            w.put_str(state.as_str().as_bytes())?;
            w.put_u8(b'"')?;
            if let Some(exit) = last_exit {
                w.put_str(br#","last_exit":"#)?;
                w.put_u32(*exit)?;
            }
            w.put_u8(b'}')?;
        }
        Reply::Deleted { vm_id } => {
            w.put_str(br#"{"reply":"deleted","vm_id":"#)?;
            w.put_u32(*vm_id)?;
            w.put_u8(b'}')?;
        }
        Reply::Listed { rows } => {
            w.put_str(br#"{"reply":"listed","rows":["#)?;
            let mut first = true;
            for row in rows.iter().flatten() {
                if !first {
                    w.put_u8(b',')?;
                }
                first = false;
                w.put_str(br#"{"vm_id":"#)?;
                w.put_u32(row.vm_id)?;
                w.put_str(br#","label":""#)?;
                w.put_str(row.label.as_slice())?;
                w.put_str(br#"","state":""#)?;
                w.put_str(row.state.as_str().as_bytes())?;
                w.put_u8(b'"')?;
                if let Some(exit) = row.last_exit {
                    w.put_str(br#","last_exit":"#)?;
                    w.put_u32(exit)?;
                }
                if !row.image_path.is_empty() {
                    w.put_str(br#","image_path":""#)?;
                    w.put_str(row.image_path.as_slice())?;
                    w.put_u8(b'"')?;
                }
                if let Some(c) = row.cpu_count {
                    w.put_str(br#","cpu_count":"#)?;
                    w.put_u32(c)?;
                }
                if let Some(m) = row.memory_mib {
                    w.put_str(br#","memory_mib":"#)?;
                    w.put_u64(m)?;
                }
                if let Some(crc) = row.boot_blob_crc32c {
                    w.put_str(br#","boot_blob_crc32c":"#)?;
                    w.put_u32(crc)?;
                }
                w.put_u8(b'}')?;
            }
            w.put_str(b"]}")?;
        }
    }
    w.put_u8(b'\n')?;
    Ok(w.pos)
}

// ---------------------------------------------------------------------------
// Tiny JSON scanner — accepts only the closed grammar above.
// ---------------------------------------------------------------------------

struct Scanner<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(buf: &'a [u8]) -> Self { Self { buf, pos: 0 } }

    fn peek(&self) -> Option<u8> { self.buf.get(self.pos).copied() }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn is_eof(&self) -> bool { self.pos >= self.buf.len() }

    fn expect(&mut self, want: u8) -> HyperResult<()> {
        match self.bump() {
            Some(b) if b == want => Ok(()),
            _ => Err(HyperError::Invalid("wire: unexpected byte")),
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> HyperResult<&'a [u8]> {
        self.expect(b'"')?;
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b'"' {
                let s = &self.buf[start..self.pos];
                self.pos += 1;
                return Ok(s);
            }
            if b == b'\\' {
                return Err(HyperError::Invalid("wire: escapes not allowed"));
            }
            self.pos += 1;
        }
        Err(HyperError::Invalid("wire: unterminated string"))
    }

    fn u32_value(&mut self) -> HyperResult<u32> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(HyperError::Invalid("wire: expected number"));
        }
        let mut acc: u32 = 0;
        for &b in &self.buf[start..self.pos] {
            acc = acc
                .checked_mul(10)
                .and_then(|v| v.checked_add(u32::from(b - b'0')))
                .ok_or(HyperError::Invalid("wire: u32 overflow"))?;
        }
        Ok(acc)
    }

    fn u64_value(&mut self) -> HyperResult<u64> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(HyperError::Invalid("wire: expected number"));
        }
        let mut acc: u64 = 0;
        for &b in &self.buf[start..self.pos] {
            acc = acc
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(b - b'0')))
                .ok_or(HyperError::Invalid("wire: u64 overflow"))?;
        }
        Ok(acc)
    }

    /// Consume a literal `null` if it appears at the current
    /// position. Used so optional fields can be explicitly nulled
    /// over the wire (`"cpu_count":null`) without forcing the
    /// encoder to omit them entirely.
    fn try_consume_null(&mut self) -> bool {
        if self.buf.get(self.pos..self.pos + 4) == Some(b"null") {
            self.pos += 4;
            true
        } else {
            false
        }
    }

    /// Skip a JSON value of any type. Only used by the forward-compat
    /// "unknown field" path: nested objects/arrays are walked with
    /// brace/bracket counting; strings respect quoting; numbers and
    /// bare literals (true/false/null) are consumed until a delimiter.
    fn skip_value(&mut self) -> HyperResult<()> {
        self.skip_ws();
        match self.peek().ok_or(HyperError::Invalid("wire: eof in value"))? {
            b'"' => { let _ = self.string()?; Ok(()) }
            b'{' | b'[' => {
                let open = self.peek().unwrap();
                let close = if open == b'{' { b'}' } else { b']' };
                self.bump();
                let mut depth = 1u32;
                while depth > 0 {
                    self.skip_ws();
                    match self.peek().ok_or(HyperError::Invalid("wire: eof"))? {
                        b'"' => { let _ = self.string()?; }
                        b if b == open  => { depth += 1; self.bump(); }
                        b if b == close => { depth -= 1; self.bump(); }
                        _ => { self.bump(); }
                    }
                }
                Ok(())
            }
            _ => {
                while let Some(b) = self.peek() {
                    if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.bump();
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tiny byte writer over a borrowed slice.
// ---------------------------------------------------------------------------

struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Self { Self { out, pos: 0 } }

    fn put_u8(&mut self, b: u8) -> HyperResult<()> {
        if self.pos >= self.out.len() {
            return Err(HyperError::Exhausted("wire: reply buffer too small"));
        }
        self.out[self.pos] = b;
        self.pos += 1;
        Ok(())
    }

    fn put_str(&mut self, s: &[u8]) -> HyperResult<()> {
        if self.pos + s.len() > self.out.len() {
            return Err(HyperError::Exhausted("wire: reply buffer too small"));
        }
        self.out[self.pos..self.pos + s.len()].copy_from_slice(s);
        self.pos += s.len();
        Ok(())
    }

    fn put_u32(&mut self, v: u32) -> HyperResult<()> {
        let mut buf = [0u8; 10];
        let mut i = buf.len();
        let mut n = v;
        if n == 0 {
            i -= 1;
            buf[i] = b'0';
        } else {
            while n > 0 {
                i -= 1;
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        self.put_str(&buf[i..])
    }

    fn put_u64(&mut self, v: u64) -> HyperResult<()> {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        let mut n = v;
        if n == 0 {
            i -= 1;
            buf[i] = b'0';
        } else {
            while n > 0 {
                i -= 1;
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        self.put_str(&buf[i..])
    }
}

// ---------------------------------------------------------------------------
// Tests (host-side, std permitted)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_create_round_trip() {
        let r = decode_request(br#"{"op":"create","label":"hello"}"#).unwrap();
        match r {
            Request::Create { label, image_path, cpu_count, memory_mib, boot_blob_crc32c } => {
                assert_eq!(label, b"hello");
                assert!(image_path.is_none());
                assert!(cpu_count.is_none());
                assert!(memory_mib.is_none());
                assert!(boot_blob_crc32c.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_create_with_full_config_payload() {
        let frame = br#"{"op":"create","label":"vm","image_path":"/img/disk.raw","cpu_count":2,"memory_mib":512,"boot_blob_crc32c":3735928559}"#;
        let r = decode_request(frame).unwrap();
        match r {
            Request::Create { label, image_path, cpu_count, memory_mib, boot_blob_crc32c } => {
                assert_eq!(label, b"vm");
                assert_eq!(image_path.unwrap(), b"/img/disk.raw");
                assert_eq!(cpu_count, Some(2));
                assert_eq!(memory_mib, Some(512));
                assert_eq!(boot_blob_crc32c, Some(3_735_928_559));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_create_accepts_explicit_null_for_optionals() {
        let frame = br#"{"op":"create","label":"vm","image_path":null,"cpu_count":null,"memory_mib":null,"boot_blob_crc32c":null}"#;
        let r = decode_request(frame).unwrap();
        match r {
            Request::Create { label, image_path, cpu_count, memory_mib, boot_blob_crc32c } => {
                assert_eq!(label, b"vm");
                assert!(image_path.is_none() && cpu_count.is_none() && memory_mib.is_none() && boot_blob_crc32c.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_create_rejects_oversized_image_path() {
        let big = "x".repeat(IMAGE_PATH_MAX + 1);
        let frame = format!(r#"{{"op":"create","label":"vm","image_path":"{big}"}}"#);
        let err = decode_request(frame.as_bytes()).unwrap_err();
        assert!(matches!(err, HyperError::Invalid(_)));
    }

    #[test]
    fn decode_create_with_unknown_field_is_forward_compat() {
        // W22-v2 might add fields; v1 kernels must tolerate them.
        let frame = br#"{"op":"create","label":"x","future":{"a":1}}"#;
        let r = decode_request(frame).unwrap();
        assert!(matches!(r, Request::Create { label, .. } if label == b"x"));
    }

    #[test]
    fn decode_start_stop_delete_list() {
        assert!(matches!(
            decode_request(br#"{"op":"start","vm_id":2}"#).unwrap(),
            Request::Start { vm_id: 2 }
        ));
        assert!(matches!(
            decode_request(br#"{"op":"stop","vm_id":3}"#).unwrap(),
            Request::Stop { vm_id: 3 }
        ));
        assert!(matches!(
            decode_request(br#"{"op":"delete","vm_id":0}"#).unwrap(),
            Request::Delete { vm_id: 0 }
        ));
        assert!(matches!(
            decode_request(br#"{"op":"list"}"#).unwrap(),
            Request::List
        ));
    }

    #[test]
    fn decode_rejects_oversized_label() {
        let big = "x".repeat(33);
        let frame = format!(r#"{{"op":"create","label":"{big}"}}"#);
        assert!(decode_request(frame.as_bytes()).is_err());
    }

    #[test]
    fn decode_rejects_unknown_op() {
        assert!(decode_request(br#"{"op":"nope"}"#).is_err());
    }

    #[test]
    fn decode_rejects_escapes_in_string() {
        assert!(decode_request(br#"{"op":"create","label":"a\nb"}"#).is_err());
    }

    #[test]
    fn encode_state_with_last_exit() {
        let mut buf = [0u8; 128];
        let n = encode_reply(
            &Reply::State {
                vm_id: 0,
                state: StateTag::Halted,
                last_exit: Some(12),
            },
            &mut buf,
        )
        .unwrap();
        let line = &buf[..n];
        assert_eq!(
            line,
            br#"{"reply":"state","vm_id":0,"state":"halted","last_exit":12}
"#
        );
    }

    #[test]
    fn encode_state_without_last_exit() {
        let mut buf = [0u8; 128];
        let n = encode_reply(
            &Reply::State {
                vm_id: 0,
                state: StateTag::Created,
                last_exit: None,
            },
            &mut buf,
        )
        .unwrap();
        let line = &buf[..n];
        assert_eq!(
            line,
            br#"{"reply":"state","vm_id":0,"state":"created"}
"#
        );
    }

    #[test]
    fn encode_listed_emits_rows_array() {
        let mut rows: [Option<Row>; ROWS_MAX] = [None; ROWS_MAX];
        rows[0] = Some(Row {
            vm_id: 0,
            label: LabelBuf::from_slice(b"first").unwrap(),
            state: StateTag::Created,
            last_exit: None,
            image_path: ImagePathBuf::empty(),
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        });
        rows[1] = Some(Row {
            vm_id: 1,
            label: LabelBuf::from_slice(b"").unwrap(),
            state: StateTag::Halted,
            last_exit: Some(12),
            image_path: ImagePathBuf::empty(),
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        });
        let mut buf = [0u8; 256];
        let n = encode_reply(&Reply::Listed { rows }, &mut buf).unwrap();
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(line.starts_with(r#"{"reply":"listed","rows":["#));
        assert!(line.contains(r#"{"vm_id":0,"label":"first","state":"created"}"#));
        assert!(line.contains(r#"{"vm_id":1,"label":"","state":"halted","last_exit":12}"#));
        assert!(line.ends_with("]}\n"));
    }

    #[test]
    fn encode_listed_includes_image_metadata_when_present() {
        let mut rows: [Option<Row>; ROWS_MAX] = [None; ROWS_MAX];
        rows[0] = Some(Row {
            vm_id: 0,
            label: LabelBuf::from_slice(b"vm").unwrap(),
            state: StateTag::Created,
            last_exit: None,
            image_path: ImagePathBuf::from_slice(b"/img/disk.raw").unwrap(),
            cpu_count: Some(2),
            memory_mib: Some(512),
            boot_blob_crc32c: Some(0xDEAD_BEEF),
        });
        let mut buf = [0u8; 512];
        let n = encode_reply(&Reply::Listed { rows }, &mut buf).unwrap();
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(line.contains(r#""image_path":"/img/disk.raw""#), "line={line}");
        assert!(line.contains(r#""cpu_count":2"#), "line={line}");
        assert!(line.contains(r#""memory_mib":512"#), "line={line}");
        assert!(line.contains(r#""boot_blob_crc32c":3735928559"#), "line={line}");
    }

    #[test]
    fn encode_reports_buffer_too_small() {
        let mut buf = [0u8; 4];
        let err = encode_reply(&Reply::Created { vm_id: 0 }, &mut buf).unwrap_err();
        assert!(matches!(err, HyperError::Exhausted(_)));
    }

    /// Byte-for-byte compatibility with the host serde_json encoder.
    /// Locks the wire shape between celhyper (kernel) and celmesh (host).
    #[test]
    fn kernel_encoder_matches_host_serde_byte_for_byte() {
        // Mimic the host serde encoding for each reply variant and
        // compare to what the kernel writes. The host encoding is
        // exercised by celmesh::hyper_host::wire's serde derives;
        // we don't pull celmesh in here (it's a different workspace
        // crate), so we re-derive the expected strings inline from
        // the rules `serde_json` follows: compact, no spaces, in
        // declaration order, `skip_serializing_if = Option::is_none`
        // omits None fields.
        let cases: &[(Reply, &str)] = &[
            (
                Reply::Created { vm_id: 0 },
                r#"{"reply":"created","vm_id":0}"#,
            ),
            (
                Reply::State { vm_id: 0, state: StateTag::Halted, last_exit: Some(12) },
                r#"{"reply":"state","vm_id":0,"state":"halted","last_exit":12}"#,
            ),
            (
                Reply::State { vm_id: 2, state: StateTag::Stopped, last_exit: None },
                r#"{"reply":"state","vm_id":2,"state":"stopped"}"#,
            ),
            (
                Reply::Deleted { vm_id: 3 },
                r#"{"reply":"deleted","vm_id":3}"#,
            ),
        ];
        for (reply, expected) in cases {
            let mut buf = [0u8; 256];
            let n = encode_reply(reply, &mut buf).unwrap();
            let line = core::str::from_utf8(&buf[..n - 1]).unwrap(); // strip \n
            assert_eq!(line, *expected, "encoding mismatch for {reply:?}");
        }
    }
}
