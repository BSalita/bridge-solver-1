//! bridge-claim - check bridge claims, and benchmark the claim checker.
//!
//! A claim is valid when the claiming side can take the tricks it claims
//! however the cards it cannot see lie. See `bridge_solver::claim` for the
//! model.
//!
//! Usage:
//!   bridge-claim bench                 # the full benchmark, on every core
//!   bridge-claim bench --preset quick  # the set the web page runs
//!   bridge-claim check claims.txt      # your own claims, one per line
//!
//! `bench` runs exactly the claims the web page runs, so the two can be set
//! side by side. Verdicts and node counts depend only on the claim and the
//! budget, never on the machine: if yours differ from someone else's, that is
//! a bug worth reporting. Only the times should differ.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use bridge_solver::cards::card_of;
use bridge_solver::claim_bench::{
    bench_claim, preset, verdict_letter, workload, Workload, PRESETS,
};
use bridge_solver::types::rank_name;
use bridge_solver::{
    ClaimPosition, Hands, Seat, Suit, DEFAULT_CLAIM_BUDGET, EAST, NORTH, NOTRUMP, NUM_RANKS, SOUTH,
    WEST,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bridge-claim")]
#[command(about = "Check bridge claims, and benchmark the claim checker")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Time the claim checker over fixed claims, on every core.
    Bench {
        /// Which set of claims: `quick` (what the web page runs by default)
        /// or `full` (every workload, 7 to 13 cards).
        #[arg(long, default_value = "full")]
        preset: String,
        /// Run only these workloads, instead of the preset's: rest-dd,
        /// strong-dd, decl-dd, def-dd, decl-all.
        #[arg(long, value_delimiter = ',')]
        workload: Vec<String>,
        /// Cards per hand, instead of the preset's.
        #[arg(long, value_delimiter = ',')]
        sizes: Vec<usize>,
        /// Claims per workload and size, instead of the preset's.
        #[arg(long)]
        count: Option<u64>,
        /// Node budget per claim. A claim that runs out is rejected.
        #[arg(long, default_value_t = DEFAULT_CLAIM_BUDGET)]
        budget: u64,
        /// Worker threads. Defaults to every core.
        #[arg(short = 'j', long = "threads")]
        threads: Option<usize>,
        /// Print every claim's verdict, nodes and time as JSON instead of the
        /// table.
        #[arg(long)]
        json: bool,
    },
    /// Check the claims in a file, one per line.
    ///
    /// Each line is seven fields separated by `|`:
    ///
    ///   deal | trump | declarer | claimer | leader | tricks | plays
    ///
    /// `deal` is a PBN deal at the start of play (`N:AKQ.xx... ...`), or an
    /// ending with four equal hands. `trump` is S, H, D, C or NT. Seats are
    /// N, E, S or W, and `leader` leads the first card of `plays`. `tricks`
    /// is how many of the tricks still to play, counting one in progress,
    /// the claiming side says it will take. `plays` is the cards played so
    /// far, like `HA H5 H3 H6 SK`, and may be empty. Blank lines and lines
    /// starting with `#` are skipped.
    Check {
        /// The file of claims.
        file: PathBuf,
        /// Node budget per claim. A claim that runs out is rejected.
        #[arg(long, default_value_t = DEFAULT_CLAIM_BUDGET)]
        budget: u64,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Bench {
            preset,
            workload,
            sizes,
            count,
            budget,
            threads,
            json,
        } => bench(&preset, &workload, &sizes, count, budget, threads, json),
        Cmd::Check { file, budget } => check(&file, budget),
    };
    if let Err(e) = result {
        eprintln!("bridge-claim: {e}");
        std::process::exit(1);
    }
}

/// One claim's outcome.
#[derive(Clone, Copy)]
struct Outcome {
    letter: char,
    nodes: u64,
    secs: f64,
}

