//! solver-bench - performance harness for the double-dummy solver.
//!
//! Answers three questions, deliberately kept apart because they move for
//! different reasons and collapsing them into one headline hides regressions:
//!
//! * **Single-threaded cost.** How efficient the search itself is. One core
//!   doing one board's work. This is the stable number — a single-threaded run
//!   gets a full core even on a busy machine, so repeats land within a percent
//!   or two — and it is the one that matters for the browser, where extra
//!   cores may not be available at all.
//! * **Scaling.** How much of the machine we can actually use, measured as a
//!   thread sweep over *equal work per thread*. Never one board per thread:
//!   board difficulty spans an order of magnitude, so that measures load
//!   imbalance and reports a number that looks like scaling and is not.
//! * **Agreement.** Every board carries its double-dummy table, so a run that
//!   is fast and wrong says so.
//!
//! Run on demand, not in CI — a full sweep takes minutes and the numbers are
//! only comparable on an otherwise-idle machine.
//!
//! ```text
//! ./dev-build.sh --ci run --release --features bench --bin solver-bench -- run
//! ./dev-build.sh --ci run --release --features bench --bin solver-bench -- sweep
//! ./dev-build.sh --ci run --release --features bench --bin solver-bench -- verify
//! ./dev-build.sh --ci run --release --features bench --bin solver-bench -- compare A.json B.json
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(feature = "dds-reference")]
mod dds;

use bridge_encodings::pbn::dd_table_to_pbn;
#[cfg(feature = "dds-reference")]
use bridge_solver::{solve_dd_strain, STRAINS};
use bridge_solver::{solve_dd_table, solve_dd_table_cells, solve_dd_table_with_nodes};
use bridge_types::Deal;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// A run whose samples spread wider than this was measuring the machine, not
/// the code. dealer3's harness uses the same threshold for the same reason.
const SPREAD_WARN: f64 = 0.15;

/// Sweep configurations shorter than this are dominated by thread startup and
/// the final join, and the curve that comes out is noise rather than scaling.
const DEFAULT_MIN_SWEEP_SECONDS: f64 = 1.0;

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// One benchmark board: the deal, and the answer it must produce.
#[derive(Debug, Clone, Deserialize)]
struct Board {
    board: usize,
    pbn: String,
    /// Seat-major N,S,E,W over strains NT,S,H,D,C, in hex.
    ddtricks: String,
    contract: String,
}

/// The committed benchmark corpus.
#[derive(Debug, Deserialize)]
struct Corpus {
    version: String,
    boards: Vec<Board>,
}

