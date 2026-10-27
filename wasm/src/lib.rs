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
//!
//! Request and response JSON match the service's, so a client can swap
//! transports without reshaping its data.
//!
//! # Caching
//!
//! The service persists position values in SQLite; there is no database here, so
//! [`Analyzer`] keeps the same prefix-keyed map in memory for its lifetime. Hold
//! one instance while stepping through a hand and each new position costs one
//! solve, with everything before it a hit — the same incremental behaviour, just
//! scoped to the page instead of shared between users.

use std::collections::HashMap;

use bridge_solver::analyse_play::{self, PlayInput};
use bridge_solver::{
    ClaimPosition, ClaimVerdict, CutoffCache, Hands, PatternCache, Solver, DEFAULT_CLAIM_BUDGET,
};
use bridge_types::Deal;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

pub mod lin_input;

/// Turn Rust panics into readable console messages rather than `unreachable`.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A play-analysis request, matching the service's `PlayRequest`.
///
/// Serialisable as well as deserialisable so [`lin_input`] can hand a caller a
/// request it can feed straight back to [`Analyzer::dd_play`].
#[derive(Debug, Serialize, Deserialize)]
pub struct PlayRequest {
    /// The deal in PBN, e.g. `"N:J98.QT83.K6.J853 ..."`.
    pub dealstr: String,
    /// Trump/strain: `S|H|D|C|N|NT`.
    pub trump: String,
    /// Declaring seat: `N|E|S|W`.
    pub declarer: String,
    /// Opening leader (declarer's LHO): `N|E|S|W`.
    pub leader: String,
    /// Play trace, e.g. `["HK","H3",...]`; may be partial.
    pub plays: Vec<String>,
}

/// Response for `dd_play`, matching the service's `DdPlayResponse` minus the
/// server-only timing field.
#[derive(Debug, Serialize)]
struct DdPlayResponse {
    /// `V_0` — the contract's double-dummy result from the opening lead.
    contract_tricks: u8,
    trace: Vec<analyse_play::TraceEntry>,
    /// Whether every position was already known, so nothing had to be solved.
    cached: bool,
}

/// The 20-cell double-dummy table, as `tricks[seat][strain]`.
#[derive(Debug, Serialize)]
struct DdTableResponse {
    /// Rows in `N, E, S, W` order; columns in `C, D, H, S, NT` order.
    tricks: Vec<Vec<u8>>,
    /// Total tricks in the deal (13 for a full deal).
    total: u8,
}

/// Parse a request into the engine's validated input form.
fn parse_input(req: &PlayRequest) -> Result<PlayInput, String> {
    let trump = analyse_play::parse_trump(&req.trump)
        .ok_or_else(|| format!("unknown trump/strain \"{}\"", req.trump))?;
    let declarer = analyse_play::parse_seat(&req.declarer)
        .ok_or_else(|| format!("unknown declarer seat \"{}\"", req.declarer))?;
    let leader = analyse_play::parse_seat(&req.leader)
        .ok_or_else(|| format!("unknown leader seat \"{}\"", req.leader))?;

    let deal = Deal::from_pbn(&req.dealstr)
        .ok_or_else(|| format!("could not parse deal \"{}\"", req.dealstr))?;

    let mut plays = Vec::with_capacity(req.plays.len());
    for p in &req.plays {
        plays.push(analyse_play::parse_card(p).ok_or_else(|| format!("unknown card \"{}\"", p))?);
    }

    Ok(PlayInput {
        hands: Hands::from_deal(&deal),
        trump,
        declarer,
        leader,
        plays,
    })
}

/// Double-dummy analysis with a session-lived position cache.
#[wasm_bindgen]
#[derive(Default)]
pub struct Analyzer {
    /// Prefix hash -> NS tricks, mirroring the service's persisted cache.
    positions: HashMap<String, u8>,
}

