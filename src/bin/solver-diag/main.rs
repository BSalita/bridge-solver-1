//! Rust double-dummy solver CLI with C++ solver-compatible file format and output
//!
//! Usage: solver -f <file> [-X <iterations>] [-P] [-T] [-R] [-V]
//!
//! File format (same as C++ solver):
//!   Line 1: North hand (spades hearts diamonds clubs, space-separated)
//!   Line 2: West hand   East hand (space-separated, aligned)
//!   Line 3: South hand
//!   Line 4: Trump (N/S/H/D/C) - optional, defaults to all 5
//!   Line 5: Leader (W/N/E/S) - optional, defaults to all 4

use bridge_solver::cards::card_of;
use bridge_solver::types::rank_name;
use bridge_solver::{
    set_no_pruning, set_no_rank_skip, set_no_tt, set_show_perf, set_xray_cache_window,
    set_xray_limit, Cards, CutoffCache, Hands, PatternCache, Solver, CLUB, DIAMOND, EAST, HEART,
    NORTH, NOTRUMP, NUM_RANKS, SOUTH, SPADE, WEST,
};
use std::env;
use std::fs;
use std::time::Instant;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Parse arguments
    let mut file_path = None;
    let mut xray_iterations = 0usize;
    let mut cache_spec: (usize, usize, usize) = (0, 0, 0);
    let mut no_pruning = false;
    let mut no_tt = false;
    let mut no_rank_skip = false;
    let mut show_perf = false;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-f" && i + 1 < args.len() {
            file_path = Some(&args[i + 1]);
            i += 2;
        } else if args[i] == "-X" && i + 1 < args.len() {
            xray_iterations = args[i + 1].parse().unwrap_or(0);
            i += 2;
        } else if args[i] == "-C" && i + 1 < args.len() {
            cache_spec = parse_cache_spec(&args[i + 1]);
            i += 2;
        } else if args[i] == "-P" {
            no_pruning = true;
            i += 1;
        } else if args[i] == "-T" {
            no_tt = true;
            i += 1;
        } else if args[i] == "-R" {
            no_rank_skip = true;
            i += 1;
        } else if args[i] == "-V" {
            show_perf = true;
            i += 1;
        } else {
            i += 1;
        }
    }

    let file_path = match file_path {
        Some(p) => p,
        None => {
            eprintln!(
                "Usage: solver -f <file> [-X <iterations>] [-C <interval>] [-P] [-T] [-R] [-V]"
            );
            std::process::exit(1);
        }
    };

    // Set xray limit if specified
    if xray_iterations > 0 {
        set_xray_limit(xray_iterations);
    }

    // Digest the caches over a window, for locating cache drift that the
    // decision trace does not show.
    if cache_spec.2 > 0 {
        set_xray_cache_window(cache_spec.0, cache_spec.1, cache_spec.2);
    }

    // Set no-pruning mode if specified
    if no_pruning {
        set_no_pruning(true);
    }

    // Set no-TT mode if specified
    if no_tt {
        set_no_tt(true);
    }

    // Set show-perf mode if specified
    if show_perf {
        set_show_perf(true);
    }
    let report_footprint = show_perf;

    // Set no-rank-skip mode if specified
    if no_rank_skip {
        set_no_rank_skip(true);
    }

    // Read and parse the file
    let content = fs::read_to_string(file_path).expect("Failed to read file");
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() < 3 {
        eprintln!("Error: File must have at least 3 lines (N, W E, S)");
        std::process::exit(1);
    }

    // Parse hands. West and East may share a line or occupy one each; the
    // reference decides by looking for a four-space gap in the West line, and
    // reads another line when there is none. Its `freak` deals use both forms.
    let north_str = lines[0].trim();
    let (west_str, east_str, south_index) = match split_west_east(lines[1]) {
        Some((west, east)) => (west, east, 2),
        None => {
            if lines.len() < 4 {
                eprintln!(
                    "Error: East is on its own line, so the file needs four hand \
                     lines (N, W, E, S); '{file_path}' has {}.",
                    lines.len()
                );
                std::process::exit(1);
            }
            (lines[1].trim().to_string(), lines[2].trim().to_string(), 3)
        }
    };
    let south_str = lines[south_index].trim();

    let hands = match Hands::from_solver_format(north_str, &west_str, &east_str, south_str) {
        Some(h) => h,
        None => {
            eprintln!(
                "Error: could not read the hands in '{file_path}'.\n\
                 Expected four space-separated suits per hand, high to low, \
                 with '-' for a void:\n\
                 \x20 AQT62 - Q97 AT832\n\
                 Suit symbols are accepted and ignored; 'x' wildcards are not."
            );
            std::process::exit(1);
        }
    };

    // Parse optional trump (line 4)
    let trump: Option<usize> = if lines.len() > south_index + 1 {
        let trump_str = lines[south_index + 1].trim();
        if !trump_str.is_empty() {
            Some(parse_trump(trump_str.chars().next().unwrap()))
        } else {
            None
        }
    } else {
        None
    };

    // Parse optional leader (line 5)
    let leader: Option<usize> = if lines.len() > south_index + 2 {
        let leader_str = lines[south_index + 2].trim();
        if !leader_str.is_empty() {
            Some(parse_seat(leader_str.chars().next().unwrap()))
        } else {
            None
        }
    } else {
        None
    };

    // Print hand diagram
    print_hand_diagram(&hands);

    // Determine what to solve
    let trumps: Vec<usize> = match trump {
        Some(t) => vec![t],
        None => vec![NOTRUMP, SPADE, HEART, DIAMOND, CLUB],
    };

    let leaders: Vec<usize> = match leader {
        Some(l) => vec![l],
        None => vec![WEST, EAST, NORTH, SOUTH],
    };

    let num_tricks = hands.num_tricks();

    // Solve for each trump/leader combination
    // Caches are shared across leaders for the same trump (matching C++ behavior)
    for &t in &trumps {
        let trump_char = trump_to_char(t);

        // Create caches for this trump - they persist across all leaders
        let mut cutoff_cache = CutoffCache::new(16);
        let mut pattern_cache = PatternCache::new(PatternCache::DEFAULT_BITS);

        if leaders.len() == 1 {
            // Single leader - simple output
            let l = leaders[0];
            let start = Instant::now();
            let solver = Solver::new(hands, t, l);
            let ns_tricks = solver.solve_with_caches(&mut cutoff_cache, &mut pattern_cache);
            let elapsed = start.elapsed();
            // Match C++ output: when N/S leads, show total - ns_tricks
            let result = if l == NORTH || l == SOUTH {
                num_tricks as u8 - ns_tricks
            } else {
                ns_tricks
            };
            println!(
                "{}  {}  {:.2} s N/A M",
                trump_char,
                result,
                elapsed.as_secs_f64()
            );
        } else {
            // Multiple leaders - show results in W E N S order on one line
            let mut results = Vec::new();
            let mut total_time = 0.0;

            let mut seed: Option<usize> = None;
            for &l in &leaders {
                let start = Instant::now();
                let solver = Solver::new(hands, t, l);
                let ns_tricks = match seed {
                    Some(g) => {
                        solver.solve_with_caches_seeded(&mut cutoff_cache, &mut pattern_cache, g)
                    }
                    None => solver.solve_with_caches(&mut cutoff_cache, &mut pattern_cache),
                };
                seed = Some(Solver::seed_from(ns_tricks));
                let elapsed = start.elapsed();
                // Match C++ output: when N/S leads, show total - ns_tricks
                let result = if l == NORTH || l == SOUTH {
                    num_tricks as u8 - ns_tricks
                } else {
                    ns_tricks
                };
                results.push(result);
                total_time += elapsed.as_secs_f64();
            }

            println!(
                "{}  {}  {}  {}  {}  {:.2} s N/A M",
                trump_char, results[0], results[1], results[2], results[3], total_time
            );
        }
        if report_footprint {
            let (cap, live, nodes, entry_bytes) = pattern_cache.footprint();
            let table_mb = cap as f64 * entry_bytes as f64 / (1024.0 * 1024.0);
            eprintln!(
                "[FOOTPRINT] pattern cache: capacity {cap} x {entry_bytes} B = {table_mb:.0} MB \
                 table, {live} live entries, {nodes} pattern nodes ({} below the roots, \
                 each owning a separate Vec)",
                nodes.saturating_sub(live)
            );
            let (pat, vec, hands_sz) = bridge_solver::type_sizes();
            eprintln!(
                "[FOOTPRINT] Pattern = {pat} B (Hands {hands_sz} + children {vec} + bounds/padding), \
                 so {nodes} nodes are {:.0} MB of structs alone",
                nodes as f64 * pat as f64 / (1024.0 * 1024.0)
            );
        }
    }
}