impl Corpus {
    /// Load the corpus, and parse every deal up front so a malformed board is
    /// a startup error rather than a mid-run panic.
    fn load(path: &Path) -> Result<(Self, Vec<Deal>), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read corpus {}: {e}", path.display()))?;
        let corpus: Corpus =
            serde_json::from_str(&text).map_err(|e| format!("corpus is not valid JSON: {e}"))?;
        if corpus.boards.is_empty() {
            return Err("corpus contains no boards".into());
        }
        let deals = corpus
            .boards
            .iter()
            .map(|b| {
                Deal::from_pbn(&b.pbn)
                    .ok_or_else(|| format!("board {} has an unparseable deal", b.board))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((corpus, deals))
    }
}

/// Encode a solved table the way the corpus pins it.
#[cfg(feature = "dds-reference")]
/// Encode a raw `[direction][strain]` array as the corpus's `ddtricks` string.
///
/// Only for tables this crate did not produce: DDS's own output, and the
/// atomics the parallel assembly check writes. Our own tables are
/// `bridge_types::DdTable` and go through `dd_table_to_pbn`, which owns the
/// orderings — this exists because those two arrive as bare arrays in a layout
/// we do not control.
fn encode_ddtricks(tricks: &[[u8; 5]; 4]) -> String {
    let mut out = String::with_capacity(20);
    // par.rs indexed [direction][strain] as N,E,S,W over C,D,H,S,NT; the
    // corpus is seat-major N,S,E,W over NT,S,H,D,C.
    for dir in [0usize, 2, 1, 3] {
        for strain in [4usize, 3, 2, 1, 0] {
            match std::char::from_digit(tricks[dir][strain] as u32, 16) {
                Some(c) => out.push(c),
                None => out.push('?'),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Wall clock and CPU time for one measured run.
///
/// Both, always. Wall clock alone cannot tell "we used four cores well" from
/// "we used one core faster", and reading a threaded result without the CPU
/// figure beside it is how a parallelism win gets mistaken for a per-core one.
#[derive(Debug, Clone, Copy, Serialize)]
struct Sample {
    wall_ms: f64,
    cpu_ms: f64,
}

impl Sample {
    /// Cores busy: CPU divided by wall. 1.0 is single-threaded, 4.0 means four
    /// cores were saturated for the whole run.
    fn cores_busy(&self) -> f64 {
        if self.wall_ms > 0.0 {
            self.cpu_ms / self.wall_ms
        } else {
            0.0
        }
    }
}

/// CPU time consumed by this process so far, across all its threads.
#[cfg(unix)]
fn cpu_time() -> Duration {
    // SAFETY: `getrusage` writes a plain POD struct through the pointer we
    // give it and reads nothing else. The struct is fully initialised by the
    // zeroed value before the call, and the call cannot fail for RUSAGE_SELF.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let secs = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// CPU time is unavailable off unix; wall clock stands in, and "cores busy"
/// will read 1.0 whatever actually happened.
#[cfg(not(unix))]
fn cpu_time() -> Duration {
    Duration::ZERO
}

/// Instructions retired and CPU cycles consumed by this process so far.
///
/// Wall clock answers "how long did the user wait", which is the question that
/// matters to a user and the one that needs repeats, a quiet machine and a
/// minimum-of-N to ask honestly. Instructions retired answers a different and
/// much cheaper question: *how much work did the program actually do*. It is a
/// property of the executed code, not of the machine's mood, so it does not
/// care about scheduling, frequency, or what else is running -- which makes a
/// single run of it worth more than fifteen wall-clock runs when the question
/// is "did my change remove work".
///
/// The two are not interchangeable, and the gap between them is informative
/// rather than noise. A change that removes instructions but adds cache misses
/// is slower despite counting better; the cycles figure catches that, and
/// cycles-per-instruction says which way a change traded.
///
/// macOS only. `proc_pid_rusage` with `RUSAGE_INFO_V4` needs no entitlement or
/// root. The offsets are checked against `<libproc.h>` on this platform:
/// `sizeof(rusage_info_v4)` is 296, `ri_instructions` at 248, `ri_cycles` at
/// 256, and the struct is read as words rather than transcribed field by field
/// because only those two are wanted and the rest are a long tail of
/// QoS accounting that would be easy to get subtly wrong.
#[cfg(target_os = "macos")]
fn counters() -> Option<(u64, u64)> {
    const RUSAGE_INFO_V4: libc::c_int = 4;
    const WORDS: usize = 296 / 8;
    const INSTRUCTIONS: usize = 248 / 8;
    const CYCLES: usize = 256 / 8;
    extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buf = [0u64; WORDS];
    // SAFETY: `proc_pid_rusage` writes at most `sizeof(rusage_info_v4)` bytes
    // through the pointer, and `buf` is exactly that size and 8-aligned, which
    // is the struct's alignment since every field past the leading uuid is a
    // `uint64_t`. It reads nothing else and cannot retain the pointer.
    let rc = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            RUSAGE_INFO_V4,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc != 0 {
        return None;
    }
    Some((buf[INSTRUCTIONS], buf[CYCLES]))
}

/// No equivalent elsewhere; the wall and CPU figures stand alone.
#[cfg(not(target_os = "macos"))]
fn counters() -> Option<(u64, u64)> {
    None
}

/// Time one closure, wall and CPU together.
fn measure<T>(f: impl FnOnce() -> T) -> (T, Sample) {
    let cpu0 = cpu_time();
    let wall0 = Instant::now();
    let out = f();
    let wall = wall0.elapsed();
    let cpu = cpu_time().saturating_sub(cpu0);
    let cpu_ms = if cfg!(unix) {
        cpu.as_secs_f64() * 1000.0
    } else {
        wall.as_secs_f64() * 1000.0
    };
    (
        out,
        Sample {
            wall_ms: wall.as_secs_f64() * 1000.0,
            cpu_ms,
        },
    )
}

/// Best-of-N, with the spread kept alongside.
///
/// The fastest run is the least contaminated by whatever else the machine was
/// doing, so it is the headline. The spread is what says whether to believe it.
#[derive(Debug, Clone, Serialize)]
struct Measured {
    best: Sample,
    median_wall_ms: f64,
    /// `(slowest - fastest) / fastest` over the samples.
    spread: f64,
    runs: usize,
}

impl Measured {
    fn from(samples: &mut [Sample]) -> Self {
        samples.sort_by(|a, b| a.wall_ms.total_cmp(&b.wall_ms));
        let best = samples[0];
        let worst = samples[samples.len() - 1];
        let median = samples[samples.len() / 2].wall_ms;
        Measured {
            best,
            median_wall_ms: median,
            spread: if best.wall_ms > 0.0 {
                (worst.wall_ms - best.wall_ms) / best.wall_ms
            } else {
                0.0
            },
            runs: samples.len(),
        }
    }

    /// Marker for a sample set too noisy to read.
    fn flag(&self) -> &'static str {
        if self.spread > SPREAD_WARN {
            " !"
        } else {
            ""
        }
    }
}

/// Repeat `f` `runs` times and reduce to a [`Measured`].
fn best_of(runs: usize, mut f: impl FnMut()) -> Measured {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs.max(1) {
        let (_, s) = measure(&mut f);
        samples.push(s);
    }
    Measured::from(&mut samples)
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Everything one invocation measured, keyed to what it measured it on.
#[derive(Debug, Serialize)]
struct Results {
    corpus_version: String,
    revision: String,
    machine: String,
    logical_cpus: usize,
    runs: usize,
    /// Per board, single-threaded.
    boards: BTreeMap<String, Measured>,
    /// Thread count to equal-work scaling, when a sweep ran.
    sweep: Option<Sweep>,
}

/// A thread sweep over equal work.
#[derive(Debug, Serialize)]
struct Sweep {
    /// The board the sweep ran on, and how many solves each thread did.
    board: usize,
    solves_per_thread: usize,
    /// Thread count to measurement, in ascending order of thread count.
    points: Vec<SweepPoint>,
}

/// One configuration of the sweep.
#[derive(Debug, Serialize)]
struct SweepPoint {
    threads: usize,
    measured: Measured,
    /// Throughput relative to the single-threaded point.
    speedup: f64,
}

/// Short git description of the working tree, or `unknown` outside a checkout.
fn revision() -> String {
    Command::new("git")
        .args(["describe", "--always", "--dirty", "--tags"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Human-readable CPU description, for the results header.
fn machine() -> String {
    #[cfg(target_os = "macos")]
    let probe = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output();
    #[cfg(not(target_os = "macos"))]
    let probe = Command::new("uname").arg("-m").output();

    probe
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Logical CPU count, defaulting to 1 where it cannot be determined.
fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "solver-bench")]
#[command(about = "Performance harness for the double-dummy solver")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

/// Options for `latency`.
#[derive(Parser)]
struct LatencyArgs {
    /// A file of PBN deal strings, one per line.
    pbn: PathBuf,
    /// Most repeats of any one deal; the fastest is kept.
    ///
    /// The minimum, not the mean: a single-threaded solve on an 8P+4E machine
    /// is occasionally scheduled onto an efficiency core at roughly a third
    /// the speed, and that does not average out, it just adds a slow mode.
    /// The fastest run is the one that got a performance core and an
    /// uncontended cache.
    #[arg(long, default_value_t = 15)]
    runs: usize,
    /// Stop repeating a deal once it has had this many milliseconds, after a
    /// floor of three repeats.
    ///
    /// Deals in this workload span more than tenfold, and the two ends need
    /// opposite treatment. A deal near the median is short enough that
    /// scheduling noise is a large fraction of it, so it wants many repeats --
    /// and being short, they are cheap. A deal in the slow tail is long enough
    /// that a real difference is visible immediately, and repeating it fifteen
    /// times would dominate the run for no gain. Budgeting the time per deal
    /// rather than fixing the count gives each end what it needs.
    #[arg(long, default_value_t = 300.0)]
    budget_ms: f64,
    /// Also measure DDS on each deal, for the same-timing-code comparison.
    #[cfg(feature = "dds-reference")]
    #[arg(long)]
    dds: bool,
    /// Write per-deal milliseconds here as TSV: index, ours, dds.
    #[arg(long)]
    tsv: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Measure each board single-threaded, and optionally sweep thread counts.
    Run(RunArgs),
    /// Sweep thread counts over equal work, without the per-board pass.
    Sweep(RunArgs),
    /// Check every board still solves to its pinned double-dummy table.
    Verify(CorpusArgs),
    /// Measure DDS on the same corpus, with the same timing code.
    #[cfg(feature = "dds-reference")]
    Reference(ReferenceArgs),
    /// Report nodes searched per deal, for comparison against the C++
    /// reference's `-S` output. Timing tells you the cost of the search;
    /// this tells you its size, and only both together say whether a gap is
    /// per-node execution or extra work.
    Nodes {
        /// A file of PBN deal strings, one per line.
        pbn: PathBuf,
        /// Break each deal down by cell (strain and declarer), which is what
        /// says *which* search to trace when a total diverges.
        #[arg(long)]
        per_cell: bool,
    },
    /// Per-deal latency over a file of deals, for the percentile view.
    ///
    /// The mean over a corpus is a tail measurement in disguise -- on random
    /// deals the slowest fifth is about half of all the work -- so what a
    /// person waiting on one board experiences is a distribution, not an
    /// average. This reports one time per deal so the percentiles can be
    /// taken, rather than a single figure.
    Latency(LatencyArgs),
    /// Instructions and cycles for a whole corpus, in one pass.
    ///
    /// The quick answer to "did my change remove work". Instructions retired
    /// are a property of the code rather than of the machine, so one run of
    /// this says more than fifteen wall-clock runs, and it can be trusted on a
    /// busy machine. Confirm anything it likes with `run`, because fewer
    /// instructions is not the same as less time.
    Cost {
        /// A file of PBN deal strings, one per line.
        pbn: PathBuf,
        /// Repeats; the lowest instruction count is reported.
        #[arg(long, default_value_t = 1)]
        runs: usize,
    },
    /// Compare two results files.
    Compare {
        /// The earlier results file (the baseline).
        baseline: PathBuf,
        /// The later results file.
        current: PathBuf,
    },
}

#[derive(Args, Clone)]
struct CorpusArgs {
    /// Corpus file to read.
    #[arg(long, default_value = "bench/corpus.json")]
    corpus: PathBuf,
}

/// Options for the DDS reference comparison.
#[cfg(feature = "dds-reference")]
#[derive(Args, Clone)]
struct ReferenceArgs {
    #[command(flatten)]
    corpus: CorpusArgs,

    /// Repeats per measurement; the fastest is reported.
    #[arg(long, default_value_t = 3)]
    runs: usize,

    /// Thread count to measure at. Both solvers get the same one.
    ///
    /// Exactly one value: DDS cannot survive a second `SetResources`, so
    /// measuring a curve means one process per thread count.
    #[arg(long, value_delimiter = ',', default_values_t = [1usize])]
    dds_threads: Vec<usize>,

    /// DDS threading backend: 3 = GCD, 5 = STL. GCD is DDS's own default on
    /// macOS and dispatches at background priority, which Apple Silicon
    /// confines to the efficiency cores; STL is the like-for-like choice.
    #[arg(long, default_value_t = 5)]
    dds_threading: usize,

    /// Solve these deals instead of generated ones: a file of PBN deal
    /// strings, one per line. Lets an external solver be measured on exactly
    /// the same deals.
    #[arg(long)]
    throughput_pbn: Option<std::path::PathBuf>,

    /// Distinct deals to generate for the throughput measurement. Ten boards
    /// is not enough work to keep twelve threads busy, and the corpus cannot
    /// simply be repeated -- see `generated_deals`.
    #[arg(long, default_value_t = 120)]
    throughput_deals: usize,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    corpus: CorpusArgs,

    /// Repeats per measurement; the fastest is reported.
    #[arg(long, default_value_t = 3)]
    runs: usize,

    /// Measure only the median-cost board. Deterministic, so two quick runs
    /// are comparable with each other.
    #[arg(long)]
    quick: bool,

    /// Skip the thread sweep (`run` only).
    #[arg(long)]
    no_sweep: bool,

    /// Highest thread count to sweep. Defaults to the logical CPU count.
    #[arg(long)]
    max_threads: Option<usize>,

    /// Grow the sweep workload until even the fastest configuration runs at
    /// least this long. Short sweeps measure thread startup, not scaling.
    #[arg(long, default_value_t = DEFAULT_MIN_SWEEP_SECONDS)]
    min_sweep_seconds: f64,

    /// Write results JSON here. Defaults to `bench/results/<revision>.json`.
    #[arg(long)]
    json: Option<PathBuf>,
}

/// Solve every board once, single-threaded, and report per-board cost.
fn per_board(
    corpus: &Corpus,
    deals: &[Deal],
    which: &[usize],
    runs: usize,
) -> BTreeMap<String, Measured> {
    let mut out = BTreeMap::new();
    println!("board  contract      wall ms     cpu ms   median   spread  runs");
    println!("{}", "-".repeat(68));
    for &i in which {
        let deal = &deals[i];
        let m = best_of(runs, || {
            let _ = solve_dd_table(deal);
        });
        println!(
            "{:>5}  {:<8}  {:>10.1} {:>10.1} {:>8.1} {:>7.1}% {:>5}{}",
            corpus.boards[i].board,
            corpus.boards[i].contract,
            m.best.wall_ms,
            m.best.cpu_ms,
            m.median_wall_ms,
            m.spread * 100.0,
            m.runs,
            m.flag(),
        );
        out.insert(corpus.boards[i].board.to_string(), m);
    }
    out
}

/// Sweep thread counts over *equal work per thread*.
///
/// Every thread solves the same board the same number of times, so the only
/// thing the ratio can reflect is how well concurrent solves coexist. A sweep
/// that hands one board to each thread would instead measure the spread of
/// board difficulty, which here is more than an order of magnitude.
fn sweep(deal: &Deal, board: usize, args: &RunArgs, max_threads: usize) -> Sweep {
    // Size the workload against the *widest* configuration, which is the
    // fastest in wall-clock terms and therefore the one at risk of being all
    // startup. Everything narrower then runs at least as long.
    let mut solves = 1usize;
    loop {
        let (_, s) = measure(|| run_threads(deal, max_threads, solves));
        if s.wall_ms / 1000.0 >= args.min_sweep_seconds || solves >= 4096 {
            break;
        }
        let ratio = (args.min_sweep_seconds * 1000.0 / s.wall_ms.max(0.01)).ceil() as usize;
        solves = (solves * ratio.max(2)).min(4096);
    }

    println!(
        "\nthread sweep: board {board}, {solves} solves per thread, best of {}",
        args.runs
    );
    println!("threads      wall ms     cpu ms   cores   speedup   spread");
    println!("{}", "-".repeat(60));

    let mut points: Vec<SweepPoint> = Vec::new();
    let mut single: Option<f64> = None;
    for threads in 1..=max_threads {
        let m = best_of(args.runs, || run_threads(deal, threads, solves));
        // Throughput per unit time: N threads each doing `solves` solves.
        let work = threads as f64;
        let rate = work / m.best.wall_ms;
        let base = *single.get_or_insert(rate);
        let speedup = rate / base;
        println!(
            "{:>7} {:>12.1} {:>10.1} {:>7.2} {:>9.2}x {:>7.1}%{}",
            threads,
            m.best.wall_ms,
            m.best.cpu_ms,
            m.best.cores_busy(),
            speedup,
            m.spread * 100.0,
            m.flag(),
        );
        points.push(SweepPoint {
            threads,
            measured: m,
            speedup,
        });
    }

    report_curve(&points);

    Sweep {
        board,
        solves_per_thread: solves,
        points,
    }
}

/// Run `threads` threads, each solving `solves` times.
fn run_threads(deal: &Deal, threads: usize, solves: usize) {
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..solves {
                    let _ = solve_dd_table(deal);
                }
            });
        }
    });
}

/// Say what the curve's shape means.
///
/// A curve that flattens past the physical core count is ordinary saturation.
/// A curve that turns *down* — more threads, less throughput — is contention,
/// and that is a bug rather than a limit.
fn report_curve(points: &[SweepPoint]) {
    let Some(peak) = points.iter().max_by(|a, b| a.speedup.total_cmp(&b.speedup)) else {
        return;
    };
    let last = &points[points.len() - 1];
    if peak.threads == last.threads {
        println!("\npeak {:.2}x at {} threads", peak.speedup, peak.threads);
    } else {
        println!(
            "\npeak {:.2}x at {} threads, falling to {:.2}x at {}",
            peak.speedup, peak.threads, last.speedup, last.threads
        );
    }
    let regression = (peak.speedup - last.speedup) / peak.speedup;
    if regression > 0.10 {
        println!(
            "  REGRESSION: throughput falls {:.0}% from its peak by {} threads.\n\
               More workers doing less work is contention on shared state, not\n\
               saturation. Re-run with a different workload size: if the turnover\n\
               moves, the cost is per-run; if it stays put, it is contention.",
            regression * 100.0,
            last.threads
        );
    } else if last.speedup < peak.speedup * 0.95 {
        println!(
            "  Plateau past {} threads — ordinary saturation.",
            peak.threads
        );
    }
}

/// Check every board against its pinned table.
fn verify(corpus: &Corpus, deals: &[Deal]) -> bool {
    let mut ok = true;
    println!("board  expected              computed              result");
    println!("{}", "-".repeat(62));
    for (board, deal) in corpus.boards.iter().zip(deals) {
        let got = dd_table_to_pbn(&solve_dd_table(deal));
        let good = got == board.ddtricks;
        ok &= good;
        println!(
            "{:>5}  {}  {}  {}",
            board.board,
            board.ddtricks,
            got,
            if good { "ok" } else { "MISMATCH" }
        );
    }
    println!(
        "\n{} of {} boards agree with the corpus.",
        corpus
            .boards
            .iter()
            .zip(deals)
            .filter(|(b, d)| { dd_table_to_pbn(&solve_dd_table(d)) == b.ddtricks })
            .count(),
        corpus.boards.len()
    );
    ok
}

/// Results as re-read for a comparison. Deliberately a separate, permissive
/// shape: a baseline written by an older revision should still be readable.
#[derive(Debug, Deserialize)]
struct StoredResults {
    corpus_version: String,
    revision: String,
    machine: String,
    boards: BTreeMap<String, StoredMeasured>,
    sweep: Option<StoredSweep>,
}

#[derive(Debug, Deserialize)]
struct StoredMeasured {
    best: StoredSample,
    spread: f64,
}

#[derive(Debug, Deserialize)]
struct StoredSample {
    wall_ms: f64,
    cpu_ms: f64,
}

#[derive(Debug, Deserialize)]
struct StoredSweep {
    solves_per_thread: usize,
    points: Vec<StoredSweepPoint>,
}

#[derive(Debug, Deserialize)]
struct StoredSweepPoint {
    threads: usize,
    speedup: f64,
}

/// Compare two result files, board by board and point by point.
fn compare(baseline: &Path, current: &Path) -> Result<(), String> {
    let read = |p: &Path| -> Result<StoredResults, String> {
        let text =
            std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} is not results JSON: {e}", p.display()))
    };
    let (a, b) = (read(baseline)?, read(current)?);

    println!("baseline : {} on {}", a.revision, a.machine);
    println!("current  : {} on {}", b.revision, b.machine);
    if a.corpus_version != b.corpus_version {
        println!(
            "\nWARNING: corpus differs ({} vs {}). The two runs measured different\n\
             work, and the per-board numbers below are not comparable.",
            a.corpus_version, b.corpus_version
        );
    }
    if a.machine != b.machine {
        println!("\nWARNING: different machines. Read the ratios, not the absolute times.");
    }

    println!("\nboard      base ms      now ms    change   base cpu    now cpu    note");
    println!("{}", "-".repeat(78));
    let mut ratios = Vec::new();
    let mut cpu_ratios = Vec::new();
    for (board, base) in &a.boards {
        let Some(now) = b.boards.get(board) else {
            println!(
                "{board:>5}  {:>11.1}           -         -          -          -    (dropped)",
                base.best.wall_ms
            );
            continue;
        };
        let change = now.best.wall_ms / base.best.wall_ms;
        ratios.push(change);
        if base.best.cpu_ms > 0.0 {
            cpu_ratios.push(now.best.cpu_ms / base.best.cpu_ms);
        }
        // A change smaller than either run's own spread is not a change.
        let noise = base.spread.max(now.spread);
        let note = if (change - 1.0).abs() <= noise {
            "within noise"
        } else if change < 1.0 {
            "faster"
        } else {
            "SLOWER"
        };
        println!(
            "{board:>5}  {:>11.1} {:>11.1} {:>8.1}% {:>10.1} {:>10.1}    {note}",
            base.best.wall_ms,
            now.best.wall_ms,
            (change - 1.0) * 100.0,
            base.best.cpu_ms,
            now.best.cpu_ms,
        );
    }
    if !ratios.is_empty() {
        // Geometric mean: these are ratios, and ratios do not average.
        let logs: f64 = ratios.iter().map(|r| r.ln()).sum();
        let geo = (logs / ratios.len() as f64).exp();
        println!(
            "\noverall wall: {:.3}x ({:+.1}%), geometric mean over {} boards",
            geo,
            (geo - 1.0) * 100.0,
            ratios.len()
        );
        if !cpu_ratios.is_empty() {
            let logs: f64 = cpu_ratios.iter().map(|r| r.ln()).sum();
            let geo_cpu = (logs / cpu_ratios.len() as f64).exp();
            println!(
                "overall cpu : {:.3}x ({:+.1}%) — this is the per-core figure, and it moves\n\
                 {:14}only when the search itself got cheaper or dearer.",
                geo_cpu,
                (geo_cpu - 1.0) * 100.0,
                ""
            );
        }
    }

    if let (Some(sa), Some(sb)) = (&a.sweep, &b.sweep) {
        if sa.solves_per_thread != sb.solves_per_thread {
            println!(
                "\nNote: the sweeps sized their workload differently ({} vs {} solves per\n\
                 thread), because each run grows it until the widest configuration is long\n\
                 enough to measure. The speedup curves below are still comparable — each is\n\
                 normalised to its own single-threaded point — but absolute sweep times are\n\
                 not.",
                sa.solves_per_thread, sb.solves_per_thread
            );
        }
        println!("\nthreads   base speedup   now speedup       change");
        println!("{}", "-".repeat(52));
        for pa in &sa.points {
            if let Some(pb) = sb.points.iter().find(|p| p.threads == pa.threads) {
                println!(
                    "{:>7} {:>14.2}x {:>13.2}x {:>11.2}x",
                    pa.threads,
                    pa.speedup,
                    pb.speedup,
                    pb.speedup / pa.speedup
                );
            }
        }
    }
    Ok(())
}

/// Measure DDS beside this solver, board for board.
///
/// Both are called from this process by the same timing code on the same
/// corpus, which is a cleaner comparison than running two programs and
/// subtracting their startup.
///
/// The two numbers are not the same *kind* of number, and the output says so.
/// DDS parallelises the twenty entries *within* one table; this solver has no
/// intra-solve parallelism and instead scales across independent deals. So
/// DDS's threaded column is its best time for one board, while ours is a
/// single core's — and a caller with many boards to solve should compare
/// DDS's threaded figure against our single-threaded one divided by the
/// scaling in `sweep`.
#[cfg(feature = "dds-reference")]
fn reference(args: &ReferenceArgs) -> Result<(), String> {
    let (corpus, deals) = Corpus::load(&args.corpus.corpus)?;

    println!(
        "corpus   : {} ({} boards)",
        corpus.version,
        corpus.boards.len()
    );
    println!("revision : {}", revision());
    println!("machine  : {} ({} logical)", machine(), logical_cpus());
    println!("runs     : best of {}\n", args.runs);

    // DDS allocates its per-thread working memory when resources are set, and
    // faults with "Memory::GetPtr" if a solve is attempted before that. It is
    // process-global state, so it is set here once before any call.
    //
    // Set it to the *first* configuration we will measure rather than to 1, so
    // that a single-valued `--dds-threads` needs only one `SetResources` for
    // the whole run. DDS cannot survive a second one -- it tears its thread
    // memory down before rebuilding it, and the rebuild does not always
    // happen -- so measuring several thread counts means one process each.
    if args.dds_threads.len() > 1 {
        return Err(
            "--dds-threads takes one value. DDS tears its thread memory down on a \n\
             second SetResources and does not always rebuild it, so a thread-count \n\
             curve means one process per point."
                .into(),
        );
    }
    let threads = args.dds_threads.first().copied().unwrap_or(1);
    dds::set_backend(args.dds_threading as i32)?;
    dds::set_threads(threads);
    let (backend, dds_cores, dds_threads) = dds::info();
    println!(
        "dds      : {backend} threading, {dds_cores} cores seen, {dds_threads} threads made\n"
    );

    // Agreement first: a timing comparison between two solvers that disagree
    // is meaningless.
    // Solved as one batch, so this doubles as the check on the batched result
    // indexing that the throughput measurement below depends on.
    let all_pbns: Vec<&str> = corpus.boards.iter().map(|b| b.pbn.as_str()).collect();
    let dds_tables = dds::solve_tables(&all_pbns)?;

    let mut disagreements = 0;
    for ((board, deal), dds_table) in corpus.boards.iter().zip(&deals).zip(&dds_tables) {
        let ours = dd_table_to_pbn(&solve_dd_table(deal));
        let theirs = encode_ddtricks(dds_table);
        if ours != theirs {
            println!(
                "board {}: DISAGREEMENT\n  ours {ours}\n  dds  {theirs}",
                board.board
            );
            disagreements += 1;
        }
    }
    if disagreements > 0 {
        return Err(format!(
            "{disagreements} board(s) disagree; the timings below would be meaningless"
        ));
    }
    println!("all {} boards agree with DDS\n", corpus.boards.len());

    let mut header = format!("{:>5}  {:>10}", "board", "ours 1t");
    for t in &args.dds_threads {
        header.push_str(&format!("  {:>8}", format!("dds {t}t")));
    }
    header.push_str(&format!("  {:>9}", "ours/dds1"));
    println!("{header}");
    println!("{}", "-".repeat(header.len()));

    let mut ratios = Vec::new();
    for (board, deal) in corpus.boards.iter().zip(&deals) {
        let ours = best_of(args.runs, || {
            let _ = solve_dd_table(deal);
        });
        let mut line = format!("{:>5}  {:>10.1}", board.board, ours.best.wall_ms);
        let mut first: Option<f64> = None;
        for &threads in &args.dds_threads {
            dds::set_threads(threads);
            let mut err = None;
            let m = best_of(args.runs, || {
                if let Err(e) = dds::solve_table(&board.pbn) {
                    err = Some(e);
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
            first.get_or_insert(m.best.wall_ms);
            line.push_str(&format!("  {:>8.1}", m.best.wall_ms));
        }
        if let Some(dds1) = first {
            let ratio = ours.best.wall_ms / dds1;
            ratios.push(ratio);
            line.push_str(&format!("  {ratio:>8.2}x"));
        }
        println!("{line}");
    }

    if !ratios.is_empty() {
        let logs: f64 = ratios.iter().map(|r| r.ln()).sum();
        let geo = (logs / ratios.len() as f64).exp();
        println!(
            "\nPer core we are {geo:.2}x DDS's cost, geometric mean over {} boards.\n\
             That figure is untouched by any threading work — it moves only when\n\
             the search itself gets cheaper.",
            ratios.len()
        );
    }

    throughput(args, threads)
}

/// Deal-level throughput: the whole corpus, both solvers, at the same thread
/// count, over one shared work list.
///
/// This is the comparison someone choosing a solver for their own machine
/// actually cares about, and it is the one the per-board table above cannot
/// answer. It is only honest because DDS gets the deals in a batch: given one
/// deal at a time it has five work items to spread over N threads and the
/// result measures load imbalance rather than scaling.
///
/// Each solver parallelises however it likes, and both take the same unit: a
/// (deal, strain) pair off a shared cursor, which is work-stealing in all but
/// name. Whole deals were the unit here until the case-2 measurement showed
/// what that costs on a small batch -- see `throughput_ours` -- and DDS is
/// handed the list in chunks of forty and schedules the pairs itself.
#[cfg(feature = "dds-reference")]
fn throughput(args: &ReferenceArgs, threads: usize) -> Result<(), String> {
    use std::sync::atomic::{AtomicU8, Ordering};

    let generated = match &args.throughput_pbn {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            let mut v = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let deal = Deal::from_pbn(line)
                    .ok_or_else(|| format!("{}:{}: unparseable deal", path.display(), i + 1))?;
                v.push((line.to_string(), deal));
            }
            if v.is_empty() {
                return Err(format!("{} held no deals", path.display()));
            }
            v
        }
        None => generated_deals(args.throughput_deals.max(1))?,
    };
    let pbns: Vec<&str> = generated.iter().map(|(p, _)| p.as_str()).collect();
    let work: Vec<&Deal> = generated.iter().map(|(_, d)| d).collect();

    // Check a sample before timing. A generator that emitted well-formed but
    // wrongly ordered deals would still parse, still solve, and still produce
    // plausible timings -- of a workload that is not the one being reported.
    let sample = work.len().min(5);
    let mut agreed = Vec::with_capacity(sample);
    for (i, (pbn, deal)) in generated.iter().take(sample).enumerate() {
        let ours = dd_table_to_pbn(&solve_dd_table(deal));
        let theirs = encode_ddtricks(&dds::solve_table(pbn)?);
        if ours != theirs {
            return Err(format!(
                "generated deal {i} disagrees, so the throughput workload is not \n\
                 the same work for both solvers\n  ours {ours}\n  dds  {theirs}\n  {pbn}"
            ));
        }
        agreed.push(ours);
    }

    println!(
        "\ndeal-level throughput: {} deals ({sample} checked), \
         {threads} thread(s), best of {}",
        work.len(),
        args.runs
    );

    // Twenty cells per deal, laid out as `DdTricks::tricks` is: cell
    // `[dir][strain]` of deal `d` at `d * 20 + dir * 5 + strain`. Filling it
    // rather than discarding the answers is what DDS is doing in the column
    // beside us, and a byte store against a millisecond search is free.
    let table: Vec<AtomicU8> = (0..work.len() * 20).map(|_| AtomicU8::new(0)).collect();
    let ours = best_of(args.runs, || throughput_ours(&work, threads, &table));

    // The split is only a speed-up if the pieces still add up to the table the
    // serial path produces. Twenty cells written from five threads is exactly
    // where an index slip hides, and it would show as a wrong answer rather
    // than a wrong time -- so re-read the cells the sample above already
    // agreed on, and require the parallel assembly to reproduce them.
    for (i, expected) in agreed.iter().enumerate() {
        let mut cells = [[0u8; 5]; 4];
        for (dir, row) in cells.iter_mut().enumerate() {
            for (strain, cell) in row.iter_mut().enumerate() {
                *cell = table[i * 20 + dir * 5 + strain].load(Ordering::Relaxed);
            }
        }
        let assembled = encode_ddtricks(&cells);
        if &assembled != expected {
            return Err(format!(
                "deal {i}: solving by (deal, strain) pair did not reproduce the \n\
                 serial table\n  serial   {expected}\n  parallel {assembled}"
            ));
        }
    }

    let mut err = None;
    let dds = best_of(args.runs, || {
        if let Err(e) = dds::solve_tables(&pbns) {
            err = Some(e);
        }
    });
    if let Some(e) = err {
        return Err(e);
    }

    println!(
        "{:>8}  {:>10}  {:>10}  {:>7}  {:>11}",
        "", "wall ms", "cpu ms", "cores", "deals/sec"
    );
    for (name, m) in [("ours", &ours), ("dds", &dds)] {
        println!(
            "{:>8}  {:>10.1}  {:>10.1}  {:>7.2}  {:>11.1}",
            name,
            m.best.wall_ms,
            m.best.cpu_ms,
            m.best.cpu_ms / m.best.wall_ms,
            work.len() as f64 * 1000.0 / m.best.wall_ms,
        );
    }
    let ratio = ours.best.wall_ms / dds.best.wall_ms;
    println!("\nOn {threads} thread(s) we take {ratio:.2}x DDS's wall clock for the same deals.");
    Ok(())
}

/// One timed pass of this port over the whole work list, filling `table`.
///
/// **The unit of parallel work is a (deal, strain) pair, not a deal.** That is
/// the only thing this function does differently from the obvious loop, and on
/// a small batch it is the difference between using the machine and waiting on
/// one board. Twenty-seven deals is twenty-seven items to spread over twelve
/// threads when deal cost spans roughly tenfold: the run ends when the slowest
/// deal ends, and threads that finished early sit idle. The same twenty-seven
/// are 135 pairs, each a fifth the size, and the expensive deal's five strains
/// now run beside each other instead of one after another. Whole deals scaled
/// 5.9x on twelve threads against DDS's 7.9x; pairs scale 7.6x. See case 2 of
/// `bench/comparison/RESULTS.md` for the measurement, before and after.
///
/// A strain is the *smallest* honest unit. Its four declarers share reset
/// caches and a chain of MTD(f) seeds, so splitting further would change the
/// search rather than merely reschedule it; `TableSolver::solve_strain` is
/// where that is guaranteed. Between strains nothing is shared -- each thread
/// solves on its own thread-local `TableSolver`, which resets both caches on
/// entry exactly as the serial path does at every strain boundary -- so
/// strains of one deal may run concurrently on different threads.
#[cfg(feature = "dds-reference")]
fn throughput_ours(work: &[&Deal], threads: usize, table: &[std::sync::atomic::AtomicU8]) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let items = work.len() * STRAINS.len();
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= items {
                    break;
                }
                let (deal, strain) = (i / STRAINS.len(), i % STRAINS.len());
                let column = solve_dd_strain(work[deal], STRAINS[strain]);
                for (dir, tricks) in column.iter().enumerate() {
                    table[deal * 20 + dir * 5 + strain].store(*tricks, Ordering::Relaxed);
                }
            });
        }
    });
}