#[wasm_bindgen]
impl Analyzer {
    /// Create an analyzer with an empty cache.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Analyzer {
        Analyzer {
            positions: HashMap::new(),
        }
    }

    /// How many positions are currently cached.
    #[wasm_bindgen(getter)]
    pub fn cached_positions(&self) -> usize {
        self.positions.len()
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
        self.positions.clear();
        bridge_solver::drain_pool();
    }

    /// Solve the full 20-cell double-dummy table for a deal.
    ///
    /// `dealstr` is PBN, e.g. `"N:J98.QT83.K6.J853 Q762..."`. Returns
    /// `{ tricks, total }` as JSON, rows `N,E,S,W` and columns `C,D,H,S,NT`.
    pub fn dd_table(&self, dealstr: &str) -> Result<String, JsError> {
        let deal = Deal::from_pbn(dealstr)
            .ok_or_else(|| JsError::new(&format!("could not parse deal \"{}\"", dealstr)))?;

        let table = bridge_solver::par::solve_dd_table(&deal);
        let total = Hands::from_deal(&deal).num_tricks() as u8;

        // Flatten in the order the client expects rather than exposing the
        // engine's internal indexing.
        use bridge_types::{Direction, Strain};
        let seats = [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ];
        let strains = [
            Strain::Clubs,
            Strain::Diamonds,
            Strain::Hearts,
            Strain::Spades,
            Strain::NoTrump,
        ];
        let tricks = seats
            .iter()
            .map(|&seat| strains.iter().map(|&st| table.tricks(seat, st)).collect())
            .collect();

        serde_json::to_string(&DdTableResponse { tricks, total })
            .map_err(|e| JsError::new(&format!("could not serialise the table: {}", e)))
    }

    /// Tier 1: the running trace — every played card with its double-dummy cost.
    ///
    /// This is what tags each error card. `request_json` matches the service's
    /// `POST /dd/play` body. Positions solved here are remembered, so stepping
    /// forward through a hand only pays for the newly reached position.
    pub fn dd_play(&mut self, request_json: &str) -> Result<String, JsError> {
        let req: PlayRequest = serde_json::from_str(request_json)
            .map_err(|e| JsError::new(&format!("could not read the request: {}", e)))?;

        let input = parse_input(&req).map_err(|e| JsError::new(&e))?;

        let keys = analyse_play::prefix_keys(&req.dealstr, input.trump, input.leader, &input.plays);
        let all_known = keys.iter().all(|k| self.positions.contains_key(k));

        let output = analyse_play::running_trace(&input, &keys, &self.positions)
            .map_err(|e| JsError::new(&format!("{:?}", e)))?;

        // Fold newly solved positions back in so the next call is cheaper.
        for (key, value) in &output.new_entries {
            self.positions.insert(key.clone(), *value);
        }

        serde_json::to_string(&DdPlayResponse {
            contract_tricks: output.contract_tricks,
            trace: output.trace,
            cached: all_known,
        })
        .map_err(|e| JsError::new(&format!("could not serialise the trace: {}", e)))
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
        let req: PlayRequest = serde_json::from_str(request_json)
            .map_err(|e| JsError::new(&format!("could not read the request: {}", e)))?;

        let input = parse_input(&req).map_err(|e| JsError::new(&e))?;

        let line = analyse_play::optimal_line(&input, from)
            .map_err(|e| JsError::new(&format!("{:?}", e)))?;

        serde_json::to_string(&line)
            .map_err(|e| JsError::new(&format!("could not serialise the line: {}", e)))
    }

    /// Tier 2: the alternatives at one decision node, with each card's cost.
    ///
    /// This is what a click on a tagged card shows. `node` is a 0-based index
    /// into `plays`.
    pub fn dd_play_node(&self, request_json: &str, node: usize) -> Result<String, JsError> {
        let req: PlayRequest = serde_json::from_str(request_json)
            .map_err(|e| JsError::new(&format!("could not read the request: {}", e)))?;

        let input = parse_input(&req).map_err(|e| JsError::new(&e))?;

        let analysis = analyse_play::node_alternatives(&input, node)
            .map_err(|e| JsError::new(&format!("{:?}", e)))?;

        serde_json::to_string(&analysis)
            .map_err(|e| JsError::new(&format!("could not serialise the analysis: {}", e)))
    }
}

/// Turn a LIN string or a BBO handviewer URL into an analysable request.
///
/// Accepts either form — a URL is recognised by its `lin=` parameter — and
/// returns JSON carrying a `request` ready for [`Analyzer::dd_play`] alongside
/// the contract, seat names, auction and claim a UI wants to display. Nothing
/// is fetched: a URL is decoded locally, so a shortened link must be expanded
/// before it gets here.
/// Hand the solver's pooled pattern-tree memory back to the allocator.
///
/// Available without an [`Analyzer`], because the pool is per-thread and global
/// to the module: a caller that only ever asked for `dd_table` still has one.
/// Worth calling after a hard deal, or on a page-visibility change.
#[wasm_bindgen]
pub fn release_memory() {
    bridge_solver::drain_pool();
}

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