/// Parse the West/East line which has both hands separated by 2+ spaces
/// Split a West/East line, or report that East is on the next line.
///
/// The reference looks for four consecutive spaces (or a tab) that are not at
/// the very start of the line: a gap that wide is the space between the two
/// hands of a diagram, while leading spaces are only indentation. When there is
/// no such gap the East hand is on a line of its own, and `None` says so.
fn split_west_east(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\t' && i > 0 {
            return Some((line[..i].trim().to_string(), line[i..].trim().to_string()));
        }
        if bytes[i] == b' ' {
            let start = i;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if start > 0 && i - start >= 4 {
                return Some((
                    line[..start].trim().to_string(),
                    line[i..].trim().to_string(),
                ));
            }
            continue;
        }
        i += 1;
    }
    None
}

fn parse_trump(c: char) -> usize {
    match c.to_ascii_uppercase() {
        'N' => NOTRUMP,
        'S' => SPADE,
        'H' => HEART,
        'D' => DIAMOND,
        'C' => CLUB,
        _ => panic!("Invalid trump: {}", c),
    }
}

fn parse_seat(c: char) -> usize {
    match c.to_ascii_uppercase() {
        'W' => WEST,
        'N' => NORTH,
        'E' => EAST,
        'S' => SOUTH,
        _ => panic!("Invalid seat: {}", c),
    }
}