/// Deterministic pseudo-random deals for the throughput measurement.
///
/// The corpus cannot be used for this, and neither can repeats of it. DDS
/// de-duplicates identical deals inside a batch -- `DetectCalcDuplicates` and
/// `CopyCalcSingle` -- and copies a result rather than solving again. Eight
/// copies of the ten-board corpus is eighty solves for us and ten for DDS,
/// which measured as a 4.7x win for DDS that was entirely an artefact of the
/// workload. Distinct deals are the only honest comparison; generating them
/// from a fixed seed keeps runs comparable without committing a large fixture.
///
/// Random deals are also the better workload here on their own merits: the
/// corpus was curated to span 16 ms to 300 ms, which is what makes it a good
/// per-board fixture and a poor model of a real file.
#[cfg(feature = "dds-reference")]
fn generated_deals(count: usize) -> Result<Vec<(String, Deal)>, String> {
    // Index 0 is the ace, so sorting ascending yields PBN's descending ranks.
    const RANK_CHARS: [u8; 13] = *b"AKQJT98765432";

    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut deck: [usize; 52] = std::array::from_fn(|i| i);
        for i in (1..52).rev() {
            let j = (rand() % (i as u64 + 1)) as usize;
            deck.swap(i, j);
        }

        // Thirteen cards per seat in PBN's N,E,S,W order; card = suit * 13 +
        // rank, with suit 0 spades so the suit loop emits S.H.D.C as PBN wants.
        let mut pbn = String::from("N:");
        for seat in 0..4 {
            if seat > 0 {
                pbn.push(' ');
            }
            let hand = &deck[seat * 13..seat * 13 + 13];
            for suit in 0..4 {
                if suit > 0 {
                    pbn.push('.');
                }
                let mut ranks: Vec<usize> = hand
                    .iter()
                    .filter(|c| *c / 13 == suit)
                    .map(|c| c % 13)
                    .collect();
                ranks.sort_unstable();
                for r in ranks {
                    pbn.push(RANK_CHARS[r] as char);
                }
            }
        }

        let deal =
            Deal::from_pbn(&pbn).ok_or_else(|| format!("generated an unparseable deal: {pbn}"))?;
        out.push((pbn, deal));
    }
    Ok(out)
}

