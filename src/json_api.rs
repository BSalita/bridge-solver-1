//! The engine's operations as JSON in, JSON out.
//!
//! One definition of every request and response, shared by each binding that
//! takes the engine outside Rust: the WebAssembly package, the C library
//! (`ffi/`) and the Python package (`python/`). Keeping it here means the three
//! cannot drift apart, and a client can move between them without reshaping
//! its data. The shapes also match the HTTP service's.
//!
//! Every function returns `Err` with a message a person can act on, never
//! panics on bad input, and never prints.
//!
//! Seats are `N`, `E`, `S`, `W`; strains `C`, `D`, `H`, `S`, `N` or `NT`; cards
//! a suit letter then a rank, like `SA` or `HT`. A deal is PBN,
//! `"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"`.

use std::collections::HashMap;

use bridge_types::{Deal, Direction, Strain};
use serde::{Deserialize, Serialize};

use crate::analyse_play::{self, PlayInput};
use crate::claim::{ClaimPosition, ClaimVerdict, DEFAULT_CLAIM_BUDGET};
use crate::claim_bench;
use crate::dd_bench;
use crate::{CutoffCache, Hands, PatternCache, Solver};

/// A deal and the play so far: the input to play analysis and to claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Response for [`PlayAnalyzer::dd_play`], matching the service's
/// `DdPlayResponse` minus the server-only timing field.
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

/// Parse a deal, saying which one failed.
fn parse_deal(dealstr: &str) -> Result<Deal, String> {
    Deal::from_pbn(dealstr).ok_or_else(|| format!("could not parse deal \"{dealstr}\""))
}

/// Parse a request into the engine's validated input form.
pub fn parse_input(req: &PlayRequest) -> Result<PlayInput, String> {
    let trump = analyse_play::parse_trump(&req.trump)
        .ok_or_else(|| format!("unknown trump/strain \"{}\"", req.trump))?;
    let declarer = analyse_play::parse_seat(&req.declarer)
        .ok_or_else(|| format!("unknown declarer seat \"{}\"", req.declarer))?;
    let leader = analyse_play::parse_seat(&req.leader)
        .ok_or_else(|| format!("unknown leader seat \"{}\"", req.leader))?;
    let deal = parse_deal(&req.dealstr)?;

    let mut plays = Vec::with_capacity(req.plays.len());
    for p in &req.plays {
        plays.push(analyse_play::parse_card(p).ok_or_else(|| format!("unknown card \"{p}\""))?);
    }

    Ok(PlayInput {
        hands: Hands::from_deal(&deal),
        trump,
        declarer,
        leader,
        plays,
    })
}

/// Serialise a response, or say what could not be.
fn to_json<T: Serialize>(value: &T, what: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("could not serialise the {what}: {e}"))
}

/// Read a request, or say why not.
fn from_json<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, String> {
    serde_json::from_str(json).map_err(|e| format!("could not read the request: {e}"))
}

/// The double-dummy table as numbers: `tricks[seat][strain]`, seats in
/// `N, E, S, W` order and strains `C, D, H, S, NT`, each the tricks that seat
/// takes as declarer in that strain. [`dd_table`] is the same as JSON.
pub fn dd_table_tricks(dealstr: &str) -> Result<[[u8; 5]; 4], String> {
    let deal = parse_deal(dealstr)?;
    let table = crate::par::solve_dd_table(&deal);
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
    Ok(seats.map(|seat| strains.map(|strain| table.tricks(seat, strain))))
}

/// The full 20-cell double-dummy table for a deal, as
/// `{"tricks": [[...5]...4], "total": 13}`: rows `N, E, S, W`, columns
/// `C, D, H, S, NT`, each the tricks that seat takes as declarer in that
/// strain.
pub fn dd_table(dealstr: &str) -> Result<String, String> {
    let tricks = dd_table_tricks(dealstr)?
        .iter()
        .map(|row| row.to_vec())
        .collect();
    let total = Hands::from_deal(&parse_deal(dealstr)?).num_tricks() as u8;
    to_json(&DdTableResponse { tricks, total }, "table")
}

/// The par result, from a [`par`] call.
#[derive(Debug, Serialize)]
struct ParResponse {
    /// Par score from North-South's side: positive when North-South gain.
    score_ns: i32,
    /// Bridge Composer's `OptimumScore`, e.g. `"NS 620"`, `"EW -500"`, `"0"`.
    optimum_score: String,
    /// Bridge Composer's `ParContract`, e.g. `"NS 4S="`; every contract tied
    /// at par, cheapest first, `"; "`-separated. Absent when passed out.
    #[serde(skip_serializing_if = "Option::is_none")]
    par_contract: Option<String>,
}