fn trump_to_char(trump: usize) -> char {
    match trump {
        NOTRUMP => 'N',
        SPADE => 'S',
        HEART => 'H',
        DIAMOND => 'D',
        CLUB => 'C',
        _ => '?',
    }
}

fn print_hand_diagram(hands: &Hands) {
    // Get string representations of each hand
    let n = format_hand_with_suits(hands.hand(NORTH));
    let w = format_hand_with_suits(hands.hand(WEST));
    let e = format_hand_with_suits(hands.hand(EAST));
    let s = format_hand_with_suits(hands.hand(SOUTH));

    // Print in diagram format with Unicode suit symbols
    println!("                          {}", n);
    println!("       {}                  {}", w, e);
    println!("                          {}", s);
}

fn format_hand_with_suits(cards: Cards) -> String {
    let mut parts = Vec::new();

    for (suit, symbol) in [
        (SPADE, "\u{2660}"),
        (HEART, "\u{2665}"),
        (DIAMOND, "\u{2666}"),
        (CLUB, "\u{2663}"),
    ] {
        let suit_cards = cards.suit(suit);
        let suit_str = format_suit_cards(suit_cards, suit);
        parts.push(format!("{} {}", symbol, suit_str));
    }

    parts.join(" ")
}

fn format_suit_cards(cards: Cards, suit: usize) -> String {
    let mut s = String::new();
    // Iterate from Ace down to 2
    for rank in (0..NUM_RANKS).rev() {
        if cards.have(card_of(suit, rank)) {
            s.push(rank_name(rank));
        }
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

/// Parse `-C`: `STEP`, `START:END`, or `START:END:STEP`.
///
/// The three forms are the three stages of a bisection — sweep the whole run
/// coarsely, then every iteration of the interval that disagreed, then narrow.
/// `START:END` defaults to a step of 1 because that is what the last stage
/// wants. Returns `(start, end, step)` with a step of 0 meaning "off".
fn parse_cache_spec(arg: &str) -> (usize, usize, usize) {
    let parts: Vec<usize> = arg
        .split(':')
        .map(|p| p.trim().parse().unwrap_or(0))
        .collect();
    match parts.as_slice() {
        [step] => (0, 0, *step),
        [start, end] => (*start, *end, 1),
        [start, end, step] => (*start, *end, *step),
        _ => (0, 0, 0),
    }
}
