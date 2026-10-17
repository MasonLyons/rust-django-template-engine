//! Error types and Python exception mapping for the DTL engine.
//!
//! Class lookups go through `py.import` every time: after the first import this
//! is a `sys.modules` dict hit, and it avoids stashing `Py<PyAny>` across GIL
//! acquisitions.

use pyo3::prelude::*;
use pyo3::types::PyTuple;

pub fn syntax_error_type(py: Python<'_>) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("django.template.exceptions")?
        .getattr("TemplateSyntaxError")?
        .unbind())
}

pub fn var_does_not_exist_type(py: Python<'_>) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("django.template.base")?
        .getattr("VariableDoesNotExist")?
        .unbind())
}

pub fn template_does_not_exist_type(py: Python<'_>) -> PyResult<Py<PyAny>> {
    static TDE: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
    Ok(TDE
        .get_or_try_init(py, || -> PyResult<_> {
            Ok(py
                .import("django.template.exceptions")?
                .getattr("TemplateDoesNotExist")?
                .unbind())
        })?
        .clone_ref(py))
}

pub fn unsupported_error_type(py: Python<'_>) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("django_rust_templates.exceptions")?
        .getattr("UnsupportedTemplateError")?
        .unbind())
}

/// Engine error kinds; mapped to Python exceptions at the boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Django-parity template syntax error; message is verbatim Django wording.
    #[error("{0}")]
    Syntax(String),
    /// A construct the native engine cannot execute exactly.
    #[error("{0}")]
    Unsupported(String),
    /// A Python error to propagate unchanged.
    #[error(transparent)]
    Python(#[from] PyErr),
}

/// Raise a TemplateSyntaxError with exact Django wording.
pub fn syntax_error(py: Python<'_>, message: &str) -> PyErr {
    match syntax_error_type(py) {
        Ok(cls) => match cls.bind(py).call1((message,)) {
            Ok(inst) => PyErr::from_value(inst),
            Err(e) => e,
        },
        Err(e) => e,
    }
}
/// Raise UnsupportedTemplateError.
pub fn unsupported_error(py: Python<'_>, message: &str) -> PyErr {
    match unsupported_error_type(py) {
        Ok(cls) => match cls.bind(py).call1((message,)) {
            Ok(inst) => PyErr::from_value(inst),
            Err(e) => e,
        },
        Err(e) => e,
    }
}

/// Build `VariableDoesNotExist(msg, params)` as a PyErr whose `str()` matches
/// Django's `"Failed lookup for key [%s] in %r" % params` formatting.
pub fn variable_does_not_exist(
    py: Python<'_>,
    msg: &str,
    params: &Bound<'_, PyTuple>,
) -> PyErr {
    match var_does_not_exist_type(py) {
        Ok(cls) => match cls.bind(py).call1((msg, params)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(e) => e,
        },
        Err(e) => e,
    }
}

impl From<Error> for PyErr {
    fn from(e: Error) -> PyErr {
        match e {
            Error::Python(err) => err,
            other => Python::attach(|py| match other {
                Error::Syntax(msg) => syntax_error(py, &msg),
                Error::Unsupported(msg) => unsupported_error(py, &msg),
                Error::Python(err) => err,
            }),
        }
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(i: std::convert::Infallible) -> Self {
        match i {}
    }
}