/// Print nodes searched per deal, and the total.
fn nodes_report(path: &PathBuf, per_cell: bool) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut total = 0u64;
    let mut count = 0usize;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let deal = Deal::from_pbn(line)
            .ok_or_else(|| format!("{}:{}: unparseable deal", path.display(), i + 1))?;
        let nodes = if per_cell {
            let (_, cells) = solve_dd_table_cells(&deal);
            for (strain, dir, n) in &cells {
                println!("{} {strain:?} {dir:?} {n}", i + 1);
            }
            cells.iter().map(|(_, _, n)| n).sum()
        } else {
            let (_, n) = solve_dd_table_with_nodes(&deal);
            println!("{} {n}", i + 1);
            n
        };
        total += nodes;
        count += 1;
    }
    eprintln!(
        "{count} deals, {total} nodes, {:.0} per deal",
        total as f64 / count as f64
    );
    Ok(())
}

/// Index of the median-cost board.
///
/// Cost is nodes searched, not wall time. Both need the same ten solves, but
/// nodes is a property of the deal and the search, so the answer is the same
/// on a loaded machine as on an idle one -- which is the whole point of a
/// board that two `--quick` runs can agree on.
///
/// It was wall time, ranked from a single unrepeated timing per board. Boards
/// 4 and 7 of `corpus-v1` sit about 6% apart, which is inside the drift on a
/// busy machine, so the median flipped between them from run to run and an A/B
/// silently averaged two different workloads. That cost a real measurement
/// before it was noticed; see "Two ways the harness will mislead you" in
/// `bench/results/release-profile.md`.
fn median_board(deals: &[Deal]) -> usize {
    let mut costs: Vec<(usize, u64)> = deals
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (_, nodes) = solve_dd_table_with_nodes(d);
            (i, nodes)
        })
        .collect();
    // Tie-break on index so the order is total, not merely deterministic-ish.
    costs.sort_by_key(|&(i, nodes)| (nodes, i));
    costs[costs.len() / 2].0
}

