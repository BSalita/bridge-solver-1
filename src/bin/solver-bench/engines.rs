//! `solver-bench engines`: this solver against DDS-compatible libraries and
//! the C++ reference it was ported from.
//!
//! Every engine solves the same deals as one batch, at each thread cap asked
//! for, and every one of the twenty cells of every table is checked against
//! this solver's. The report has the shape of ddss's `dtest --verify` engine
//! summary, with this solver (`bridge-solver-rs`) as the baseline.
//!
//! The libraries are loaded at run time by path (`--dds-lib`, repeatable), so
//! upstream DDS 2.9 and the ddss fork can be measured side by side in one
//! process, on one corpus, by one timer. A library with ddss's
//! `CalcAllTablesPBNx` gets the whole batch in one call; plain DDS gets it
//! forty deals at a time, the most its `CalcAllTablesPBN` accepts.
//!
//! The C++ reference (`--cpp-worker`, reported as `bridge-solver-c++`) is not
//! thread-safe, so a thread cap of N runs it as N worker processes; see
//! `cpp_engine.rs`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use bridge_solver::dds_engine::{self, Engine, Tricks};

use crate::cpp_engine::CppEngine;
use clap::Args;
use serde::Serialize;

/// Options for `solver-bench engines`.
#[derive(Args, Clone)]
pub struct EnginesArgs {
    /// A DDS-compatible library to compare (dds.dll, libdds.so, ddss). May be
    /// repeated. `NAME=PATH` sets its name in the report, e.g. to tell an
    /// MSVC build from a clang build of the same version. With none,
    /// `BRIDGE_SOLVER_DDS_LIB` (or `DDSS_DLL_PATH`) is used.
    #[arg(long = "dds-lib", value_name = "[NAME=]PATH")]
    dds_libs: Vec<String>,

    /// The C++ reference's batch worker (`cpp-worker`, from
    /// bench/comparison/build-cpp-worker.sh), to compare as
    /// `bridge-solver-c++`. Run as one process per thread. May be repeated;
    /// `NAME=PATH` names a build in the report, e.g. to compare variants.
    #[arg(long = "cpp-worker", value_name = "[NAME=]PATH")]
    cpp_workers: Vec<String>,

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

    /// Repeats per measurement; the fastest is reported. Repeats are
    /// interleaved across engines, each round starting with a different one,
    /// so none always runs straight after the heaviest.
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
    /// Baseline time over this engine's: above 1 is faster than
    /// bridge-solver-rs. (The field name predates the -rs suffix.)
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

/// One engine in the comparison.
enum Contender {
    /// This crate's solver, the baseline.
    Rs,
    /// A DDS-compatible library, with the name given on the command line.
    Dds(Engine, Option<String>),
    /// The C++ reference, as worker processes.
    Cpp(CppEngine),
}

impl Contender {
    fn label(&self) -> String {
        match self {
            Contender::Rs => format!("bridge-solver-rs {}", env!("CARGO_PKG_VERSION")),
            Contender::Dds(_, Some(name)) => name.clone(),
            Contender::Dds(engine, None) => engine.label(),
            Contender::Cpp(cpp) => cpp.label(),
        }
    }

    fn solve_tables(&self, pbns: &[&str], cap: usize) -> Result<Vec<Tricks>, String> {
        match self {
            Contender::Rs => Engine::Native.solve_tables(pbns, cap),
            Contender::Dds(engine, _) => engine.solve_tables(pbns, cap),
            Contender::Cpp(cpp) => cpp.solve_tables(pbns, cap),
        }
    }

    /// Threads (or worker processes) used at `cap`.
    fn threads(&self, cap: usize, cores: usize) -> usize {
        match self {
            Contender::Dds(Engine::Dds(lib), _) => usize::try_from(lib.info().threads).unwrap_or(0),
            _ if cap == 0 => cores,
            _ => cap,
        }
    }

