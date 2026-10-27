//! `bridge-solver --bench`: time the solver on the built-in benchmark deals.
//!
//! The deals and presets are `bridge_solver::dd_bench`'s, the same ones the
//! benchmark web page runs, so a result from here sets directly against one
//! from a browser: identical tricks and node counts, and only the time differs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use bridge_solver::dd_bench::{bench_table, preset, BenchTable, PRESETS};

/// One deal's outcome.
struct Outcome {
    table: BenchTable,
    secs: f64,
}

/// Solve the preset's deals over `threads` threads and print the results.
pub fn run(preset_name: &str, threads: Option<usize>, json: bool) -> Result<(), String> {
    let p = preset(preset_name).ok_or_else(|| {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        format!("no preset {preset_name:?}; one of {}", names.join(", "))
    })?;
    let threads = threads
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1)
        .max(1);

    let results: Mutex<Vec<Option<Outcome>>> = Mutex::new((0..p.deals).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    if !json {
        println!(
            "bridge-solver {} on {}-{}, {threads} threads, preset {} ({} deals)",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            p.name,
            p.deals
        );
    }
    let wall = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= p.deals {
                    break;
                }
                let start = Instant::now();
                let table = bench_table(i);
                let secs = start.elapsed().as_secs_f64();
                if let (Some(table), Ok(mut all)) = (table, results.lock()) {
                    all[i] = Some(Outcome { table, secs });
                }
            });
        }
    });
    let wall = wall.elapsed().as_secs_f64();
    let results: Vec<Outcome> = results
        .into_inner()
        .map_err(|e| e.to_string())?
        .into_iter()
        .enumerate()
        .map(|(i, o)| o.ok_or_else(|| format!("deal {i} could not be solved")))
        .collect::<Result<_, _>>()?;

    let nodes: u64 = results.iter().map(|o| o.table.nodes).sum();
    let secs: f64 = results.iter().map(|o| o.secs).sum();
    if json {
        // Written by hand: every value is a number or a hex string.
        let list = |f: &dyn Fn(&Outcome) -> String| -> String {
            results.iter().map(f).collect::<Vec<_>>().join(",")
        };
        println!(
            r#"{{"version":"{}","os":"{}","arch":"{}","threads":{threads},"preset":"{}","wall_s":{wall:.3},"nodes":[{}],"tricks":[{}],"ms":[{}]}}"#,
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            p.name,
            list(&|o| o.table.nodes.to_string()),
            list(&|o| format!("\"{}\"", o.table.tricks_hex())),
            list(&|o| format!("{:.3}", o.secs * 1000.0)),
        );
        return Ok(());
    }

    // Per decile of the corpus rather than per deal: two hundred lines is a
    // wall, and ten rows still show where the time goes.
    println!(
        "{:>9} {:>14} {:>10} {:>9}",
        "deals", "nodes", "cpu ms", "ms/deal"
    );
    let step = p.deals.div_ceil(10).max(1);
    for (k, chunk) in results.chunks(step).enumerate() {
        let n: u64 = chunk.iter().map(|o| o.table.nodes).sum();
        let s: f64 = chunk.iter().map(|o| o.secs).sum();
        println!(
            "{:>9} {n:>14} {:>10.1} {:>9.1}",
            format!("{}-{}", k * step + 1, k * step + chunk.len()),
            s * 1000.0,
            s * 1000.0 / chunk.len() as f64
        );
    }
    println!(
        "{} deals, {nodes} nodes, {:.2} s of solving in {:.2} s wall: {:.1} deals/s, {:.2} M nodes/s per thread",
        results.len(),
        secs,
        wall,
        results.len() as f64 / wall.max(1e-9),
        nodes as f64 / secs.max(1e-9) / 1e6
    );
    Ok(())
}
