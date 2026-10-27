//! WebAssembly bindings for the double-dummy engine.
//!
//! Exposes the HTTP service's three operations, plus claim checking, so a browser can do
//! its own analysis instead of round-tripping every hand to a server:
//!
//! | here | service route | engine |
//! |---|---|---|
//! | [`Analyzer::dd_table`] | `POST /dd` | [`bridge_solver::par::solve_dd_table`] |
//! | [`Analyzer::dd_play`] | `POST /dd/play` | [`bridge_solver::analyse_play::running_trace`] |
//! | [`Analyzer::dd_play_node`] | `POST /dd/play/node` | [`bridge_solver::analyse_play::node_alternatives`] |
//! | [`check_claim`] | — | [`bridge_solver::ClaimPosition`] |
//! | [`ClaimBenchCase`], [`claim_bench_presets`] | — | [`bridge_solver::claim_bench`] |
//! | [`dd_bench_table`], [`dd_bench_presets`] | — | [`bridge_solver::dd_bench`] |
//!
//! Request and response JSON match the service's, so a client can swap
//! transports without reshaping its data. Both are defined once, in
//! [`bridge_solver::json_api`], which the C library and the Python package
//! wrap too; this crate only adapts its errors to JavaScript.
//!
//! # Caching
//!
//! The service persists position values in SQLite; there is no database here, so
//! [`Analyzer`] keeps the same prefix-keyed map in memory for its lifetime. Hold
//! one instance while stepping through a hand and each new position costs one
//! solve, with everything before it a hit — the same incremental behaviour, just
//! scoped to the page instead of shared between users.

use bridge_solver::claim_bench::{self, BenchClaim};
use bridge_solver::json_api::{self, PlayAnalyzer};
use serde::Serialize;
use wasm_bindgen::prelude::*;

pub use bridge_solver::json_api::PlayRequest;

pub mod lin_input;

/// Turn Rust panics into readable console messages rather than `unreachable`.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A shared-module error, as a JavaScript exception.
fn js(e: String) -> JsError {
    JsError::new(&e)
}

/// Double-dummy analysis with a session-lived position cache.
#[wasm_bindgen]
#[derive(Default)]
pub struct Analyzer {
    inner: PlayAnalyzer,
}

#[wasm_bindgen]
impl Analyzer {
    /// Create an analyzer with an empty cache.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Analyzer {
        Analyzer::default()
    }

    /// How many positions are currently cached.
    #[wasm_bindgen(getter)]
    pub fn cached_positions(&self) -> usize {
        self.inner.cached_positions()
    }

    /// Forget every cached position, and hand the solver's pooled memory back
    /// to the allocator.
    ///
    /// The two are separate stores and only one of them is obvious. Beyond this
    /// position cache, the solver keeps a free list of pattern-tree blocks that
    /// it recycles rather than freeing, which is right for a process solving
    /// deal after deal and wrong for a browser tab. A freakish distribution --
    /// voids in several hands -- can build a pattern tree of fifteen million
    /// nodes and over a gigabyte of blocks, and without this that peak is held
    /// for as long as the page is open, on a heap that is capped at four
    /// gigabytes and in practice cut off well below it.
    pub fn clear_cache(&mut self) {
        self.inner.clear_cache();
    }

    /// Solve the full 20-cell double-dummy table for a deal.
    ///
    /// `dealstr` is PBN, e.g. `"N:J98.QT83.K6.J853 Q762..."`. Returns
    /// `{ tricks, total }` as JSON, rows `N,E,S,W` and columns `C,D,H,S,NT`.
    pub fn dd_table(&self, dealstr: &str) -> Result<String, JsError> {
        json_api::dd_table(dealstr).map_err(js)
    }

    /// Tier 1: the running trace — every played card with its double-dummy cost.
    ///
    /// This is what tags each error card. `request_json` matches the service's
    /// `POST /dd/play` body. Positions solved here are remembered, so stepping
    /// forward through a hand only pays for the newly reached position.
    pub fn dd_play(&mut self, request_json: &str) -> Result<String, JsError> {
        self.inner.dd_play(request_json).map_err(js)
    }

    /// A double-dummy-perfect continuation from one point in the hand.
    ///
    /// What should have happened from `from` onwards, with both sides playing
    /// optimally. Started at the first costed error, it is the correction for it.
    ///
    /// Done in one call rather than by walking `dd_play_node` forward: the work is
    /// the same shape, but the caches live for the whole playout instead of being
    /// rebuilt per position, and one call replaces forty round trips.
    pub fn dd_optimal_line(&self, request_json: &str, from: usize) -> Result<String, JsError> {
        self.inner.dd_optimal_line(request_json, from).map_err(js)
    }

    /// Tier 2: the alternatives at one decision node, with each card's cost.
    ///
    /// This is what a click on a tagged card shows. `node` is a 0-based index
    /// into `plays`.
    pub fn dd_play_node(&self, request_json: &str, node: usize) -> Result<String, JsError> {
        self.inner.dd_play_node(request_json, node).map_err(js)
    }
}

