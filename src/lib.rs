//! dtl — Django Template Language, native Rust engine.
// GIL-bound engine: every fallible call propagates Python exceptions;
// per-function `# Errors` sections would be pure boilerplate.
#![allow(clippy::missing_errors_doc)]

use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::prelude::*;

pub mod bridge;
pub mod engine;
pub mod error;
pub mod filters;
pub mod ifexpr;
pub mod lexer;
pub mod nodes;
pub mod nodes_py;
pub mod parser;
pub mod render;
pub mod tags;
pub mod varparse;

/// Count of template renders executed by the Rust engine.
/// The Django-suite patch dumps this at exit to prove the engine ran.
pub static RENDER_COUNT: AtomicU64 = AtomicU64::new(0);


#[pyfunction]
fn render_count() -> PyResult<u64> {
    Ok(RENDER_COUNT.load(Ordering::Relaxed))
}

#[pyfunction]
fn _version() -> PyResult<String> {
    Ok(env!("CARGO_PKG_VERSION").to_string())
}

#[pymodule]
#[pyo3(name = "_dtl")]
fn dtl(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_version, m)?)?;
    m.add_function(wrap_pyfunction!(render_count, m)?)?;
    engine::register(m)?;
    Ok(())
}
