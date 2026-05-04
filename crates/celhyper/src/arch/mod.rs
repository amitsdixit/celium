//! Architecture-specific helpers. Currently x86_64 only.

pub mod x86;

pub use x86 as cpu;