/// Hand the solver's pooled pattern-tree memory back to the allocator.
///
/// Available without an [`Analyzer`], because the pool is per-thread and global
/// to the module: a caller that only ever asked for `dd_table` still has one.
/// Worth calling after a hard deal, or on a page-visibility change.
#[wasm_bindgen]
pub fn release_memory() {
    bridge_solver::drain_pool();
}

/// Turn a LIN string or a BBO handviewer URL into an analysable request.
///
/// Accepts either form — a URL is recognised by its `lin=` parameter — and
/// returns JSON carrying a `request` ready for [`Analyzer::dd_play`] alongside
/// the contract, seat names, auction and claim a UI wants to display. Nothing
/// is fetched: a URL is decoded locally, so a shortened link must be expanded
/// before it gets here.
#[wasm_bindgen]
pub fn parse_lin(input: &str) -> Result<String, JsError> {
    let parsed = lin_input::parse(input).map_err(|e| JsError::new(&e))?;
    serde_json::to_string(&parsed)
        .map_err(|e| JsError::new(&format!("could not serialise the parsed LIN: {}", e)))
}

/// Parse a multi-board LIN file, one board per line.
///
/// Returns a JSON array with one entry per board, each either
/// `{ "ok": <parsed board> }` or `{ "error": "<why>" }`, so one unanalysable
/// board — a passed-out auction, say — does not cost the caller the rest of the
/// file.
#[wasm_bindgen]
pub fn parse_lin_file(content: &str) -> Result<String, JsError> {
    #[derive(Serialize)]
    struct Entry {
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<lin_input::LinInput>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }

    let boards = lin_input::parse_file(content).map_err(|e| JsError::new(&e))?;
    let entries: Vec<Entry> = boards
        .into_iter()
        .map(|b| match b {
            Ok(parsed) => Entry {
                ok: Some(parsed),
                error: None,
            },
            Err(e) => Entry {
                ok: None,
                error: Some(e),
            },
        })
        .collect();

    serde_json::to_string(&entries)
        .map_err(|e| JsError::new(&format!("could not serialise the parsed LIN file: {}", e)))
}

/// Check a claim against every layout of the defenders' cards consistent with
/// the play so far, not just the actual one. See
/// [`bridge_solver::json_api::check_claim`] for the request and response.
///
/// Unlike the double-dummy calls, a finesse or a favourable split is not
/// assumed: the claim is valid only if the claimer has a line that works
/// however the unseen cards lie. A verdict of `"undecided"` means the node
/// budget ran out, not that the claim fails.
#[wasm_bindgen]
pub fn check_claim(request_json: &str) -> Result<String, JsError> {
    json_api::check_claim(request_json).map_err(js)
}

/// The claim benchmark's workloads and presets, and the default budget, as
/// JSON: the same ones the native `bridge-claim bench` runs.
#[wasm_bindgen]
pub fn claim_bench_presets() -> String {
    json_api::claim_bench_presets()
}

/// One benchmark claim, generated and ready to check.
///
/// Generating and checking are separate calls so that a page can time the
/// check alone: generating a claim runs a double-dummy solve, and for some
/// workloads several, which is not the claim checker's work.
#[wasm_bindgen]
pub struct ClaimBenchCase {
    claim: BenchClaim,
}

#[wasm_bindgen]
impl ClaimBenchCase {
    /// Claim number `index` of `workload` with `size` cards per hand.
    ///
    /// `index` and, in [`Self::check`], `budget` are `u32` rather than the
    /// library's `u64` so JavaScript passes plain numbers, not `BigInt`s.
    #[wasm_bindgen(constructor)]
    pub fn new(workload: &str, size: usize, index: u32) -> Result<ClaimBenchCase, JsError> {
        let w = claim_bench::workload(workload)
            .ok_or_else(|| JsError::new(&format!("no workload \"{}\"", workload)))?;
        let claim = claim_bench::bench_claim(w, size, u64::from(index))
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(ClaimBenchCase { claim })
    }

    /// Check the claim within `budget` nodes. Returns the verdict letter
    /// (`V`, `X` or `?`) and the nodes searched, as `"V 1234"`.
    pub fn check(&self, budget: u32) -> Result<String, JsError> {
        let result = self
            .claim
            .position
            .check(self.claim.tricks, u64::from(budget))
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(format!(
            "{} {}",
            claim_bench::verdict_letter(result.verdict),
            result.nodes
        ))
    }
}

/// The analysis benchmark's presets, as JSON: the same ones the native
/// `bridge-solver --bench` runs.
#[wasm_bindgen]
pub fn dd_bench_presets() -> String {
    json_api::dd_bench_presets()
}

/// Solve analysis-benchmark deal `index`'s full double-dummy table. Returns
/// the tricks as twenty hex digits and the nodes searched, as
/// `"a9b8... 1234567"`.
#[wasm_bindgen]
pub fn dd_bench_table(index: u32) -> Result<String, JsError> {
    json_api::dd_bench_table(index as usize).map_err(js)
}

/// Solve one position: North-South's double-dummy tricks with `leader` on
/// lead in `trump`.
///
/// A convenience for callers that want a single number and no play trace.
#[wasm_bindgen]
pub fn solve_contract(dealstr: &str, trump: &str, leader: &str) -> Result<u8, JsError> {
    json_api::solve_contract(dealstr, trump, leader).map_err(js)
}