fn run(args: &RunArgs, do_sweep: bool) -> Result<(), String> {
    let (corpus, deals) = Corpus::load(&args.corpus.corpus)?;
    let cpus = logical_cpus();
    let max_threads = args.max_threads.unwrap_or(cpus).max(1);

    println!(
        "corpus   : {} ({} boards)",
        corpus.version,
        corpus.boards.len()
    );
    println!("revision : {}", revision());
    println!("machine  : {} ({cpus} logical)", machine());
    println!("runs     : best of {}\n", args.runs);

    let chosen = if args.quick {
        vec![median_board(&deals)]
    } else {
        (0..deals.len()).collect()
    };

    let boards = if do_sweep && args.quick {
        BTreeMap::new()
    } else {
        per_board(&corpus, &deals, &chosen, args.runs)
    };

    let sweep_result = if do_sweep {
        let i = if args.quick {
            chosen[0]
        } else {
            median_board(&deals)
        };
        Some(sweep(&deals[i], corpus.boards[i].board, args, max_threads))
    } else {
        None
    };

    let results = Results {
        corpus_version: corpus.version.clone(),
        revision: revision(),
        machine: machine(),
        logical_cpus: cpus,
        runs: args.runs,
        boards,
        sweep: sweep_result,
    };

    let path = args
        .json
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("bench/results/{}.json", results.revision)));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| format!("cannot serialise results: {e}"))?;
    std::fs::write(&path, json + "\n")
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("\nwrote {}", path.display());
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let outcome = match &cli.command {
        Cmd::Run(args) => run(args, !args.no_sweep),
        Cmd::Sweep(args) => run(args, true),
        Cmd::Verify(args) => Corpus::load(&args.corpus).and_then(|(c, d)| {
            if verify(&c, &d) {
                Ok(())
            } else {
                Err("corpus disagreement: the solver changed an answer".into())
            }
        }),
        #[cfg(feature = "dds-reference")]
        Cmd::Reference(args) => reference(args),
        Cmd::Nodes { pbn, per_cell } => nodes_report(pbn, *per_cell),
        Cmd::Latency(args) => latency_report(args),
        Cmd::Cost { pbn, runs } => cost_report(pbn, *runs),
        Cmd::Compare { baseline, current } => compare(baseline, current),
    };
    if let Err(e) = outcome {
        eprintln!("solver-bench: {e}");
        std::process::exit(1);
    }
}

