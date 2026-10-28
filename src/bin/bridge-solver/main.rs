//! bridge-solver - Double-dummy solver for PBN files
//!
//! Reads a PBN file containing bridge deals, performs double-dummy analysis,
//! and writes the results as Bridge Composer compatible tags, where Bridge
//! Composer itself puts them (see `fixtures/bridge-composer`):
//! - DoubleDummyTricks (compact encoding)
//! - OptimumScore (if vulnerability is known)
//! - ParContract (if vulnerability is known)
//!
//! all three one-line supplemental tag pairs, sorted alphabetically among the
//! board's other supplemental tags; and
//! - OptimumResultTable (full table)
//!
//! a supplemental *section*, below `[Auction]` and `[Play]` and sorted
//! alphabetically among any other sections.
//!
//! Boards whose deal is incomplete are passed through untouched.
//!
//! How a PBN file is shaped is `bridge_encodings::pbn::PbnDocument`'s: it holds
//! the file as written and splices the tags in, so `%` directives, `;` comments
//! and `{...}` commentary survive byte-for-byte, CRLF stays CRLF, a file that
//! ended without a newline still does, and a file with nothing to add is not
//! rewritten at all. What is this binary's is which tags to write and which
//! boards to write them on.
//!
//! Usage:
//!   bridge-solver -i <file.pbn> -o <file.pbn>   # one file to another
//!   bridge-solver -i <file.pbn>                 # one file to stdout
//!   bridge-solver -w -i <file.pbn> <dir> ...    # annotate in place, recursively

mod bench;

use bridge_encodings::pbn::{
    dd_table_to_pbn, is_optimum_result_row, optimum_result_table_header, optimum_result_table_rows,
    PbnDocument,
};
use bridge_solver::dds_engine::{self, Engine, Tricks};
use bridge_solver::{par, Hands, TableSolver};
use bridge_types::{DdTable, Direction, Strain, Vulnerability, DECLARERS};
use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Parser)]
#[command(name = "bridge-solver")]
#[command(about = "Double-dummy solver for PBN files")]
#[command(version)]
struct Args {
    /// Input PBN file(s) or director(ies); directories are searched recursively
    /// for *.pbn. Accepts several, which requires --in-place.
    #[arg(
        short = 'i',
        long = "input",
        required_unless_present = "bench",
        num_args = 1..
    )]
    input: Vec<String>,

    /// Output PBN file (if not specified, writes to stdout)
    #[arg(short = 'o', long = "output", conflicts_with = "in_place")]
    output: Option<String>,

    /// Rewrite each input file in place. Files whose content is unchanged are
    /// left alone, so a re-run touches nothing and build systems see no churn.
    #[arg(short = 'w', long = "in-place")]
    in_place: bool,

    /// Set the "double-dummy data has been verified" bit (0x00080000) in each
    /// annotated board's [BCFlags], adding the tag if absent. Note this marks
    /// provenance; it does not make Bridge Composer display the DD table.
    #[arg(long = "mark-verified")]
    mark_verified: bool,

    /// Recompute analysis for boards that already carry it. By default a board
    /// with a [DoubleDummyTricks] tag is left exactly as found, so annotating a
    /// collection only fills in what is missing.
    #[arg(long = "recalculate")]
    recalculate: bool,

    /// Verbose output - show progress
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Worker threads used for solving. Defaults to the machine's available
    /// parallelism; `1` solves serially.
    ///
    /// Work is spread over (deal, strain) pairs rather than whole deals. A
    /// strain is the smallest piece of a table that can move to another thread
    /// without changing the search: its four declarers share one pair of caches
    /// and a chain of MTD(f) seeds, and nothing crosses the boundary from one
    /// strain to the next. Deal cost spans roughly tenfold, so the finer unit is
    /// what keeps the last threads busy instead of waiting on the slowest deal.
    ///
    /// Output does not depend on this: tables are assembled by index, not by the
    /// order the work finished, so any thread count produces identical bytes.
    #[arg(short = 'j', long = "threads", value_name = "N")]
    threads: Option<usize>,

    /// Which double-dummy engine computes the tables: `native` (this solver),
    /// or `dds`, a DDS-compatible library such as DDS 2.9 or ddss, loaded from
    /// --dds-lib. Par and the written tags are the same either way.
    #[arg(long = "engine", value_name = "ENGINE", default_value = "native")]
    engine: String,

    /// The library for `--engine dds` (dds.dll, libdds.so, a ddss build).
    /// Defaults to $BRIDGE_SOLVER_DDS_LIB, then $DDSS_DLL_PATH. With --threads,
    /// DDS's own thread pool is capped instead.
    #[arg(long = "dds-lib", value_name = "PATH")]
    dds_lib: Option<PathBuf>,

    /// Time the solver instead of annotating files: solve the built-in
    /// benchmark deals (`quick`, 20 deals, or `full`, 200) across the threads
    /// and report tricks, nodes and time. The same deals as the benchmark
    /// page at bridge-craftwork.com/bridge-solver/bench, node for node.
    #[arg(
        long = "bench",
        value_name = "PRESET",
        num_args = 0..=1,
        default_missing_value = "full",
        conflicts_with_all = ["input", "output", "in_place"]
    )]
    bench: Option<String>,

    /// With --bench, print the results as JSON.
    #[arg(long = "json", requires = "bench")]
    json: bool,
}

