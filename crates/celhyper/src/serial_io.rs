//! Byte-level COM2 UART driver used by the W22 bridge.
//!
//! Distinct from [`crate::logger`] which is a line-buffered
//! `core::fmt::Write` sink on COM1. The bridge needs raw RX/TX with
//! explicit NDJSON framing **and** must not intermix with kernel log
//! lines, so it lives on a *separate* UART (COM2, port `0x2F8`).
//! That lets the QEMU integration script route the two serials at
//! the host: `-serial file:com1.log -serial tcp:host:port,server`
//! and have a host-side `SerialHyperLink` drive the bridge cleanly.

#![cfg(not(test))]

use spin::Mutex;
use x86_64::instructions::port::Port;

/// Base I/O port for the bridge UART (COM2).
const COM2: u16 = 0x2F8;

static UART: Mutex<Uart> = Mutex::new(Uart::new(COM2));

struct Uart {
    base: u16,
    data: Port<u8>,
    lsr:  Port<u8>,
    initialised: bool,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self {
            base,
            data: Port::new(base),
            lsr:  Port::new(base + 5),
            initialised: false,
        }
    }

    fn init(&mut self) {
        if self.initialised {
            return;
        }
        // Standard 38400 8N1 setup. Mirrors logger::Serial::init.
        // SAFETY: legacy UART I/O ports are harmless at CPL 0.
        unsafe {
            Port::<u8>::new(self.base + 1).write(0x00); // disable IRQs
            Port::<u8>::new(self.base + 3).write(0x80); // enable DLAB
            Port::<u8>::new(self.base + 0).write(0x03); // divisor lo
            Port::<u8>::new(self.base + 1).write(0x00); // divisor hi
            Port::<u8>::new(self.base + 3).write(0x03); // 8N1, DLAB off
            Port::<u8>::new(self.base + 2).write(0xC7); // FIFO on, clear
            Port::<u8>::new(self.base + 4).write(0x0B); // RTS/DSR/OUT2
        }
        self.initialised = true;
    }

    fn rx_ready(&mut self) -> bool {
        // SAFETY: legacy UART read; harmless and side-effect free.
        unsafe { self.lsr.read() & 0x01 != 0 }
    }

    fn tx_ready(&mut self) -> bool {
        // SAFETY: legacy UART read; harmless.
        unsafe { self.lsr.read() & 0x20 != 0 }
    }

    fn read_byte(&mut self) -> u8 {
        while !self.rx_ready() {
            core::hint::spin_loop();
        }
        // SAFETY: legacy UART read; harmless.
        unsafe { self.data.read() }
    }

    fn write_byte(&mut self, b: u8) {
        while !self.tx_ready() {
            core::hint::spin_loop();
        }
        // SAFETY: legacy UART write; harmless to legacy I/O port.
        unsafe { self.data.write(b) }
    }
}

/// Initialise the bridge UART (COM2). Idempotent; call before any
/// [`read_byte`] / [`write_byte`] / [`write_all`] / [`read_line`].
pub fn init() {
    UART.lock().init();
}

/// Block until one byte arrives on the bridge UART RX.
pub fn read_byte() -> u8 {
    UART.lock().read_byte()
}

/// Block until the bridge UART TX is ready, then write `b`.
pub fn write_byte(b: u8) {
    UART.lock().write_byte(b);
}

/// Write every byte of `buf` to the bridge UART TX.
pub fn write_all(buf: &[u8]) {
    let mut u = UART.lock();
    for &b in buf {
        u.write_byte(b);
    }
}

/// Read bytes until a `\n` is observed (LF dropped from the result).
/// Returns the number of bytes stored in `out`, or
/// [`crate::error::HyperError::Exhausted`] if the line exceeds the
/// buffer (the offending bytes are still drained off the UART so the
/// next call resyncs to the following frame).
pub fn read_line(out: &mut [u8]) -> crate::HyperResult<usize> {
    let mut n = 0usize;
    let mut overflow = false;
    loop {
        let b = read_byte();
        if b == b'\n' {
            if overflow {
                return Err(crate::error::HyperError::Exhausted(
                    "serial_io: line exceeded buffer",
                ));
            }
            return Ok(n);
        }
        if b == b'\r' {
            continue; // tolerate CRLF
        }
        if n < out.len() {
            out[n] = b;
            n += 1;
        } else {
            overflow = true;
        }
    }
}
