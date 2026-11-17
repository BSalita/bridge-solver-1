//! A thread that solves and then exits gives its pattern pool back (issue #48).
//!
//! Its own test binary, and one test, because it reads a process-wide
//! counter that other tests solving in parallel would move.

use bridge_solver::{pool_bytes, solve_dd_table, Hands, TableSolver};
use bridge_types::{Deal, Strain};

/// Deals from the lock-step corpus, the first few being enough to build a
/// pool of some megabytes.
const DEALS: &str = include_str!("../fixtures/divergence/lockstep-200.pbn");

fn deals(n: usize) -> Vec<&'static str> {
    DEALS
        .lines()
        .filter(|l| l.starts_with("N:"))
        .take(n)
        .collect()
}

#[test]
fn a_thread_that_solved_gives_its_pool_back_when_it_exits() {
    let before = pool_bytes();

    // The library's free functions, which keep a solver per thread.
    std::thread::spawn(move || {
        for pbn in deals(2) {
            let deal = Deal::from_pbn(pbn).expect("a corpus deal parses");
            solve_dd_table(&deal);
        }
        assert!(pool_bytes() > before, "solving should have filled a pool");
    })
    .join()
    .expect("the solving thread finishes");
    assert_eq!(
        pool_bytes(),
        before,
        "shared solver: pool kept after thread exit"
    );

    // A solver of the thread's own, dropped before the thread ends -- the
    // batch solver's way.
    std::thread::spawn(|| {
        let mut solver = TableSolver::new();
        for pbn in deals(2) {
            let hands = Hands::from_pbn(pbn).expect("a corpus deal parses");
            solver.solve_strain_hands(hands, Strain::NoTrump);
        }
    })
    .join()
    .expect("the solving thread finishes");
    assert_eq!(
        pool_bytes(),
        before,
        "own solver: pool kept after thread exit"
    );

    // Many threads at once, as a batch on fresh threads does.
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                for pbn in deals(1) {
                    let deal = Deal::from_pbn(pbn).expect("a corpus deal parses");
                    solve_dd_table(&deal);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("the solving thread finishes");
    }
    assert_eq!(
        pool_bytes(),
        before,
        "batch of threads: pool kept after exit"
    );
}