fn main() {
    let args = Args::parse();

    if let Some(preset) = &args.bench {
        if let Err(e) = bench::run(preset, args.threads, args.json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Settled before any file is read, so a bad engine or library fails fast.
    let engine = match Engine::from_name(&args.engine, args.dds_lib.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let files = match collect_inputs(&args.input) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if files.is_empty() {
        eprintln!("Error: no .pbn files found in the given input(s)");
        std::process::exit(1);
    }
    if files.len() > 1 && !args.in_place {
        eprintln!(
            "Error: {} input files matched; use --in-place to annotate them, \
             or name a single file with --output",
            files.len()
        );
        std::process::exit(1);
    }

    let threads = args
        .threads
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
        .max(1);

    // Read and index every input before solving any of it. The solve is one
    // batch across all of them, so a directory of one-deal files spreads over
    // the threads exactly as well as a single file of many deals does.
    let mut documents = Vec::with_capacity(files.len());
    for path in &files {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error reading input file '{}': {}", path.display(), e);
                std::process::exit(1);
            }
        };
        match PbnDocument::parse(&content) {
            Ok(doc) => documents.push(doc),
            Err(e) => {
                eprintln!("Error reading input file '{}': {}", path.display(), e);
                std::process::exit(1);
            }
        }
    }

    // Which boards need analysis is decided once, before anything is solved, so
    // the "already analysed", "incomplete deal" and `--recalculate` rulings are
    // made by one piece of code and the write-back cannot disagree with them.
    let plans: Vec<Vec<Pending>> = documents
        .iter()
        .map(|doc| boards_to_analyse(doc, args.recalculate, args.verbose))
        .collect();
    let pending: Vec<Hands> = plans.iter().flatten().map(|board| board.hands).collect();

    let items = pending.len() * DENOMINATIONS.len();
    let threads = threads.min(items.max(1));
    if args.verbose {
        eprintln!(
            "{} board(s) to analyse in {} file(s), on {} thread(s)",
            pending.len(),
            files.len(),
            threads
        );
    }

    if args.verbose {
        eprintln!("Engine: {}", engine.label());
    }
    let solved: Vec<DdTable> = if matches!(engine, Engine::Dds(_)) {
        // DDS takes the whole batch and schedules its own threads; --threads,
        // when given, caps them.
        let pbns: Vec<String> = pending.iter().map(Hands::to_pbn).collect();
        let refs: Vec<&str> = pbns.iter().map(String::as_str).collect();
        match engine.solve_tables(&refs, args.threads.unwrap_or(0)) {
            Ok(tables) => tables.iter().map(tricks_to_table).collect(),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    } else if threads > 1 {
        solve_deals_parallel(&pending, threads, args.verbose)
    } else {
        let done = AtomicUsize::new(0);
        pending
            .iter()
            .map(|hands| {
                let table = solve_deal(hands);
                if args.verbose {
                    for _ in 0..DENOMINATIONS.len() {
                        report_progress(&done, items);
                    }
                }
                table
            })
            .collect()
    };

    // Each file is then annotated in place in its document, taking each board's
    // table from the batch.
    let mut tables = solved.into_iter();
    let mut changed = 0usize;
    for ((path, doc), plan) in files.iter().zip(&mut documents).zip(&plans) {
        if args.verbose {
            eprintln!("Processing {}...", path.display());
        }
        for board in plan {
            // The plan is what the batch was solved from, so the two cannot
            // disagree; if they ever did, stopping beats writing a table of
            // zeroes over someone's file.
            let Some(table) = tables.next() else {
                eprintln!("Error: internal mismatch between the analysis and writing passes");
                std::process::exit(1);
            };
            if let Err(e) = annotate(doc, board, &table, args.mark_verified) {
                eprintln!("Error annotating '{}': {}", path.display(), e);
                std::process::exit(1);
            }
        }
        if args.verbose {
            eprintln!("Processed {} deal(s)", plan.len());
        }
        let result = doc.to_pbn();

        if args.in_place {
            // Unchanged files are left untouched so a re-run is a true no-op
            // and does not churn mtimes in a build.
            if !doc.is_modified() {
                continue;
            }
            if let Err(e) = write_atomically(path, &result) {
                eprintln!("Error writing '{}': {}", path.display(), e);
                std::process::exit(1);
            }
            changed += 1;
        } else {
            match args.output {
                Some(ref out) => {
                    if let Err(e) = fs::write(out, &result) {
                        eprintln!("Error writing output file '{out}': {e}");
                        std::process::exit(1);
                    }
                    if args.verbose {
                        eprintln!("Output written to {out}");
                    }
                }
                None => {
                    // A reader that closed early is not a failure. `bridge-solver
                    // -i deals.pbn | head` ends the pipe as soon as head has what
                    // it wants, and a tool in a pipeline exits quietly there —
                    // panicking prints a backtrace over the user's terminal for
                    // something they did on purpose.
                    let mut out = io::stdout().lock();
                    if let Err(e) = out.write_all(result.as_bytes()).and_then(|()| out.flush()) {
                        if e.kind() == io::ErrorKind::BrokenPipe {
                            std::process::exit(0);
                        }
                        eprintln!("Error writing to stdout: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    if args.in_place && args.verbose {
        eprintln!("{changed} of {} file(s) updated", files.len());
    }
}

/// Expand the input arguments into a sorted, de-duplicated list of PBN files.
/// A directory contributes every `*.pbn` beneath it; a file is taken as given,
/// whatever its extension, so an oddly-named file can still be named directly.
fn collect_inputs(inputs: &[String]) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for raw in inputs {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            collect_pbn_files(&path, &mut files)?;
        } else if path.exists() {
            files.push(path);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such file or directory: {raw}"),
            ));
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Recursively gather `*.pbn` under `dir`.
fn collect_pbn_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_pbn_files(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pbn"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Write via a sibling temporary file and rename, so an interrupted run cannot
/// leave a half-written lesson file behind.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let tmp = path.with_extension("pbn.tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// Bridge Composer's [BCFlags] bit meaning "double-dummy data has been
/// verified". Note this records provenance only: no documented BCFlags bit
/// controls whether the DD table is *displayed*, and Bridge Composer does not
/// set this one itself when it computes a table.
const BC_FLAG_DD_VERIFIED: u64 = 0x0008_0000;

/// The tags this binary is responsible for, and the only ones it will move.
///
/// A board is stripped of all four before ours are written, so a board an older
/// build annotated with all four above the auction is re-laid out rather than
/// having its tags replaced where they lie. Anything else the board carries
/// stays exactly where its author put it. A board where removing one of these
/// would take a line we did not write is refused instead; see
/// [`stray_line_under_analysis`].
const ANALYSIS_TAGS: [&str; 4] = [
    "DoubleDummyTricks",
    "OptimumScore",
    "ParContract",
    "OptimumResultTable",
];

/// A board waiting on analysis: which board of its document, the hands to
/// solve, and the vulnerability par will need once the table comes back.
struct Pending {
    /// Index into the document's [`PbnDocument::boards`].
    board: usize,
    hands: Hands,
    vulnerability: Option<Vulnerability>,
}

/// The boards of `doc` that need analysing, in file order.
///
/// This is the whole of the "only what is missing" policy: a board with no
/// `[Deal]`, one whose deal will not parse, one dealt no cards, and — unless
/// `recalculate` — one already carrying a `[DoubleDummyTricks]` tag are all
/// left for [`PbnDocument`] to emit from the original bytes.
///
/// The `[DoubleDummyTricks]` tag is the marker for "already analysed": a board
/// carrying a stray par tag but no DD table has not been, and still gets filled
/// in.
fn boards_to_analyse(doc: &PbnDocument, recalculate: bool, verbose: bool) -> Vec<Pending> {
    let mut pending = Vec::new();
    for board in 0..doc.boards().len() {
        // A block with no [Deal] — a preamble, a Bridge Composer template
        // record — is not something to analyse.
        let Some(deal) = doc.tag(board, "Deal") else {
            continue;
        };

        if !recalculate && doc.tag(board, "DoubleDummyTricks").is_some() {
            if verbose {
                eprintln!("Skipping board that already has analysis");
            }
            continue;
        }

        // A board with no cards — Bridge Composer writes
        // [Deal "N:... ... ... ..."] for auction-only teaching boards — parses
        // fine into empty hands, so completeness is checked too. Annotating one
        // of those would stamp a fabricated all-zero table and a "Pass" par
        // onto a board that has no deal to analyse.
        let Some(hands) = Hands::from_pbn(deal) else {
            continue;
        };
        if !hands.is_complete() {
            if verbose {
                eprintln!("Skipping board with an incomplete deal");
            }
            continue;
        }

        // Replacing a tag takes the lines the document says belong to it. On a
        // board where those are not the lines we wrote, that would delete
        // someone else's, so such a board is left exactly as found.
        if let Some(stray) = stray_line_under_analysis(doc, board) {
            eprintln!(
                "Skipping board {}: {stray}, so replacing the analysis would take that line \
                 with it. Move the tag out of the section by hand and run again.",
                doc.tag(board, "Board").unwrap_or("(unnumbered)")
            );
            continue;
        }

        if verbose {
            eprintln!("Processing deal {}...", pending.len() + 1);
        }
        pending.push(Pending {
            board,
            hands,
            // `bridge_types` owns the spelling table, and is what
            // `bridge-encodings` and `pbn-to-pdf` already parse this tag with.
            // Keeping a private copy here is how this binary came to accept
            // `"N"` and `"E"` — which PBN 2.1 §3.4.10 does not define — while
            // rejecting the `"N-S"` and `"E-W"` that everything else in the
            // family accepts.
            vulnerability: doc
                .tag(board, "Vulnerable")
                .and_then(Vulnerability::from_pbn),
        });
    }
    pending
}

/// A line the board's existing analysis tags own that this binary did not
/// write, described for the message that reports it — or `None` when replacing
/// them would take nothing but themselves.
///
/// PBN 2.1 §5.5 gives a tag every line below it until the next tag pair, so a
/// tag written *into* a section — which a build of this binary before #25 did,
/// putting all four between `[Auction "N"]` and its calls — ends up owning the
/// lines that section was holding. `PbnDocument` follows the standard and takes
/// the whole span, so removing that stale `[OptimumResultTable]` would take the
/// auction's calls with it.
///
/// Repairing such a board means moving lines the document did not offer to
/// move, which is how a `%` directive or a commentary block gets stranded. So
/// it is refused instead, and the board keeps whatever it had. Tracked as
/// bridge-craftwork/bridge-encodings#19; a way to ask for a tag's own rows back
/// while replacing it would let this be a repair again.
///
/// The three one-line tags own no rows at all, so any row under one is a stray.
/// `OptimumResultTable`'s rows are its own only while they read as result rows,
/// which is exactly the question [`is_optimum_result_row`] answers.
fn stray_line_under_analysis(doc: &PbnDocument, board: usize) -> Option<String> {
    for name in ANALYSIS_TAGS {
        let rows = doc.tag_rows(board, name);
        let stray = if name == "OptimumResultTable" {
            rows.into_iter().find(|row| !is_optimum_result_row(row))
        } else {
            rows.into_iter().next()
        };
        if let Some(row) = stray {
            return Some(format!("its [{name}] is followed by {row:?}"));
        }
    }
    None
}

/// Write one board's analysis into `doc`, leaving every other byte alone.
///
/// The four tags are removed and then set rather than set in place, because
/// where they go is part of the answer: [`PbnDocument`] ranks a new tag the way
/// the standard's export order does, which `fixtures/bridge-composer` confirms
/// is what Bridge Composer does — the one-line summaries alphabetically among
/// the supplemental tag pairs above `[Auction]`, and the twenty-row table with
/// the supplemental sections below `[Auction]` and `[Play]`. A board an older
/// build left with all four above the auction is repaired by the round trip.
///
/// Nothing about how a PBN file is shaped is decided here: line endings, where
/// a record ends, which lines belong to a section header, and the round trip of
/// `%` directives, `;` comments and `{...}` commentary are all
/// [`PbnDocument`]'s, and re-setting a tag to the value it already holds leaves
/// [`PbnDocument::is_modified`] false so a re-run writes nothing.
fn annotate(
    doc: &mut PbnDocument,
    board: &Pending,
    table: &DdTable,
    mark_verified: bool,
) -> bridge_encodings::Result<()> {
    let at = board.board;

    if mark_verified {
        // Fold the verified bit into whatever the board already carried. An
        // unparsable value is replaced rather than propagated, since the tag is
        // meaningless if it is not hex.
        let flags = doc
            .tag(at, "BCFlags")
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
            .unwrap_or(0);
        doc.set_tag(at, "BCFlags", &format!("{:x}", flags | BC_FLAG_DD_VERIFIED))?;
    }

    for name in ANALYSIS_TAGS {
        doc.remove_tag(at, name)?;
    }

    doc.set_tag(at, "DoubleDummyTricks", &dd_table_to_pbn(table))?;

    // Par needs vulnerability to score; without it the board gets a table and
    // no par, which is also what a board with no [Vulnerable] tag gets — and
    // the stale par tags the board arrived with have already been removed.
    if let Some(vul) = board.vulnerability {
        let scored = par(
            table,
            vul.is_vulnerable(Direction::North),
            vul.is_vulnerable(Direction::East),
        );
        doc.set_tag(at, "OptimumScore", &scored.optimum_score())?;
        if let Some(contracts) = scored.par_contract() {
            doc.set_tag(at, "ParContract", &contracts)?;
        }
    }

    // The `Result` column is one character wide when no declarer takes ten
    // tricks and two when one does — header and rows together, both from
    // `bridge_encodings::pbn`, which is what Bridge Composer writes. A fixed
    // `\2R` had every single-digit board's table rewritten the moment someone
    // opened and saved the file there.
    let rows = optimum_result_table_rows(table);
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    doc.set_section(
        at,
        "OptimumResultTable",
        &optimum_result_table_header(table),
        &rows,
    )
}

/// The five strains, in the order this binary hands them out as work.
///
/// A strain is the unit of parallel work, and the smallest one that does not
/// change the search: its four declarers share one pair of caches and a chain
/// of MTD(f) seeds, and nothing crosses the boundary from one strain to the
/// next. [`TableSolver::solve_strain_hands`] is that unit, so this array is
/// only a work order — any order gives the same table.
const DENOMINATIONS: [Strain; 5] = [
    Strain::NoTrump,
    Strain::Spades,
    Strain::Hearts,
    Strain::Diamonds,
    Strain::Clubs,
];

/// Place one strain's four solved cells into `table`.
///
/// [`TableSolver::solve_strain_hands`] returns its column in `N, E, S, W`
/// order — [`Direction::to_index`], which is the order
/// [`bridge_types::DECLARERS`] lists — so the two are zipped rather than
/// transposed by hand. `solve_deal_matches_the_library_table` is the guard
/// that they still agree.
/// A table from an engine's `[seat][strain]` rows.
fn tricks_to_table(tricks: &Tricks) -> DdTable {
    let (seats, strains) = dds_engine::table_order();
    let mut table = DdTable::new();
    for (seat, row) in seats.into_iter().zip(tricks) {
        for (strain, &n) in strains.into_iter().zip(row) {
            table.set(seat, strain, n);
        }
    }
    table
}

fn place_column(table: &mut DdTable, strain: Strain, column: [u8; 4]) {
    for (declarer, tricks) in DECLARERS.into_iter().zip(column) {
        table.set(declarer, strain, tricks);
    }
}

/// Note one finished work item and, at each ten percent, say so.
///
/// The decile test is true for exactly one value of `n`, so exactly one thread
/// prints each line however many are running and in whatever order they finish.
fn report_progress(done: &AtomicUsize, items: usize) {
    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
    if items > 0 && n * 10 / items != (n - 1) * 10 / items {
        eprintln!("  {}% ({n} of {items} strains)", n * 100 / items);
    }
}

/// Solve a deal and return its DD table, on the calling thread.
fn solve_deal(hands: &Hands) -> DdTable {
    let mut solver = TableSolver::new();
    let mut table = DdTable::new();
    for strain in DENOMINATIONS {
        place_column(
            &mut table,
            strain,
            solver.solve_strain_hands(*hands, strain),
        );
    }
    table
}

/// Solve every deal in `deals` across `threads` workers, returning one table per
/// deal in the order given.
///
/// Work is handed out one (deal, strain) pair at a time from a shared counter,
/// hardest-looking deals first ([`Hands::hardest_first`]),
/// so a thread that draws a cheap strain comes straight back for another. That
/// matters because deal cost spans roughly tenfold: scheduling whole deals ends
/// the run when the slowest deal ends, with most threads long since idle, which
/// is the load-imbalance signature `bench/comparison/RESULTS.md` measured.
///
/// Each worker accumulates its own results and they are merged after the join,
/// so the only state shared between threads is the counter, and the merge is by
/// index — the output is identical whatever order the work completed in.
///
/// A worker keeps one [`TableSolver`] for its whole run rather than building a
/// pair of caches per strain. `CutoffCache::new(16)` alone is a megabyte, and
/// both caches then double their way up to whatever the deal wants; a solver
/// held across items keeps the grown capacity and resets the entries, which is
/// what the C++ reference does with its process globals.
fn solve_deals_parallel(deals: &[Hands], threads: usize, verbose: bool) -> Vec<DdTable> {
    let items = deals.len() * DENOMINATIONS.len();
    // Hardest-looking deals first, so a slow one cannot start last.
    let order = Hands::hardest_first(deals);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);

    let harvest = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut solver = TableSolver::new();
                    let mut mine: Vec<(usize, usize, [u8; 4])> = Vec::new();
                    loop {
                        let item = next.fetch_add(1, Ordering::Relaxed);
                        if item >= items {
                            break;
                        }
                        let (deal_idx, denom_idx) = (
                            order[item / DENOMINATIONS.len()],
                            item % DENOMINATIONS.len(),
                        );
                        let column =
                            solver.solve_strain_hands(deals[deal_idx], DENOMINATIONS[denom_idx]);
                        mine.push((deal_idx, denom_idx, column));
                        if verbose {
                            report_progress(&done, items);
                        }
                    }
                    mine
                })
            })
            .collect();

        workers.into_iter().map(|w| w.join()).collect::<Vec<_>>()
    });

    let mut tables = vec![DdTable::new(); deals.len()];
    for worker in harvest {
        // A worker only ends by panicking, which has already printed its own
        // message; carrying on would silently write a table of zeroes.
        let Ok(found) = worker else {
            eprintln!("Error: a solver thread panicked; no files were written");
            std::process::exit(1);
        };
        for (deal_idx, denom_idx, column) in found {
            place_column(&mut tables[deal_idx], DENOMINATIONS[denom_idx], column);
        }
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Annotate a whole file, solving each deal inline on the calling thread.
    ///
    /// `main` does the same three steps — index, solve, annotate — with the
    /// solve batched across every file and spread over the threads, so this is
    /// that path with the batching taken out.
    fn process_pbn(content: &str, verbose: bool, recalculate: bool, mark_verified: bool) -> String {
        let mut doc = PbnDocument::parse(content).expect("the fixture parses");
        for board in boards_to_analyse(&doc, recalculate, verbose) {
            let table = solve_deal(&board.hands);
            annotate(&mut doc, &board, &table, mark_verified).expect("the tags are writable");
        }
        doc.to_pbn()
    }

    /// The tag name of a line, for reading an annotated file back. The binary
    /// itself no longer needs this: `PbnDocument` indexes the tags.
    fn extract_tag_name(line: &str) -> Option<&str> {
        let rest = line.trim().strip_prefix('[')?;
        let end = rest.find([' ', ']'])?;
        Some(&rest[..end])
    }

    /// Whether `[TagName ...]` opens a section whose data lines follow it, per
    /// PBN 2.1 §5.5, §5.6 and §7. Used only to read an annotated file back.
    fn starts_a_section(tag_name: &str) -> bool {
        tag_name == "Auction" || tag_name == "Play" || tag_name.ends_with("Table")
    }

    /// The 15 tag pairs PBN 2.1 §3.4 requires of every game, for telling a
    /// mandatory tag from a supplemental one when reading a result back.
    const MANDATORY_TAGS: [&str; 15] = [
        "Event",
        "Site",
        "Date",
        "Board",
        "West",
        "North",
        "East",
        "South",
        "Dealer",
        "Vulnerable",
        "Deal",
        "Scoring",
        "Declarer",
        "Contract",
        "Result",
    ];

    /// The `[Vulnerable]` value a board carries, read the way the binary reads
    /// it: `PbnDocument` finds the tag, `bridge_types` spells it.
    fn vulnerability_of(value: &str) -> Option<Vulnerability> {
        let pbn = format!("[Board \"1\"]\n[Vulnerable \"{value}\"]\n");
        let doc = PbnDocument::parse(&pbn).expect("the board parses");
        doc.tag(0, "Vulnerable").and_then(Vulnerability::from_pbn)
    }

    /// The deal string a board carries, as the binary reads it.
    #[test]
    fn the_deal_tag_is_read_as_written() {
        let deal = "N:AK.QJ.T9.8765 432.A.K.QJT94 QJT.KT9.QJ.AK3 9876.8765.A8765.2";
        let pbn = format!("[Board \"1\"]\n[Deal \"{deal}\"]\n");
        let doc = PbnDocument::parse(&pbn).expect("the board parses");
        assert_eq!(doc.tag(0, "Deal"), Some(deal));
        assert_eq!(doc.tag(0, "Event"), None);
    }

    /// Every spelling PBN 2.1 §3.4.10 defines, and nothing else.
    #[test]
    fn test_extract_vulnerability() {
        use Vulnerability::{Both, EastWest, None as NoneVul, NorthSouth};
        for (value, expected) in [
            ("None", NoneVul),
            ("Love", NoneVul),
            ("-", NoneVul),
            ("NS", NorthSouth),
            ("EW", EastWest),
            ("All", Both),
            ("Both", Both),
        ] {
            assert_eq!(vulnerability_of(value), Some(expected), "{value}");
        }
    }

    /// Case is not significant, which the spec's own mixed-case examples imply.
    #[test]
    fn vulnerability_is_case_insensitive() {
        assert_eq!(vulnerability_of("none"), Some(Vulnerability::None));
        assert_eq!(vulnerability_of("bOtH"), Some(Vulnerability::Both));
    }

    /// Gained by moving to `bridge_types`. Not in the spec, but every other
    /// crate in the family accepts them, and this binary used to be the one
    /// that silently emitted no par contract for a board written this way.
    #[test]
    fn vulnerability_accepts_the_hyphenated_forms() {
        assert_eq!(vulnerability_of("N-S"), Some(Vulnerability::NorthSouth));
        assert_eq!(vulnerability_of("E-W"), Some(Vulnerability::EastWest));
    }

    /// Lost by moving to `bridge_types`, deliberately: PBN 2.1 §3.4.10 does not
    /// define bare `"N"` or `"E"`, and nothing here ever produced them. An
    /// unrecognised value means "no vulnerability stated", so such a board keeps
    /// its double-dummy table and simply gets no par — the same treatment as a
    /// board with no `[Vulnerable]` tag at all.
    #[test]
    fn vulnerability_rejects_undefined_spellings() {
        for value in ["N", "E", "S", "W", "NorthSouth", ""] {
            assert_eq!(vulnerability_of(value), Option::None, "{value}");
        }
    }

    #[test]
    fn test_extract_tag_name() {
        assert_eq!(extract_tag_name("[Event \"Test\"]"), Some("Event"));
        assert_eq!(
            extract_tag_name("[OptimumResultTable \"...\"]"),
            Some("OptimumResultTable")
        );
        assert_eq!(extract_tag_name("[Deal \"N:...\"]"), Some("Deal"));
        assert_eq!(extract_tag_name("N NT 3"), None);
    }

    /// A table this binary would have written before the codec moved, checked
    /// against the value Bridge Composer writes for it. This is the guard that
    /// the shared codec's row and column orders are the ones this CLI has
    /// always emitted: `N,S,E,W` by row and `NT,S,H,D,C` by column.
    #[test]
    fn test_encode_ddt() {
        // 0-9 -> '0'-'9', 10-13 -> 'a'-'d'. From Bridge Composer:
        // "32691326914a74a4a74a".
        let rows: [(Direction, [u8; 5]); 4] = [
            (Direction::North, [3, 2, 6, 9, 1]),  // NT=3 S=2 H=6 D=9 C=1
            (Direction::South, [3, 2, 6, 9, 1]),  // same
            (Direction::East, [4, 10, 7, 4, 10]), // NT=4 S=10 H=7 D=4 C=10
            (Direction::West, [4, 10, 7, 4, 10]), // same
        ];
        let mut table = DdTable::new();
        for (declarer, cells) in rows {
            for (strain, tricks) in DENOMINATIONS.iter().zip(cells) {
                table.set(declarer, *strain, tricks);
            }
        }
        assert_eq!(dd_table_to_pbn(&table), "32691326914a74a4a74a");
    }

    #[test]
    fn test_process_simple_pbn() {
        // Use a real 52-card deal from Bridge Composer reference
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[Dealer "N"]
"#;
        let result = process_pbn(pbn, false, false, false);
        assert!(result.contains("[DoubleDummyTricks"));
        assert!(result.contains("[OptimumResultTable"));
        assert!(result.contains("N NT"));
    }

    #[test]
    fn test_replaces_existing_dd_tags() {
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[DoubleDummyTricks "00000000000000000000"]
[OptimumScore "NS 0"]
[ParContract "NS Pass"]
[OptimumResultTable "Declarer;Denomination\2R;Result\2R"]
N NT  0
N  S  0
N  H  0
N  D  0
N  C  0
S NT  0
S  S  0
S  H  0
S  D  0
S  C  0
E NT  0
E  S  0
E  H  0
E  D  0
E  C  0
W NT  0
W  S  0
W  H  0
W  D  0
W  C  0
[Dealer "N"]
"#;
        let result = process_pbn(pbn, false, true, false);
        // Each tag we generate must appear exactly once: the stale copy is
        // stripped and replaced, not duplicated. `Vulnerable` is present, so
        // the par tags are generated too.
        assert_eq!(result.matches("[DoubleDummyTricks").count(), 1);
        assert_eq!(result.matches("[OptimumResultTable").count(), 1);
        assert_eq!(result.matches("[OptimumScore").count(), 1);
        assert_eq!(result.matches("[ParContract").count(), 1);
        // ...and carry recomputed values, not the placeholders from the input.
        assert!(!result.contains("\"00000000000000000000\""));
        assert!(result.contains(r#"[DoubleDummyTricks "9a8789a8784346543465"]"#));
        assert!(result.contains(r#"[OptimumScore "NS 420"]"#));
        assert!(result.contains(r#"[ParContract "NS 4S="]"#));
    }

    /// The default is to fill in only what is missing: a board that already
    /// carries a DD table is passed through byte-for-byte, however stale its
    /// values, so a builder can point the tool at a whole collection safely.
    #[test]
    fn test_existing_analysis_is_kept_unless_recalculating() {
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[DoubleDummyTricks "00000000000000000000"]
[Dealer "N"]
"#;
        let kept = process_pbn(pbn, false, false, false);
        assert_eq!(kept, pbn, "default must not touch an analyzed board");

        let redone = process_pbn(pbn, false, true, false);
        assert!(redone.contains(r#"[DoubleDummyTricks "9a8789a8784346543465"]"#));
    }

    /// A board holding a par tag but no DD table has not been analyzed, so the
    /// default still fills it in — and replaces the orphaned par value.
    #[test]
    fn test_par_tag_alone_does_not_count_as_analyzed() {
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[OptimumScore "NS 9999"]
[Dealer "N"]
"#;
        let result = process_pbn(pbn, false, false, false);
        assert_eq!(result.matches("[DoubleDummyTricks").count(), 1);
        assert!(!result.contains("NS 9999"));
    }

    /// --mark-verified folds bit 0x00080000 into the board's existing BCFlags
    /// without disturbing the bits it already carried.
    #[test]
    fn test_mark_verified_preserves_other_bcflags_bits() {
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[BCFlags "40001f"]
"#;
        let result = process_pbn(pbn, false, false, true);
        // 0x40001f | 0x80000 == 0x48001f — every original bit survives.
        assert!(
            result.contains(r#"[BCFlags "48001f"]"#),
            "got:
{result}"
        );
        assert_eq!(result.matches("[BCFlags").count(), 1);

        // Without the flag the tag is left exactly as written.
        let plain = process_pbn(pbn, false, false, false);
        assert!(plain.contains(r#"[BCFlags "40001f"]"#));
    }

    /// A board with no BCFlags of its own gets one carrying just that bit.
    #[test]
    fn test_mark_verified_adds_bcflags_when_absent() {
        let pbn = r#"[Event "Test"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
"#;
        let result = process_pbn(pbn, false, false, true);
        assert!(
            result.contains(r#"[BCFlags "80000"]"#),
            "got:
{result}"
        );
        assert_eq!(result.matches("[BCFlags").count(), 1);
    }

    /// A board with no cards must be left exactly as found. BridgeComposer
    /// writes `[Deal "N:... ... ... ..."]` for auction-only teaching boards;
    /// those parse into empty hands, and annotating one would stamp a
    /// fabricated all-zero table and a "Pass" par onto a board with no deal.
    #[test]
    fn test_placeholder_deals_pass_through_untouched() {
        let pbn = r#"[Event "Test"]
[Board "1"]
[Vulnerable "None"]
[Deal "N:... ... ... ..."]
[Auction "N"]
1S Pass 2S AP
"#;
        let result = process_pbn(pbn, false, false, false);
        assert_eq!(result, pbn, "placeholder board must be byte-identical");
        assert!(!result.contains("DoubleDummyTricks"));
        assert!(!result.contains("OptimumScore"));
    }

    /// A file mixing real and placeholder boards annotates only the real ones.
    #[test]
    fn test_annotates_only_complete_deals_in_mixed_file() {
        let pbn = r#"[Event "Test"]
[Board "1"]
[Vulnerable "None"]
[Deal "N:... ... ... ..."]

[Event "Test"]
[Board "2"]
[Vulnerable "None"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
"#;
        let result = process_pbn(pbn, false, false, false);
        assert_eq!(result.matches("[DoubleDummyTricks").count(), 1);
        assert!(!result.contains("\"00000000000000000000\""));
    }

    /// A complete board with an auction, for the placement tests. `AP` ends the
    /// auction, so the calls are the whole of the `[Auction]` section.
    const AUCTION_BOARD: &str = concat!(
        "[Event \"Rich\"]\n",
        "[Board \"4\"]\n",
        "[Vulnerable \"EW\"]\n",
        "[Deal \"N:Q7432.85.J983.63 J65.64.AKT5.AK98 AK98.AKQJ7.6.QJ7 T.T932.Q742.T542\"]\n",
        "[Auction \"N\"]\n",
        "1S Pass 2S AP\n",
        "[Play \"E\"]\n",
        "HA H2 H3 H4\n",
    );

    /// The index of the first line equal to `wanted`.
    fn line_of(text: &str, wanted: &str) -> usize {
        text.lines()
            .position(|l| l == wanted)
            .unwrap_or_else(|| panic!("no {wanted:?} line in:\n{text}"))
    }

    /// Issue #22, and then the Bridge Composer fixture. `[Auction]` and
    /// `[Play]` own every line below them until the next tag pair, so nothing
    /// may be inserted between a header and its data — ranking the insertion
    /// point alphabetically once put the whole twenty-row table between
    /// `[Auction "N"]` and its calls. The tags then went *above* the auction,
    /// all four of them, until `fixtures/bridge-composer` showed that Bridge
    /// Composer splits them: one-liners above, the table below the game record.
    #[test]
    fn one_liners_go_above_the_auction_and_the_table_below_the_play() {
        let result = process_pbn(AUCTION_BOARD, false, false, false);
        let lines: Vec<&str> = result.lines().collect();

        // Each section header is still followed immediately by its own data.
        let auction = line_of(&result, "[Auction \"N\"]");
        assert_eq!(lines[auction + 1], "1S Pass 2S AP");
        let play = line_of(&result, "[Play \"E\"]");
        assert_eq!(lines[play + 1], "HA H2 H3 H4");

        // The one-line tags sit between the deal and the auction...
        let deal = lines
            .iter()
            .position(|l| l.starts_with("[Deal "))
            .unwrap_or_else(|| panic!("no deal in:\n{result}"));
        for tag in ["[DoubleDummyTricks", "[OptimumScore", "[ParContract"] {
            let at = lines
                .iter()
                .position(|l| l.starts_with(tag))
                .unwrap_or_else(|| panic!("no {tag} in:\n{result}"));
            assert!(
                at > deal && at < auction,
                "{tag} at {at}, deal at {deal}, auction at {auction}"
            );
        }

        // ...and the table, with its twenty rows, below the play.
        let table = line_of(
            &result,
            "[OptimumResultTable \"Declarer;Denomination\\2R;Result\\2R\"]",
        );
        assert!(table > play, "table at {table}, play at {play}");
        assert_eq!(lines.len() - table - 1, 20, "got:\n{result}");
    }

    /// Group 2 of Bridge Composer's layout: supplemental tag *pairs*, sorted
    /// alphabetically among themselves, custom tags included. Board 7 of
    /// `fixtures/bridge-composer` proves it with `AAACustom` and `ZZZCustom`
    /// bracketing the analysis, so ours has to sort into the same place.
    #[test]
    fn the_one_liners_sort_among_the_boards_own_supplemental_tags() {
        let pbn = concat!(
            "[Board \"1\"]\n",
            "[Vulnerable \"None\"]\n",
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]\n",
            "[Result \"\"]\n",
            "[AAACustom \"first\"]\n",
            "[Generator \"between the analysis tags\"]\n",
            "[ZZZCustom \"last\"]\n",
        );
        let result = process_pbn(pbn, false, false, false);
        let names: Vec<&str> = result
            .lines()
            .filter_map(|l| extract_tag_name(l.trim()))
            .filter(|n| !MANDATORY_TAGS.contains(n))
            .collect();
        assert_eq!(
            names,
            [
                "AAACustom",
                "DoubleDummyTricks",
                "Generator",
                "OptimumScore",
                "ParContract",
                "ZZZCustom",
                "OptimumResultTable",
            ],
            "got:\n{result}"
        );
    }

    /// Group 5: supplemental *sections*, below the game record and sorted
    /// alphabetically among themselves. `AAATable` was written above the
    /// auction on board 8 of the fixture and came back below it, so the game
    /// record outranks the sort — and `ZZZTable` keeps ours above it.
    #[test]
    fn the_table_sorts_among_the_boards_own_sections() {
        let pbn = concat!(
            "[Board \"1\"]\n",
            "[Vulnerable \"None\"]\n",
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]\n",
            "[AAATable \"Declarer;Result\\2R\"]\n",
            "N  1\n",
            "[Auction \"N\"]\n",
            "1S Pass 2S AP\n",
            "[ZZZTable \"Declarer;Result\\2R\"]\n",
            "S  2\n",
        );
        let result = process_pbn(pbn, false, false, false);
        let sections: Vec<&str> = result
            .lines()
            .filter_map(|l| extract_tag_name(l.trim()))
            .filter(|n| starts_a_section(n))
            .collect();
        assert_eq!(
            sections,
            ["AAATable", "Auction", "OptimumResultTable", "ZZZTable"],
            "got:\n{result}"
        );
        // Each header still owns its own rows.
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(
            lines[line_of(&result, "N  1") - 1],
            "[AAATable \"Declarer;Result\\2R\"]"
        );
        assert_eq!(
            lines[line_of(&result, "S  2") - 1],
            "[ZZZTable \"Declarer;Result\\2R\"]"
        );
    }

    /// The Bridge Composer oracle, end to end: annotate the fixture ourselves
    /// and compare against the file Bridge Composer 5.118.2 produced from the
    /// same input, board by board.
    ///
    /// Equality of the whole file is not the goal and never will be — Bridge
    /// Composer reorders the mandatory tags, adds `[BCFlags]` and a preamble of
    /// its own settings, and rewrites `;` comments as `{...}` commentary, none
    /// of which we do. What must agree is everything we write: all four tag
    /// values, the header's `Result` width, the twenty cells, and where the
    /// tags sit relative to `[Auction]` and `[Play]`.
    ///
    /// The table is compared byte for byte, header and rows: Bridge Composer
    /// narrows the data rows along with the header, so a `Result\1R` board
    /// reads `N NT 5` and a `\2R` board `N NT  9`, and
    /// `bridge_encodings::pbn` derives both widths from the same table. Four
    /// boards here are narrow and four are wide.
    #[test]
    fn the_bridge_composer_fixture_round_trips() {
        const OURS: &str = include_str!("../../../fixtures/bridge-composer/pbn-order-test.pbn");
        const THEIRS: &str =
            include_str!("../../../fixtures/bridge-composer/pbn-order-test-bc-dd.pbn");

        let annotated = process_pbn(OURS, false, true, false);
        let ours = analysis_of(&annotated);
        let theirs = analysis_of(THEIRS);
        assert_eq!(ours.len(), 8, "expected eight boards, got {}", ours.len());
        assert_eq!(theirs.len(), ours.len());

        for (board, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
            let board = board + 1;
            assert_eq!(ours.tags, theirs.tags, "board {board}");
            assert_eq!(ours.rows, theirs.rows, "board {board}");
            // The table is the last section on the board, so it is below both
            // [Auction] and [Play] and below any section sorting above it.
            assert_eq!(
                ours.sections.last(),
                Some(&"OptimumResultTable"),
                "board {board}"
            );
        }

        // Board 8 is the one place the section order differs, and it is not
        // ours: Bridge Composer moved the board's own `AAATable` from above the
        // auction to below it. We do not move tags we did not write, so ours
        // reads `AAATable, Auction, OptimumResultTable` where theirs reads
        // `Auction, AAATable, OptimumResultTable`. The table is last either way.
        assert_eq!(
            ours[7].sections,
            ["AAATable", "Auction", "OptimumResultTable"]
        );
        assert_eq!(
            theirs[7].sections,
            ["Auction", "AAATable", "OptimumResultTable"]
        );
        for (ours, theirs) in ours.iter().zip(&theirs).take(7) {
            assert_eq!(ours.sections, theirs.sections);
        }
    }

    /// One board's analysis: the part of a PBN record this binary is
    /// responsible for.
    #[derive(Debug, PartialEq, Eq)]
    struct Analysis<'a> {
        /// The analysis tag lines, exactly as written.
        tags: Vec<&'a str>,
        /// The table's rows, as written — column widths included, since those
        /// are Bridge Composer's too.
        rows: Vec<&'a str>,
        /// The board's section headers, in file order.
        sections: Vec<&'a str>,
    }

    /// Every board's [`Analysis`], in file order.
    ///
    /// Boards without a dealt hand (Bridge Composer's template record) are
    /// skipped, so the two files line up board for board.
    fn analysis_of(pbn: &str) -> Vec<Analysis<'_>> {
        let mut boards = Vec::new();
        // Blank lines separate records; `str::lines` drops the file's CRLF for
        // us, which is all this needs to read either file.
        let mut blocks: Vec<Vec<&str>> = vec![Vec::new()];
        for line in pbn.lines() {
            if line.trim().is_empty() {
                blocks.push(Vec::new());
            } else if let Some(block) = blocks.last_mut() {
                block.push(line);
            }
        }

        for block in blocks {
            // Bridge Composer writes a template record with an empty [Deal];
            // skipping it lines the two files up board for board.
            if !block
                .iter()
                .any(|l| l.starts_with("[Deal \"") && l.len() > 10)
            {
                continue;
            }
            let mut found = Analysis {
                tags: Vec::new(),
                rows: Vec::new(),
                sections: Vec::new(),
            };
            let mut in_table = false;
            for line in block {
                match extract_tag_name(line.trim()) {
                    Some(name) => {
                        in_table = name == "OptimumResultTable";
                        if ["DoubleDummyTricks", "OptimumScore", "ParContract"].contains(&name)
                            || in_table
                        {
                            found.tags.push(line);
                        }
                        if starts_a_section(name) {
                            found.sections.push(name);
                        }
                    }
                    // A `;` comment is not a table row, wherever it sits. The
                    // tags are inserted ahead of any trailing commentary, so a
                    // board whose comment sat below its mandatory tags now has
                    // it below the table rather than above the header.
                    None if in_table && !line.trim_start().starts_with(';') => {
                        found.rows.push(line)
                    }
                    None => {}
                }
            }
            boards.push(found);
        }
        boards
    }

    /// A board with no supplemental tags of its own gets the one-liners in one
    /// alphabetical run, directly below the mandatory tags.
    #[test]
    fn the_one_liners_come_out_in_one_alphabetical_run() {
        let pbn = concat!(
            "[Board \"1\"]\n",
            "[Vulnerable \"None\"]\n",
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]\n",
        );
        let result = process_pbn(pbn, false, false, false);
        let names: Vec<&str> = result
            .lines()
            .filter_map(extract_tag_name)
            .filter(|name| !MANDATORY_TAGS.contains(name))
            .collect();
        assert_eq!(
            names,
            [
                "DoubleDummyTricks",
                "OptimumScore",
                "ParContract",
                "OptimumResultTable",
            ],
            "got:\n{result}"
        );
        assert!(names[..3].windows(2).all(|w| w[0] < w[1]));
    }

    /// Only the placement moved: the same tags with the same values are written.
    #[test]
    fn moving_the_tags_did_not_change_them() {
        let result = process_pbn(AUCTION_BOARD, false, false, false);
        assert!(result.contains(r#"[DoubleDummyTricks "7a9547a9544248942489"]"#));
        assert!(result.contains(r#"[OptimumScore "NS 420"]"#));
        assert!(result.contains(r#"[ParContract "NS 4S="]"#));
    }

    /// A board an older build corrupted — the tags written inside the auction —
    /// is left exactly as found rather than having its auction deleted.
    ///
    /// PBN 2.1 §5.5 gives the stale `[OptimumResultTable]` every line below it
    /// until the next tag pair, and on such a board that is its twenty rows
    /// *and* the auction's calls. Replacing it would take them, so the board is
    /// refused with a message instead. See [`stray_line_under_analysis`].
    #[test]
    fn a_board_whose_tags_are_inside_the_auction_is_left_alone() {
        let mut corrupted = String::new();
        for line in AUCTION_BOARD.lines() {
            corrupted.push_str(line);
            corrupted.push('\n');
            if line == "[Auction \"N\"]" {
                corrupted.push_str("[DoubleDummyTricks \"00000000000000000000\"]\n");
                corrupted.push_str("[OptimumScore \"NS 0\"]\n");
                corrupted.push_str("[ParContract \"NS Pass\"]\n");
                corrupted
                    .push_str("[OptimumResultTable \"Declarer;Denomination\\2R;Result\\2R\"]\n");
                for declarer in ["N", "S", "E", "W"] {
                    for strain in ["NT", " S", " H", " D", " C"] {
                        corrupted.push_str(&format!("{declarer} {strain}  0\n"));
                    }
                }
            }
        }

        let result = process_pbn(&corrupted, false, true, false);
        assert_eq!(result, corrupted, "the board must come back byte-for-byte");
        // In particular the calls are still there, twenty rows below the stale
        // table's header, where the older build stranded them.
        let lines: Vec<&str> = result.lines().collect();
        let table = line_of(
            &result,
            "[OptimumResultTable \"Declarer;Denomination\\2R;Result\\2R\"]",
        );
        assert_eq!(lines[table + 21], "1S Pass 2S AP");

        // A board whose analysis owns only its own lines is still re-laid out.
        let clean = process_pbn(AUCTION_BOARD, false, false, false);
        let redone = process_pbn(&clean, false, true, false);
        assert_eq!(
            redone, clean,
            "re-annotating a well-formed board is a no-op"
        );
    }

    /// Any `*Table` tag is a section header too, per PBN 2.1 §7, and
    /// `"ActionTable" < "DoubleDummyTricks"` — so without the rule its rows
    /// would have been split from their header the same way the auction was.
    #[test]
    fn table_tags_are_section_headers_too() {
        let pbn = concat!(
            "[Board \"1\"]\n",
            "[Vulnerable \"None\"]\n",
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]\n",
            "[ActionTable \"Player;Action\"]\n",
            "N 1S\n",
        );
        let result = process_pbn(pbn, false, false, false);
        let lines: Vec<&str> = result.lines().collect();
        let header = line_of(&result, "[ActionTable \"Player;Action\"]");
        assert_eq!(lines[header + 1], "N 1S");
        assert!(
            lines[..header]
                .iter()
                .any(|l| l.starts_with("[DoubleDummyTricks")),
            "analysis must sit above the table header:\n{result}"
        );
    }

    /// Issue #24. `str::lines` discards the terminator, so rejoining with `\n`
    /// rewrote every line of every CRLF file. Bridge Composer writes CRLF, so
    /// that was every real-world file.
    #[test]
    fn crlf_files_stay_crlf() {
        let pbn = AUCTION_BOARD.replace('\n', "\r\n");
        let result = process_pbn(&pbn, false, false, false);
        assert!(result.contains("[DoubleDummyTricks"));
        assert_eq!(
            result.matches('\n').count(),
            result.matches("\r\n").count(),
            "a bare LF survived:\n{result:?}"
        );
        // The inserted lines took the file's ending, not the compiled-in one.
        assert!(result.contains("[OptimumScore \"NS 420\"]\r\n"));
        assert!(result.contains("N NT  7\r\n"));
    }

    /// An LF file is not "corrected" to CRLF either: each line keeps what it had.
    #[test]
    fn lf_files_stay_lf() {
        let result = process_pbn(AUCTION_BOARD, false, false, false);
        assert!(result.contains("[DoubleDummyTricks"));
        assert_eq!(
            result.matches('\r').count(),
            0,
            "a CR appeared:\n{result:?}"
        );
    }

    /// The reported symptom: a CRLF file that already carries a complete
    /// analysis has nothing to add, so it must come back byte-for-byte — which
    /// is what `main` compares to decide whether to write the file at all.
    /// Before the fix it reported "1 of 1 file(s) updated" and came back LF.
    #[test]
    fn an_already_annotated_crlf_file_is_not_rewritten() {
        let annotated = process_pbn(&AUCTION_BOARD.replace('\n', "\r\n"), false, false, false);
        let again = process_pbn(&annotated, false, false, false);
        assert_eq!(again, annotated, "re-annotating must be a byte-level no-op");
    }

    /// A file whose lines disagree keeps each one as it was written; only the
    /// inserted lines need a choice made for them, and they take the majority.
    #[test]
    fn mixed_endings_are_kept_line_by_line() {
        let pbn = concat!(
            "[Board \"1\"]\r\n",
            "[Vulnerable \"None\"]\n",
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]\r\n",
        );
        let result = process_pbn(pbn, false, false, false);
        assert!(result.starts_with(pbn), "input lines changed:\n{result:?}");
        // Two CRLF against one LF, so insertions are CRLF.
        assert!(result.contains("[DoubleDummyTricks \"9a8789a8784346543465\"]\r\n"));
    }

    /// A file that ended without a newline still ends without one, whether or
    /// not anything was appended to it.
    #[test]
    fn a_missing_final_newline_is_not_added() {
        let deal =
            "[Deal \"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\"]";
        let pbn = format!("[Board \"1\"]\n[Vulnerable \"None\"]\n{deal}");

        // Nothing to add: byte-for-byte, no newline grown.
        let annotated = process_pbn(&pbn, false, false, false);
        assert!(
            !annotated.ends_with('\n'),
            "gained a newline:\n{annotated:?}"
        );
        // The deal line kept a real terminator, so the appended tag is its own
        // line rather than being run onto the end of it.
        assert!(annotated.contains(&format!("{deal}\n[DoubleDummyTricks")));
        assert!(annotated.ends_with("W  C  5"));

        let again = process_pbn(&annotated, false, false, false);
        assert_eq!(again, annotated);
    }

    /// Without a `Vulnerable` tag par cannot be scored, so the par tags are
    /// omitted — and a stale copy in the input is still stripped.
    #[test]
    fn test_strips_par_tags_when_vulnerability_unknown() {
        let pbn = r#"[Event "Test"]
[Deal "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"]
[OptimumScore "NS 0"]
[ParContract "NS Pass"]
[Dealer "N"]
"#;
        let result = process_pbn(pbn, false, false, false);
        assert_eq!(result.matches("[DoubleDummyTricks").count(), 1);
        assert_eq!(result.matches("[OptimumScore").count(), 0);
        assert_eq!(result.matches("[ParContract").count(), 0);
    }

    /// A few deals of varying shape, including one with a void, since voids are
    /// the case where the caches behave differently.
    const SAMPLE_PBN: [&str; 3] = [
        "N:62.JT765.AKJ5.Q3 KQ85.Q9.Q876.J75 J9743.K84.T2.K84 AT.A32.943.AT962",
        "N:Q7432.85.J983.63 J65.64.AKT5.AK98 AK98.AKQJ7.6.QJ7 T.T932.Q742.T542",
        "N:KJ86.KQ9.T3.JT76 QT53.JT74.87.A93 -.A83.AQJ642.K542 A9742.652.K95.Q8",
    ];

    fn sample_deals() -> Vec<Hands> {
        SAMPLE_PBN
            .iter()
            .filter_map(|d| Hands::from_pbn(d))
            .collect()
    }

    /// The threaded path must agree with the serial one cell for cell. It is the
    /// same search either way — a strain's four declarers stay together on one
    /// thread — so this is an equality, not an approximation.
    #[test]
    fn parallel_solve_matches_serial() {
        let deals = sample_deals();
        assert_eq!(deals.len(), 3);

        let serial: Vec<DdTable> = deals.iter().map(solve_deal).collect();

        for threads in [2, 4, 12] {
            let threaded = solve_deals_parallel(&deals, threads, false);
            assert_eq!(threaded, serial, "disagreement on {threads} threads");
        }
    }

    /// Results are placed by index, so they come back in the order the deals
    /// were given however the work was scheduled.
    #[test]
    fn parallel_solve_keeps_deal_order() {
        let deals = sample_deals();
        let forward = solve_deals_parallel(&deals, 8, false);

        let mut reversed_input = deals.clone();
        reversed_input.reverse();
        let mut reversed = solve_deals_parallel(&reversed_input, 8, false);
        reversed.reverse();

        assert_eq!(forward, reversed);
    }

    /// The table this binary assembles must be the library's own table, cell
    /// for cell. `TableSolver::solve_strain_hands` returns a column in
    /// `Direction::to_index` order and `place_column` places it by seat, so a
    /// transposition here would be silent — every cell would still be a real
    /// double-dummy result, just the wrong one's.
    #[test]
    fn solve_deal_matches_the_library_table() {
        for (hands, pbn) in sample_deals().iter().zip(SAMPLE_PBN) {
            let deal = bridge_types::Deal::from_pbn(pbn).expect("sample deal parses");
            let reference = bridge_solver::solve_dd_table(&deal);
            let ours = solve_deal(hands);
            for (declarer, strain, tricks) in ours.cells() {
                assert_eq!(
                    tricks,
                    reference.tricks(declarer, strain),
                    "{declarer:?} in {strain:?} of {pbn}"
                );
            }
        }
    }

    /// The deal strings this binary reads are parsed by `Hands::from_pbn`, and
    /// they stay that way: it accepts PBN that `bridge_types::Deal::from_pbn`
    /// does not, so routing the CLI through a `Deal` to reach `TableSolver`
    /// would have quietly stopped annotating boards it annotates today. That is
    /// why `TableSolver::solve_strain_hands` exists.
    #[test]
    fn the_cli_parser_accepts_more_than_the_typed_one() {
        for lenient in [
            // No leading seat: `Hands::from_pbn` defaults to North.
            "AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72",
            // A void written inside the suit rather than as the whole suit.
            "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.-9653.QJ72",
        ] {
            assert!(Hands::from_pbn(lenient).is_some(), "{lenient}");
            assert!(bridge_types::Deal::from_pbn(lenient).is_none(), "{lenient}");
        }
    }

    /// More threads than there is work to do must still terminate and be right.
    #[test]
    fn parallel_solve_with_more_threads_than_work() {
        let deals = sample_deals();
        let serial: Vec<DdTable> = deals.iter().map(solve_deal).collect();
        let threaded = solve_deals_parallel(&deals, 64, false);
        assert_eq!(threaded, serial);
    }
}
