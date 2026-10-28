//! `solver-bench engines`: this solver against DDS-compatible libraries.
//!
//! Every engine solves the same deals as one batch, at each thread cap asked
//! for, and every one of the twenty cells of every table is checked against
//! this solver's. The report has the shape of ddss's `dtest --verify` engine
//! summary, with this solver as the baseline.
//!
//! The libraries are loaded at run time by path (`--dds-lib`, repeatable), so
//! upstream DDS 2.9 and the ddss fork can be measured side by side in one
//! process, on one corpus, by one timer. A library with ddss's
//! `CalcAllTablesPBNx` gets the whole batch in one call; plain DDS gets it
//! forty deals at a time, the most its `CalcAllTablesPBN` accepts.

use std::path::{Path, PathBuf};
use std::time::Instant;

use bridge_solver::dds_engine::{self, Engine, Tricks};
use clap::Args;
use serde::Serialize;

/// Options for `solver-bench engines`.
#[derive(Args, Clone)]
pub struct EnginesArgs {
    /// A DDS-compatible library to compare (dds.dll, libdds.so, ddss). May be
    /// repeated. With none, `BRIDGE_SOLVER_DDS_LIB` (or `DDSS_DLL_PATH`) is
    /// used.
    #[arg(long = "dds-lib", value_name = "PATH")]
    dds_libs: Vec<PathBuf>,

    /// Deals to solve: a file of PBN deal strings, one per line. Without it,
    /// the first `--deals` of the 200-deal lock-step corpus.
    #[arg(long, value_name = "FILE")]
    pbn: Option<PathBuf>,

    /// How many deals of the built-in corpus to use (at most 200).
    #[arg(long, default_value_t = 200)]
    deals: usize,

    /// Thread caps to measure, comma-separated; 0 lets each engine use every
    /// core. Every engine gets the same cap.
    #[arg(long, value_delimiter = ',', default_values_t = [0usize, 1])]
    threads: Vec<usize>,

    /// Repeats per measurement; the fastest is reported.
    #[arg(long, default_value_t = 1)]
    runs: usize,

    /// Also write the report as JSON.
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,
}

#[derive(Serialize)]
struct Row {
    engine: String,
    path: Option<String>,
    threads: usize,
    time_ms: f64,
    tables_per_s: f64,
    /// Baseline time over this engine's: above 1 is faster than bridge-solver.
    speed_vs_bridge_solver: f64,
    mismatched_cells: Option<usize>,
}

#[derive(Serialize)]
struct Pair {
    a: String,
    b: String,
    mismatched_cells: usize,
}

#[derive(Serialize)]
struct CapReport {
    thread_cap: usize,
    rows: Vec<Row>,
    pairwise: Vec<Pair>,
}

#[derive(Serialize)]
struct Report {
    deals: usize,
    cells_per_engine: usize,
    cores: usize,
    caps: Vec<CapReport>,
    pass: bool,
}

fn load_deals(args: &EnginesArgs) -> Result<Vec<String>, String> {
    if let Some(path) = &args.pbn {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let deals: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if deals.is_empty() {
            return Err(format!("no deals in {}", path.display()));
        }
        return Ok(deals);
    }
    let n = args.deals.min(bridge_solver::dd_bench::deal_count());
    Ok((0..n)
        .filter_map(bridge_solver::dd_bench::deal_pbn)
        .map(str::to_string)
        .collect())
}

fn mismatches(a: &[Tricks], b: &[Tricks]) -> usize {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            x.iter()
                .flatten()
                .zip(y.iter().flatten())
                .filter(|(p, q)| p != q)
                .count()
        })
        .sum()
}