/// A claim to check: the play so far, and the claim as BBO states it.
#[derive(Debug, Deserialize)]
pub struct ClaimRequest {
    /// The deal and play so far, as for [`Analyzer::dd_play`].
    #[serde(flatten)]
    pub play: PlayRequest,
    /// The claiming seat, `N|E|S|W`; defaults to declarer. A defender's claim
    /// may not rely on partner's unseen cards.
    pub claimer: Option<String>,
    /// The claiming side's total for the whole hand, counting tricks already
    /// won. For a claim by declarer this is the number BBO shows and LIN
    /// records in `mc`. Omit it to ask for the largest claim the claimer could
    /// make instead.
    pub total_tricks: Option<usize>,
    /// Search node budget; defaults to [`DEFAULT_CLAIM_BUDGET`].
    pub budget: Option<u64>,
}

/// The answer to a [`ClaimRequest`]. Trick counts are the claiming side's,
/// and totals for the hand.
#[derive(Debug, Serialize)]
struct ClaimResponse {
    /// `"valid"`, `"invalid"` or `"undecided"` for the claimed total; absent
    /// when no total was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<&'static str>,
    /// The largest total proven guaranteed; present when no total was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    guaranteed: Option<usize>,
    /// With `guaranteed`: whether the budget ran out before one more trick
    /// could be ruled out.
    #[serde(skip_serializing_if = "Option::is_none")]
    undecided: Option<bool>,
    /// Tricks the claiming side has already won.
    tricks_won: usize,
    /// Tricks still to play, counting one in progress.
    tricks_left: usize,
    /// Search nodes visited.
    nodes: u64,
}

/// Check a claim against every layout of the defenders' cards consistent with
/// the play so far, not just the actual one.
///
/// Unlike the double-dummy calls, a finesse or a favourable split is not
/// assumed: the claim is valid only if the claimer has a line that works
/// however the unseen cards lie. A verdict of `"undecided"` means the node budget ran
/// out, not that the claim fails.
#[wasm_bindgen]
pub fn check_claim(request_json: &str) -> Result<String, JsError> {
    let req: ClaimRequest = serde_json::from_str(request_json)
        .map_err(|e| JsError::new(&format!("could not read the request: {}", e)))?;
    let input = parse_input(&req.play).map_err(|e| JsError::new(&e))?;
    let claimer = match &req.claimer {
        Some(seat) => analyse_play::parse_seat(seat)
            .ok_or_else(|| JsError::new(&format!("unknown claimer seat \"{}\"", seat)))?,
        None => input.declarer,
    };
    let pos = ClaimPosition::from_play(
        &input.hands,
        input.trump,
        input.declarer,
        claimer,
        input.leader,
        &input.plays,
    )
    .map_err(|e| JsError::new(&e.to_string()))?;
    let budget = req.budget.unwrap_or(DEFAULT_CLAIM_BUDGET);
    let won = pos.tricks_won();

    let response = match req.total_tricks {
        Some(total) => {
            // Tricks already won count towards the claim.
            let result = pos
                .check(total.saturating_sub(won), budget)
                .map_err(|e| JsError::new(&e.to_string()))?;
            ClaimResponse {
                verdict: Some(match result.verdict {
                    ClaimVerdict::Valid => "valid",
                    ClaimVerdict::Invalid => "invalid",
                    ClaimVerdict::Undecided => "undecided",
                }),
                guaranteed: None,
                undecided: None,
                tricks_won: won,
                tricks_left: pos.tricks_left(),
                nodes: result.nodes,
            }
        }
        None => {
            let limit = pos.max_claim(budget);
            ClaimResponse {
                verdict: None,
                guaranteed: Some(won + limit.guaranteed),
                undecided: Some(limit.undecided),
                tricks_won: won,
                tricks_left: pos.tricks_left(),
                nodes: limit.nodes,
            }
        }
    };
    serde_json::to_string(&response)
        .map_err(|e| JsError::new(&format!("could not serialise the claim: {}", e)))
}

/// Solve one position: how many tricks the declaring side takes from the lead.
///
/// A convenience for callers that want a single number and no play trace.
#[wasm_bindgen]
pub fn solve_contract(dealstr: &str, trump: &str, leader: &str) -> Result<u8, JsError> {
    let deal = Deal::from_pbn(dealstr)
        .ok_or_else(|| JsError::new(&format!("could not parse deal \"{}\"", dealstr)))?;
    let trump_idx = analyse_play::parse_trump(trump)
        .ok_or_else(|| JsError::new(&format!("unknown trump/strain \"{}\"", trump)))?;
    let leader_idx = analyse_play::parse_seat(leader)
        .ok_or_else(|| JsError::new(&format!("unknown leader seat \"{}\"", leader)))?;

    let hands = Hands::from_deal(&deal);
    let mut cutoff = CutoffCache::new(16);
    let mut pattern = PatternCache::new(16);
    let solver = Solver::new(hands, trump_idx, leader_idx);

    Ok(solver.solve_with_caches(&mut cutoff, &mut pattern))
}
