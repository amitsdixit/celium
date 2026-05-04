//! Minimal serial logger over COM1 (0x3F8). Used until we wire up `tracing`
//! over a proper transport in Week-3.
//!
//! Write-only; no init/handshake on the UART beyond enabling FIFOs and
//! disabling interrupts. Calls are spinlocked so log lines from different
//! cores do not interleave.

use core::fmt::Write;
use spin::Mutex;
use x86_64::instructions::port::Port;

/// COM1 base.
const COM1: u16 = 0x3F8;

static SERIAL: Mutex<Serial> = Mutex::new(Serial::new(COM1));

struct Serial {
    data:  Port<u8>,
    lsr:   Port<u8>,
    initialised: bool,
}

impl Serial {
    const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            lsr:  Port::new(base + 5),
            initialised: false,
        }
    }

    fn init(&mut self) {
        if self.initialised {
            return;
        }
        // Standard 38400 8N1 init sequence on COM1.
        // SAFETY: writing to the legacy UART I/O ports has no effect outside
        // the device itself and is permitted in long mode at CPL 0.
        unsafe {
            Port::<u8>::new(COM1 + 1).write(0x00); // disable IRQs
            Port::<u8>::new(COM1 + 3).write(0x80); // enable DLAB
            Port::<u8>::new(COM1 + 0).write(0x03); // divisor lo (38400)
            Port::<u8>::new(COM1 + 1).write(0x00); // divisor hi
            Port::<u8>::new(COM1 + 3).write(0x03); // 8N1, DLAB off
            Port::<u8>::new(COM1 + 2).write(0xC7); // FIFO on, clear, 14-byte
            Port::<u8>::new(COM1 + 4).write(0x0B); // RTS/DSR/OUT2 set
        }
        self.initialised = true;
    }

    fn write_byte(&mut self, b: u8) {
        // Spin until THR empty.
        // SAFETY: legacy UART read; harmless.
        while unsafe { self.lsr.read() } & 0x20 == 0 {}
        // SAFETY: legacy UART write; harmless.
        unsafe { self.data.write(b) }
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}

/// Initialise COM1. Idempotent; safe to call on every CPU.
pub fn init_serial() {
    SERIAL.lock().init();
}

/// Log a single line.
pub fn log(line: &str) {
    let mut s = SERIAL.lock();
    let _ = writeln!(s, "{line}");
}

/// Log `key=value` (value as hex). Avoids pulling in fmt machinery for u64.
pub fn log_kv(key: &str, value: u64) {
    let mut s = SERIAL.lock();
    let _ = writeln!(s, "{key}={value:#x}");
}