fn bench(
    preset_name: &str,
    workloads: &[String],
    sizes: &[usize],
    count: Option<u64>,
    budget: u64,
    threads: Option<usize>,
    json: bool,
) -> Result<(), String> {
    let p = preset(preset_name).ok_or_else(|| {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        format!("no preset {preset_name:?}; one of {}", names.join(", "))
    })?;
    let chosen: Vec<&Workload> = if workloads.is_empty() {
        p.workloads.iter().filter_map(|n| workload(n)).collect()
    } else {
        workloads
            .iter()
            .map(|n| workload(n).ok_or_else(|| format!("no workload {n:?}")))
            .collect::<Result<_, _>>()?
    };
    let sizes = if sizes.is_empty() { p.sizes } else { sizes };
    let count = count.unwrap_or(p.count);
    let threads = threads
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1)
        .max(1);

    // Every claim of every row goes into one queue, so the threads stay busy
    // to the end rather than waiting on the slowest claim of each row.
    let rows: Vec<(&Workload, usize)> = chosen
        .iter()
        .flat_map(|&w| sizes.iter().map(move |&s| (w, s)))
        .collect();
    let jobs: Vec<(usize, u64)> = (0..rows.len())
        .flat_map(|r| (0..count).map(move |i| (r, i)))
        .collect();
    let results: Mutex<Vec<Option<Outcome>>> = Mutex::new(vec![None; jobs.len()]);
    let failure: Mutex<Option<String>> = Mutex::new(None);
    let next = AtomicUsize::new(0);

    if !json {
        println!(
            "bridge-claim {} on {}-{}, {threads} threads, budget {budget} nodes, {count} claims per row",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }
    let wall = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(row, index)) = jobs.get(j) else {
                    break;
                };
                let (w, size) = rows[row];
                let outcome = bench_claim(w, size, index)
                    .map_err(|e| e.to_string())
                    .and_then(|c| {
                        let start = Instant::now();
                        let r = c
                            .position
                            .check(c.tricks, budget)
                            .map_err(|e| e.to_string())?;
                        Ok(Outcome {
                            letter: verdict_letter(r.verdict),
                            nodes: r.nodes,
                            secs: start.elapsed().as_secs_f64(),
                        })
                    });
                match outcome {
                    Ok(o) => {
                        if let Ok(mut all) = results.lock() {
                            all[j] = Some(o);
                        }
                    }
                    Err(e) => {
                        if let Ok(mut f) = failure.lock() {
                            f.get_or_insert(format!("{} size {size} claim {index}: {e}", w.name));
                        }
                        // Drain the queue so every thread stops.
                        next.store(jobs.len(), Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let wall = wall.elapsed().as_secs_f64();
    if let Some(e) = failure.into_inner().map_err(|e| e.to_string())? {
        return Err(e);
    }
    let results = results.into_inner().map_err(|e| e.to_string())?;
    let row_outcomes = |r: usize| -> Vec<Outcome> {
        results
            .iter()
            .zip(&jobs)
            .filter(|(_, &(jr, _))| jr == r)
            .filter_map(|(o, _)| *o)
            .collect()
    };
    if json {
        print_json(&rows, row_outcomes, threads, budget, wall);
        return Ok(());
    }

    println!(
        "{:<10} {:>4} {:>4} {:>4} {:>4} {:>12} {:>10} {:>9}  verdicts",
        "workload", "size", "V", "X", "?", "nodes", "cpu ms", "ms/claim"
    );
    let (mut total_nodes, mut total_secs, mut total_claims) = (0u64, 0f64, 0u64);
    for (r, &(w, size)) in rows.iter().enumerate() {
        let row = row_outcomes(r);
        let tally = |c: char| row.iter().filter(|o| o.letter == c).count();
        let nodes: u64 = row.iter().map(|o| o.nodes).sum();
        let secs: f64 = row.iter().map(|o| o.secs).sum();
        let verdicts: String = row.iter().map(|o| o.letter).collect();
        println!(
            "{:<10} {size:>4} {:>4} {:>4} {:>4} {nodes:>12} {:>10.1} {:>9.2}  {verdicts}",
            w.name,
            tally('V'),
            tally('X'),
            tally('?'),
            secs * 1000.0,
            secs * 1000.0 / row.len().max(1) as f64
        );
        total_nodes += nodes;
        total_secs += secs;
        total_claims += row.len() as u64;
    }
    println!(
        "{total_claims} claims, {total_nodes} nodes, {:.1} s of checking in {:.1} s wall: {:.1} claims/s",
        total_secs,
        wall,
        total_claims as f64 / wall.max(1e-9)
    );
    println!("(cpu ms counts checking only; generating the claims is not timed)");
    Ok(())
}

/// The `--json` form of a benchmark run. Written by hand: every value is a
/// number or one of this program's own names, so nothing needs escaping.
fn print_json(
    rows: &[(&Workload, usize)],
    row_outcomes: impl Fn(usize) -> Vec<Outcome>,
    threads: usize,
    budget: u64,
    wall: f64,
) {
    let list = |values: Vec<String>| values.join(",");
    let rows_json: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(r, &(w, size))| {
            let row = row_outcomes(r);
            format!(
                r#"{{"workload":"{}","size":{size},"verdicts":"{}","nodes":[{}],"ms":[{}]}}"#,
                w.name,
                row.iter().map(|o| o.letter).collect::<String>(),
                list(row.iter().map(|o| o.nodes.to_string()).collect()),
                list(
                    row.iter()
                        .map(|o| format!("{:.3}", o.secs * 1000.0))
                        .collect()
                ),
            )
        })
        .collect();
    println!(
        r#"{{"version":"{}","os":"{}","arch":"{}","threads":{threads},"budget":{budget},"wall_s":{wall:.3},"rows":[{}]}}"#,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        rows_json.join(",")
    );
}

fn check(path: &PathBuf, budget: u64) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = format!("{}:{}", path.display(), i + 1);
        let (position, tricks) = parse_claim(line).map_err(|e| format!("{at}: {e}"))?;
        let start = Instant::now();
        let result = position
            .check(tricks, budget)
            .map_err(|e| format!("{at}: {e}"))?;
        println!(
            "{at}: {:?} ({} nodes, {:.1} ms)",
            result.verdict,
            result.nodes,
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

/// Parse one `check` line into a position and the tricks claimed.
fn parse_claim(line: &str) -> Result<(ClaimPosition, usize), String> {
    let fields: Vec<&str> = line.split('|').map(str::trim).collect();
    let [deal, trump, declarer, claimer, leader, tricks, plays] = fields[..] else {
        return Err(format!(
            "expected 7 fields separated by |, found {}",
            fields.len()
        ));
    };
    let hands = Hands::from_pbn(deal).ok_or_else(|| format!("unreadable deal {deal:?}"))?;
    let trump = parse_trump(trump)?;
    let tricks: usize = tricks
        .parse()
        .map_err(|_| format!("tricks {tricks:?} is not a number"))?;
    let plays = plays
        .split_whitespace()
        .map(parse_card)
        .collect::<Result<Vec<_>, _>>()?;
    let position = ClaimPosition::from_play(
        &hands,
        trump,
        parse_seat(declarer)?,
        parse_seat(claimer)?,
        parse_seat(leader)?,
        &plays,
    )
    .map_err(|e| e.to_string())?;
    Ok((position, tricks))
}

fn parse_seat(s: &str) -> Result<Seat, String> {
    match s.to_ascii_uppercase().as_str() {
        "N" => Ok(NORTH),
        "E" => Ok(EAST),
        "S" => Ok(SOUTH),
        "W" => Ok(WEST),
        _ => Err(format!("seat {s:?} is not N, E, S or W")),
    }
}

fn parse_suit(c: char) -> Option<Suit> {
    "SHDC".find(c.to_ascii_uppercase())
}

fn parse_trump(s: &str) -> Result<Suit, String> {
    let up = s.to_ascii_uppercase();
    if up == "N" || up == "NT" {
        return Ok(NOTRUMP);
    }
    let mut chars = up.chars();
    match (chars.next().and_then(parse_suit), chars.next()) {
        (Some(suit), None) => Ok(suit),
        _ => Err(format!("trump {s:?} is not S, H, D, C or NT")),
    }
}

/// A card like `SA`, `HT` or `D10`.
fn parse_card(s: &str) -> Result<usize, String> {
    let bad = || format!("card {s:?} is not a suit then a rank, like SA or HT");
    let mut chars = s.chars();
    let suit = chars.next().and_then(parse_suit).ok_or_else(bad)?;
    let rank_text = chars.as_str().to_ascii_uppercase();
    let rank_char = if rank_text == "10" {
        'T'
    } else {
        let mut r = rank_text.chars();
        match (r.next(), r.next()) {
            (Some(c), None) => c,
            _ => return Err(bad()),
        }
    };
    let rank = (0..NUM_RANKS)
        .find(|&r| rank_name(r) == rank_char)
        .ok_or_else(bad)?;
    Ok(card_of(suit, rank))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_claim_line() {
        let line = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72 \
                    | S | N | E | E | 2 | HA H5 H3 H6";
        let (position, tricks) = parse_claim(line).expect("parses");
        assert_eq!(tricks, 2);
        assert_eq!(position.tricks_left(), 12);
    }

    #[test]
    fn reads_cards() {
        assert_eq!(parse_card("SA"), Ok(card_of(0, NUM_RANKS - 1)));
        assert_eq!(parse_card("d10"), parse_card("DT"));
        assert!(parse_card("X4").is_err());
        assert!(parse_card("S").is_err());
    }
}
