//! The native half of the `bridge-solver` Python package.
//!
//! Deliberately thin: each function takes and returns JSON text from
//! [`bridge_solver::json_api`], and `python/bridge_solver/__init__.py` turns
//! that into keyword arguments and dictionaries. Keeping the Pythonic layer in
//! Python keeps it readable, and keeps this file to one line per operation.
//!
//! Every call releases the GIL while it solves, so Python threads can run
//! solves in parallel.

use bridge_solver::json_api::{self, PlayAnalyzer as Inner};
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

create_exception!(
    _native,
    BridgeSolverError,
    PyValueError,
    "A deal, request or argument bridge-solver could not use."
);

fn err(message: String) -> PyErr {
    BridgeSolverError::new_err(message)
}

/// The library version.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The double-dummy table as JSON.
#[pyfunction]
fn dd_table(py: Python<'_>, deal: &str) -> PyResult<String> {
    py.detach(|| json_api::dd_table(deal)).map_err(err)
}

/// Many tables by the chosen engine, JSON in and out.
#[pyfunction]
fn dd_tables(py: Python<'_>, request_json: &str) -> PyResult<String> {
    py.detach(|| json_api::dd_tables(request_json)).map_err(err)
}

/// Par as JSON.
#[pyfunction]
fn par(py: Python<'_>, deal: &str, vulnerable: &str) -> PyResult<String> {
    py.detach(|| json_api::par(deal, vulnerable)).map_err(err)
}

/// North-South's double-dummy tricks.
#[pyfunction]
fn solve(py: Python<'_>, deal: &str, trump: &str, leader: &str) -> PyResult<u8> {
    py.detach(|| json_api::solve_contract(deal, trump, leader))
        .map_err(err)
}

/// A claim check, JSON in and out.
#[pyfunction]
fn check_claim(py: Python<'_>, request_json: &str) -> PyResult<String> {
    py.detach(|| json_api::check_claim(request_json))
        .map_err(err)
}

/// The claim benchmark's workloads and presets as JSON.
#[pyfunction]
fn claim_bench_presets() -> String {
    json_api::claim_bench_presets()
}

/// Hand the solver's pooled memory on this thread back to the allocator.
#[pyfunction]
fn release_memory() {
    bridge_solver::drain_pool();
}

/// Play analysis with a position cache.
#[pyclass(module = "bridge_solver._native")]
struct PlayAnalyzer {
    inner: Inner,
}

#[pymethods]
impl PlayAnalyzer {
    #[new]
    fn new() -> Self {
        PlayAnalyzer {
            inner: Inner::new(),
        }
    }

    #[getter]
    fn cached_positions(&self) -> usize {
        self.inner.cached_positions()
    }

    fn clear_cache(&mut self) {
        self.inner.clear_cache();
    }

    fn play(&mut self, py: Python<'_>, request_json: &str) -> PyResult<String> {
        let inner = &mut self.inner;
        py.detach(|| inner.dd_play(request_json)).map_err(err)
    }

    fn play_node(&self, py: Python<'_>, request_json: &str, node: usize) -> PyResult<String> {
        let inner = &self.inner;
        py.detach(|| inner.dd_play_node(request_json, node))
            .map_err(err)
    }

    fn optimal_line(&self, py: Python<'_>, request_json: &str, start: usize) -> PyResult<String> {
        let inner = &self.inner;
        py.detach(|| inner.dd_optimal_line(request_json, start))
            .map_err(err)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("BridgeSolverError", m.py().get_type::<BridgeSolverError>())?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(dd_table, m)?)?;
    m.add_function(wrap_pyfunction!(dd_tables, m)?)?;
    m.add_function(wrap_pyfunction!(par, m)?)?;
    m.add_function(wrap_pyfunction!(solve, m)?)?;
    m.add_function(wrap_pyfunction!(check_claim, m)?)?;
    m.add_function(wrap_pyfunction!(claim_bench_presets, m)?)?;
    m.add_function(wrap_pyfunction!(release_memory, m)?)?;
    m.add_class::<PlayAnalyzer>()?;
    Ok(())
}