    fn path(&self) -> Option<String> {
        match self {
            Contender::Dds(Engine::Dds(lib), _) => Some(lib.path().display().to_string()),
            Contender::Cpp(cpp) => Some(cpp.path().display().to_string()),
            _ => None,
        }
    }
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

/// `NAME=PATH` to its parts; a bare path has no name. Only a `NAME` with no
/// path separator counts, so a path containing `=` still works.
fn split_name(arg: &str) -> (Option<String>, PathBuf) {
    match arg.split_once('=') {
        Some((name, path)) if !name.is_empty() && !name.contains(['/', '\\', ':']) => {
            (Some(name.to_string()), PathBuf::from(path))
        }
        _ => (None, PathBuf::from(arg)),
    }
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

    let mut libs: Vec<(Option<String>, PathBuf)> =
        args.dds_libs.iter().map(|a| split_name(a)).collect();
    if libs.is_empty() {
        libs.extend(dds_engine::default_dds_lib().map(|p| (None, p)));
    }
    let mut engines = vec![Contender::Rs];
    for arg in &args.cpp_workers {
        let (name, path) = split_name(arg);
        engines.push(Contender::Cpp(CppEngine::new(&path, name)?));
    }
    for (name, path) in libs {
        let engine = Engine::from_name("dds", Some(Path::new(&path)))?;
        engines.push(Contender::Dds(engine, name));
    }
    if engines.len() == 1 {
        eprintln!(
            "note: no other engine given (--cpp-worker, --dds-lib or {}); measuring bridge-solver-rs alone",
            dds_engine::DDS_LIB_ENV
        );
    }
    for engine in &engines {
        if let Contender::Cpp(cpp) = engine {
            println!(
                "{:<22} {}  (one process per thread)",
                engine.label(),
                cpp.path().display()
            );
        }
        if let Contender::Dds(Engine::Dds(lib), _) = engine {
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
        // Repeats are interleaved, and each round starts with a different
        // engine. Run engine by engine, whichever came straight after the
        // heaviest paid for it: on a 128-thread Threadripper this solver was
        // 16% slower at 64 threads following DDS 2.9 than running first --
        // most likely the CPU holding lower clocks after a long all-core load.
        let n = engines.len();
        let mut results: Vec<Vec<Tricks>> = vec![Vec::new(); n];
        let mut times = vec![f64::INFINITY; n];
        for round in 0..args.runs.max(1) {
            for k in 0..n {
                let i = (round + k) % n;
                let start = Instant::now();
                results[i] = engines[i].solve_tables(&pbns, cap)?;
                times[i] = times[i].min(start.elapsed().as_secs_f64());
            }
        }
        for (engine, best) in engines.iter().zip(&times) {
            println!(
                "  {} solved {} deals in {:.1} ms ({:.1} tables/s)",
                engine.label(),
                pbns.len(),
                best * 1000.0,
                pbns.len() as f64 / best
            );
        }

        let base = &results[0];
        let rows: Vec<Row> = engines
            .iter()
            .enumerate()
            .map(|(i, engine)| Row {
                engine: engine.label(),
                path: engine.path(),
                threads: engine.threads(cap, cores),
                time_ms: times[i] * 1000.0,
                tables_per_s: pbns.len() as f64 / times[i],
                speed_vs_bridge_solver: times[0] / times[i],
                mismatched_cells: (i > 0).then(|| mismatches(&results[i], base)),
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
        "{:<24}{:>8}{:>12}{:>11}  {:>25}  {:>30}",
        "Engine",
        "Threads",
        "Time (ms)",
        "Tables/s",
        "Speed vs bridge-solver-rs",
        "Mismatches vs bridge-solver-rs"
    );
    for r in rows {
        let m = r
            .mismatched_cells
            .map_or_else(|| "-".to_string(), |m| m.to_string());
        println!(
            "{:<24}{:>8}{:>12.1}{:>11.1}  {:>24.2}x  {:>30}",
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
