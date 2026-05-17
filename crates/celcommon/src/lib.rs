//! Shared types, errors, tracing, and metrics for Celium.
//!
//! Per `00_GLOBAL_CONVENTIONS.md`: every fallible API in Celium must return
//! [`CelResult<T>`] and use the [`CelError`] taxonomy below. No `unwrap`/`panic`
//! in production code.

#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms, clippy::pedantic)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::module_name_repetitions)]

pub mod error;
pub mod ids;

pub use error::{CelError, CelResult};