/// Run `solver-bench engines`.
pub fn run(args: &EnginesArgs) -> Result<(), String> {
    let deals = load_deals(args)?;
    let pbns: Vec<&str> = deals.iter().map(String::as_str).collect();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());

    let mut libs: Vec<PathBuf> = args.dds_libs.clone();
    if libs.is_empty() {
        libs.extend(dds_engine::default_dds_lib());
    }
    let mut engines = vec![Engine::Native];
    for path in &libs {
        engines.push(Engine::from_name("dds", Some(Path::new(path)))?);
    }
    if engines.len() == 1 {
        eprintln!(
            "note: no DDS library given (--dds-lib or {}); measuring bridge-solver alone",
            dds_engine::DDS_LIB_ENV
        );
    }
    for engine in &engines {
        if let Engine::Dds(lib) = engine {
            let info = lib.info();
            println!(
                "{:<22} {}  ({} cores seen, batch API: {})",
                engine.label(),
                lib.path().display(),
                info.cores,
                if info.batch_api {
                    "CalcAllTablesPBNx"
                } else {
                    "CalcAllTablesPBN x40"
                }
            );
        }
    }

    // Warm every engine: library initialisation, thread pools, first caches.
    for engine in &engines {
        engine.solve_tables(&pbns[..pbns.len().min(4)], 0)?;
    }

    let mut caps = Vec::new();
    let mut pass = true;
    for &cap in &args.threads {
        println!(
            "\nThread cap {} ...",
            if cap == 0 {
                "auto".into()
            } else {
                cap.to_string()
            }
        );
        let mut results: Vec<Vec<Tricks>> = Vec::new();
        let mut times = Vec::new();
        for engine in &engines {
            let mut best = f64::INFINITY;
            let mut tables = Vec::new();
            for _ in 0..args.runs.max(1) {
                let start = Instant::now();
                tables = engine.solve_tables(&pbns, cap)?;
                best = best.min(start.elapsed().as_secs_f64());
            }
            println!(
                "  {} solved {} deals in {:.1} ms ({:.1} tables/s)",
                engine.label(),
                pbns.len(),
                best * 1000.0,
                pbns.len() as f64 / best
            );
            results.push(tables);
            times.push(best);
        }

        let base = &results[0];
        let rows: Vec<Row> = engines
            .iter()
            .enumerate()
            .map(|(i, engine)| {
                let threads = match engine {
                    Engine::Native if cap == 0 => cores,
                    Engine::Native => cap,
                    Engine::Dds(lib) => usize::try_from(lib.info().threads).unwrap_or(0),
                };
                Row {
                    engine: engine.label(),
                    path: match engine {
                        Engine::Dds(lib) => Some(lib.path().display().to_string()),
                        Engine::Native => None,
                    },
                    threads,
                    time_ms: times[i] * 1000.0,
                    tables_per_s: pbns.len() as f64 / times[i],
                    speed_vs_bridge_solver: times[0] / times[i],
                    mismatched_cells: (i > 0).then(|| mismatches(&results[i], base)),
                }
            })
            .collect();
        let mut pairwise = Vec::new();
        for i in 1..engines.len() {
            for j in i + 1..engines.len() {
                pairwise.push(Pair {
                    a: engines[i].label(),
                    b: engines[j].label(),
                    mismatched_cells: mismatches(&results[i], &results[j]),
                });
            }
        }
        let ok = rows.iter().all(|r| r.mismatched_cells.unwrap_or(0) == 0)
            && pairwise.iter().all(|p| p.mismatched_cells == 0);
        pass &= ok;
        print_summary(pbns.len(), cap, &rows, &pairwise, ok);
        caps.push(CapReport {
            thread_cap: cap,
            rows,
            pairwise,
        });
    }

    if let Some(path) = &args.json {
        let report = Report {
            deals: pbns.len(),
            cells_per_engine: pbns.len() * 20,
            cores,
            caps,
            pass,
        };
        let text = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("serialising the report: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        println!("\nwrote {}", path.display());
    }
    if pass {
        Ok(())
    } else {
        Err("engines disagree: see the mismatched cells above".into())
    }
}

fn print_summary(deals: usize, cap: usize, rows: &[Row], pairwise: &[Pair], ok: bool) {
    println!("\n=== Engine Comparison Summary ===");
    println!(
        "Deals: {deals}   Cells per engine: {}   Thread cap: {}\n",
        deals * 20,
        if cap == 0 {
            "0 (auto)".into()
        } else {
            cap.to_string()
        }
    );
    println!(
        "{:<24}{:>8}{:>12}{:>11}  {:>22}  {:>27}",
        "Engine",
        "Threads",
        "Time (ms)",
        "Tables/s",
        "Speed vs bridge-solver",
        "Mismatches vs bridge-solver"
    );
    for r in rows {
        let m = r
            .mismatched_cells
            .map_or_else(|| "-".to_string(), |m| m.to_string());
        println!(
            "{:<24}{:>8}{:>12.1}{:>11.1}  {:>21.2}x  {:>27}",
            r.engine, r.threads, r.time_ms, r.tables_per_s, r.speed_vs_bridge_solver, m
        );
    }
    if !pairwise.is_empty() {
        println!("\nPairwise checks:");
        for p in pairwise {
            println!(
                "  {} vs {}: {} mismatched cells",
                p.a, p.b, p.mismatched_cells
            );
        }
    }
    println!(
        "\nAll engines agree on all {} cells: {}",
        deals * 20,
        if ok { "PASS" } else { "FAIL" }
    );
    println!("=================================");
}