/// Percentiles of a sorted slice, by nearest-rank.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[i.saturating_sub(1).min(sorted.len() - 1)]
}

/// Per-deal latency over a file of deals.
fn latency_report(args: &LatencyArgs) -> Result<(), String> {
    let text = std::fs::read_to_string(&args.pbn)
        .map_err(|e| format!("reading {}: {e}", args.pbn.display()))?;
    let mut deals = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let deal = Deal::from_pbn(line)
            .ok_or_else(|| format!("{}:{}: unparseable deal", args.pbn.display(), i + 1))?;
        deals.push((line.to_string(), deal));
    }
    if deals.is_empty() {
        return Err(format!("{}: no deals", args.pbn.display()));
    }

    println!("deals    : {}", deals.len());
    println!(
        "runs     : best of up to {}, or until a deal has had {:.0} ms",
        args.runs, args.budget_ms
    );
    println!("machine  : {} ({} logical)", machine(), logical_cpus());

    // DDS's resources are process-global and must be set before the first
    // solve; without this the first call dies with `Memory::GetPtr: 0 vs. 0`
    // and DDS calls `exit(1)`, taking the harness with it. One thread, and
    // the STL backend rather than DDS's macOS default of GCD, which
    // dispatches at background priority and so lands on the efficiency cores.
    #[cfg(feature = "dds-reference")]
    if args.dds {
        dds::set_backend(5)?;
        dds::set_threads(1);
        let (backend, cores, threads) = dds::info();
        println!("dds      : {backend} threading, {cores} cores seen, {threads} threads made");
    }

    let mut ours = Vec::with_capacity(deals.len());
    // Only ever pushed to under `dds-reference`; without that feature it stays
    // empty and every reader below already handles an empty column.
    #[cfg_attr(not(feature = "dds-reference"), allow(unused_mut))]
    let mut dds_ms: Vec<f64> = Vec::new();

    // Interleaved by deal rather than by solver: a deal's two figures are then
    // taken under the same machine conditions, which is what makes the
    // per-deal ratio meaningful even when the machine drifts over the run.
    /// Repeat until the budget is spent, keeping the fastest. At least three,
    /// at most `runs`.
    fn best_of(runs: usize, budget_ms: f64, mut once: impl FnMut() -> f64) -> f64 {
        let (mut best, mut spent) = (f64::MAX, 0.0);
        for i in 0..runs.max(3) {
            let ms = once();
            best = best.min(ms);
            spent += ms;
            if i + 1 >= 3 && spent >= budget_ms {
                break;
            }
        }
        best
    }

    for (pbn, deal) in &deals {
        ours.push(best_of(args.runs, args.budget_ms, || {
            measure(|| {
                let _ = solve_dd_table(deal);
            })
            .1
            .wall_ms
        }));

        #[cfg(feature = "dds-reference")]
        if args.dds {
            dds_ms.push(best_of(args.runs, args.budget_ms, || {
                measure(|| {
                    let _ = dds::solve_table(pbn);
                })
                .1
                .wall_ms
            }));
        }
        let _ = pbn;
    }

    if let Some(path) = &args.tsv {
        let mut out = String::from("deal\tours_ms\tdds_ms\n");
        for (i, o) in ours.iter().enumerate() {
            let d = dds_ms.get(i).map(|v| format!("{v:.3}")).unwrap_or_default();
            out.push_str(&format!("{}\t{o:.3}\t{d}\n", i + 1));
        }
        std::fs::write(path, out).map_err(|e| format!("writing {}: {e}", path.display()))?;
        println!("wrote {}", path.display());
    }

    let mut so = ours.clone();
    so.sort_by(f64::total_cmp);
    let mut sd = dds_ms.clone();
    sd.sort_by(f64::total_cmp);

    println!();
    if sd.is_empty() {
        println!("{:>6} {:>10}", "pct", "ours ms");
        for p in [50.0, 80.0, 90.0, 95.0, 99.0, 100.0] {
            println!("{:>6} {:>10.1}", fmt_pct(p), percentile(&so, p));
        }
    } else {
        println!(
            "{:>6} {:>10} {:>10} {:>10}",
            "pct", "ours ms", "dds ms", "ours/dds"
        );
        for p in [50.0, 80.0, 90.0, 95.0, 99.0, 100.0] {
            let (o, d) = (percentile(&so, p), percentile(&sd, p));
            println!("{:>6} {:>10.1} {:>10.1} {:>9.3}x", fmt_pct(p), o, d, o / d);
        }
        // Ratio of the whole set, and the ratio a user actually waits on.
        let sum_o: f64 = ours.iter().sum();
        let sum_d: f64 = dds_ms.iter().sum();
        println!(
            "\ntotal    {sum_o:>9.0} {sum_d:>10.0} {:>9.3}x",
            sum_o / sum_d
        );
    }
    Ok(())
}