/// Par for a deal, given who is vulnerable: `None`, `NS`, `EW` or `All`
/// (also `Both`, `-`, `Love`). Returns
/// `{"score_ns": 620, "optimum_score": "NS 620", "par_contract": "NS 4S="}`.
pub fn par(dealstr: &str, vulnerable: &str) -> Result<String, String> {
    let (vul_ns, vul_ew) = match vulnerable.trim().to_ascii_uppercase().as_str() {
        "NONE" | "-" | "LOVE" | "" => (false, false),
        "NS" | "N-S" => (true, false),
        "EW" | "E-W" => (false, true),
        "ALL" | "BOTH" => (true, true),
        _ => {
            return Err(format!(
                "unknown vulnerability \"{vulnerable}\"; use None, NS, EW or All"
            ))
        }
    };
    let deal = parse_deal(dealstr)?;
    let table = crate::par::solve_dd_table(&deal);
    let result = crate::par::par(&table, vul_ns, vul_ew);
    to_json(
        &ParResponse {
            score_ns: result.score_ns,
            optimum_score: result.optimum_score(),
            par_contract: result.par_contract(),
        },
        "par result",
    )
}

/// North-South's double-dummy tricks with `leader` on lead in `trump`. A
/// convenience for callers that want a single number and no trace.
pub fn solve_contract(dealstr: &str, trump: &str, leader: &str) -> Result<u8, String> {
    let deal = parse_deal(dealstr)?;
    let trump_idx = analyse_play::parse_trump(trump)
        .ok_or_else(|| format!("unknown trump/strain \"{trump}\""))?;
    let leader_idx = analyse_play::parse_seat(leader)
        .ok_or_else(|| format!("unknown leader seat \"{leader}\""))?;

    let hands = Hands::from_deal(&deal);
    let mut cutoff = CutoffCache::new(16);
    let mut pattern = PatternCache::new(16);
    let solver = Solver::new(hands, trump_idx, leader_idx);
    Ok(solver.solve_with_caches(&mut cutoff, &mut pattern))
}

/// Double-dummy play analysis with a position cache that lives as long as
/// the analyzer, so stepping through a hand pays for each new position once.
#[derive(Debug, Default)]
pub struct PlayAnalyzer {
    /// Prefix hash -> NS tricks, mirroring the service's persisted cache.
    positions: HashMap<String, u8>,
}

impl PlayAnalyzer {
    /// An analyzer with an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many positions are currently cached.
    pub fn cached_positions(&self) -> usize {
        self.positions.len()
    }

    /// Forget every cached position, and hand the solver's pooled memory back
    /// to the allocator.
    pub fn clear_cache(&mut self) {
        self.positions.clear();
        crate::drain_pool();
    }

    /// The running trace: every played card with its double-dummy cost.
    /// `request_json` is a [`PlayRequest`].
    pub fn dd_play(&mut self, request_json: &str) -> Result<String, String> {
        let req: PlayRequest = from_json(request_json)?;
        let input = parse_input(&req)?;

        let keys = analyse_play::prefix_keys(&req.dealstr, input.trump, input.leader, &input.plays);
        let all_known = keys.iter().all(|k| self.positions.contains_key(k));

        let output = analyse_play::running_trace(&input, &keys, &self.positions)
            .map_err(|e| format!("{e:?}"))?;
        for (key, value) in &output.new_entries {
            self.positions.insert(key.clone(), *value);
        }

        to_json(
            &DdPlayResponse {
                contract_tricks: output.contract_tricks,
                trace: output.trace,
                cached: all_known,
            },
            "trace",
        )
    }

    /// A double-dummy-perfect continuation from card `from` onwards.
    pub fn dd_optimal_line(&self, request_json: &str, from: usize) -> Result<String, String> {
        let req: PlayRequest = from_json(request_json)?;
        let input = parse_input(&req)?;
        let line = analyse_play::optimal_line(&input, from).map_err(|e| format!("{e:?}"))?;
        to_json(&line, "line")
    }

    /// The alternatives at one card of the play, each with its cost. `node`
    /// is a 0-based index into `plays`.
    pub fn dd_play_node(&self, request_json: &str, node: usize) -> Result<String, String> {
        let req: PlayRequest = from_json(request_json)?;
        let input = parse_input(&req)?;
        let analysis =
            analyse_play::node_alternatives(&input, node).map_err(|e| format!("{e:?}"))?;
        to_json(&analysis, "analysis")
    }
}

