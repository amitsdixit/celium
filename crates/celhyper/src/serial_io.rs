//! Byte-level COM1 UART driver used by the W22 bridge.
//!
//! Distinct from [`crate::logger`] which is a line-buffered
//! `core::fmt::Write` sink behind a mutex. The bridge needs raw RX
//! and TX with explicit framing, so it gets its own driver that
//! pokes the same UART register file directly. Locking is via the
//! same spin mutex as the logger so log lines and bridge frames
//! never interleave on the wire.

#![cfg(not(test))]

use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

static UART: Mutex<Uart> = Mutex::new(Uart::new(COM1));

struct Uart {
    data: Port<u8>,
    lsr:  Port<u8>,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            lsr:  Port::new(base + 5),
        }
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

/// Block until one byte arrives on COM1 RX.
pub fn read_byte() -> u8 {
    UART.lock().read_byte()
}

/// Block until COM1 TX is ready, then write `b`.
pub fn write_byte(b: u8) {
    UART.lock().write_byte(b);
}

/// Write every byte of `buf` to COM1 TX.
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