/// `p100` reads badly as a percentile; it is the maximum.
fn fmt_pct(p: f64) -> String {
    if p >= 100.0 {
        "max".to_string()
    } else {
        format!("p{p:.0}")
    }
}

/// Instructions, cycles and nodes for a whole corpus.
fn cost_report(path: &PathBuf, runs: usize) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut deals = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        deals.push(
            Deal::from_pbn(line)
                .ok_or_else(|| format!("{}:{}: unparseable deal", path.display(), i + 1))?,
        );
    }
    if deals.is_empty() {
        return Err(format!("{}: no deals", path.display()));
    }
    if counters().is_none() {
        return Err("instruction counters are unavailable on this platform".into());
    }

    let (mut best_ins, mut best_cyc, mut best_ms, mut nodes) = (u64::MAX, u64::MAX, f64::MAX, 0u64);
    for _ in 0..runs.max(1) {
        let before = counters().unwrap_or((0, 0));
        let mut n = 0u64;
        let (_, sample) = measure(|| {
            for deal in &deals {
                let (_, c) = solve_dd_table_with_nodes(deal);
                n += c;
            }
        });
        let after = counters().unwrap_or((0, 0));
        best_ins = best_ins.min(after.0.saturating_sub(before.0));
        best_cyc = best_cyc.min(after.1.saturating_sub(before.1));
        best_ms = best_ms.min(sample.wall_ms);
        nodes = n;
    }

    println!("deals        {:>18}", deals.len());
    println!("nodes        {nodes:>18}");
    println!("instructions {best_ins:>18}");
    println!("cycles       {best_cyc:>18}");
    println!("wall ms      {best_ms:>18.1}");
    println!();
    println!("ins/node     {:>18.2}", best_ins as f64 / nodes as f64);
    println!("cycles/node  {:>18.2}", best_cyc as f64 / nodes as f64);
    // Below about 1.0 the search is stalling rather than computing, which is
    // what separates "removed instructions" from "made it faster".
    println!("IPC          {:>18.2}", best_ins as f64 / best_cyc as f64);
    Ok(())
}