/// A claim to check: the play so far, and the claim as BBO states it.
#[derive(Debug, Deserialize)]
pub struct ClaimRequest {
    /// The deal and play so far, as for [`PlayAnalyzer::dd_play`].
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

/// Check a claim against every layout of the unseen cards consistent with
/// the play so far, not just the actual one. `request_json` is a
/// [`ClaimRequest`].
///
/// A verdict of `"undecided"` means the node budget ran out, not that the
/// claim fails; treat it as rejected, as BBO does.
pub fn check_claim(request_json: &str) -> Result<String, String> {
    let req: ClaimRequest = from_json(request_json)?;
    let input = parse_input(&req.play)?;
    let claimer = match &req.claimer {
        Some(seat) => analyse_play::parse_seat(seat)
            .ok_or_else(|| format!("unknown claimer seat \"{seat}\""))?,
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
    .map_err(|e| e.to_string())?;
    let budget = req.budget.unwrap_or(DEFAULT_CLAIM_BUDGET);
    let won = pos.tricks_won();

    let response = match req.total_tricks {
        Some(total) => {
            // Tricks already won count towards the claim.
            let result = pos
                .check(total.saturating_sub(won), budget)
                .map_err(|e| e.to_string())?;
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
    to_json(&response, "claim")
}

/// One workload, as [`claim_bench_presets`] describes it.
#[derive(Serialize)]
struct BenchWorkload {
    name: &'static str,
    description: &'static str,
}

/// One preset, as [`claim_bench_presets`] describes it.
#[derive(Serialize)]
struct BenchPreset {
    name: &'static str,
    workloads: &'static [&'static str],
    sizes: &'static [usize],
    count: u64,
}

#[derive(Serialize)]
struct BenchCatalogue {
    version: &'static str,
    budget: u64,
    workloads: Vec<BenchWorkload>,
    presets: Vec<BenchPreset>,
}

/// The claim benchmark's workloads and presets, and the default budget: the
/// same ones `bridge-claim bench` runs.
pub fn claim_bench_presets() -> String {
    let catalogue = BenchCatalogue {
        version: env!("CARGO_PKG_VERSION"),
        budget: DEFAULT_CLAIM_BUDGET,
        workloads: claim_bench::WORKLOADS
            .iter()
            .map(|w| BenchWorkload {
                name: w.name,
                description: w.description,
            })
            .collect(),
        presets: claim_bench::PRESETS
            .iter()
            .map(|p| BenchPreset {
                name: p.name,
                workloads: p.workloads,
                sizes: p.sizes,
                count: p.count,
            })
            .collect(),
    };
    to_json(&catalogue, "catalogue").unwrap_or_else(|_| "{}".to_string())
}

/// One analysis-benchmark preset, as [`dd_bench_presets`] describes it.
#[derive(Serialize)]
struct DdBenchPreset {
    name: &'static str,
    deals: usize,
}

#[derive(Serialize)]
struct DdBenchCatalogue {
    version: &'static str,
    deals: usize,
    presets: Vec<DdBenchPreset>,
}

/// The analysis benchmark's presets and corpus size: the same ones
/// `bridge-solver --bench` runs. Each preset solves the first `deals` deals.
pub fn dd_bench_presets() -> String {
    let catalogue = DdBenchCatalogue {
        version: env!("CARGO_PKG_VERSION"),
        deals: dd_bench::deal_count(),
        presets: dd_bench::PRESETS
            .iter()
            .map(|p| DdBenchPreset {
                name: p.name,
                deals: p.deals,
            })
            .collect(),
    };
    to_json(&catalogue, "catalogue").unwrap_or_else(|_| "{}".to_string())
}

/// Solve benchmark deal `index`'s full table. Returns the twenty trick counts
/// as hex digits (rows `N, E, S, W`, columns `C, D, H, S, NT`) and the nodes
/// searched, as `"a9b8... 1234567"`.
pub fn dd_bench_table(index: usize) -> Result<String, String> {
    let table = dd_bench::bench_table(index).ok_or_else(|| format!("no benchmark deal {index}"))?;
    Ok(format!("{} {}", table.tricks_hex(), table.nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEAL: &str = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";

    #[test]
    fn table_has_four_rows_of_five() {
        let json = dd_table(DEAL).expect("table");
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v["total"], 13);
        let rows = v["tricks"].as_array().expect("rows");
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.as_array().map(Vec::len) == Some(5)));
    }

    #[test]
    fn par_reads_every_spelling_of_vulnerability() {
        let none = par(DEAL, "None").expect("par");
        assert_eq!(par(DEAL, "-").expect("par"), none);
        assert_eq!(par(DEAL, "love").expect("par"), none);
        assert!(par(DEAL, "All").expect("par").contains("score_ns"));
        assert!(par(DEAL, "sideways").is_err());
    }

    #[test]
    fn claims_answer_or_explain() {
        let request = r#"{"dealstr": "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72",
            "trump": "S", "declarer": "N", "leader": "E",
            "plays": ["HA","H5","H3","H6","HK","H9","H7","HJ"],
            "claimer": "E", "total_tricks": 2}"#;
        let v: serde_json::Value =
            serde_json::from_str(&check_claim(request).expect("claim")).expect("json");
        assert_eq!(v["verdict"], "valid");
        assert_eq!(v["tricks_won"], 2);

        let bad = request.replace("\"claimer\": \"E\"", "\"claimer\": \"X\"");
        assert_eq!(
            check_claim(&bad).err().as_deref(),
            Some("unknown claimer seat \"X\"")
        );
    }

    #[test]
    fn bad_deals_are_errors_not_panics() {
        assert!(dd_table("nonsense").is_err());
        assert!(par("N:AKQ", "None").is_err());
        assert!(check_claim("{").is_err());
    }
}
