//! The C++ reference (macroxue/bridge-solver) as an engine for
//! `solver-bench engines`, run as a pool of worker processes.
//!
//! The reference keeps its caches in globals and is not thread-safe, so the
//! only way it uses more than one core is more than one process. Each worker
//! is `bench/comparison/cpp-worker.cc` built against the reference's
//! `solver.cc` (see `build-cpp-worker.sh`): it reads one (deal, strain) item
//! per line on stdin and writes back that strain's four declarer results.
//!
//! The scheduling is the native engine's: (deal, strain) items, hardest-looking
//! deals first, each taken by whichever worker is free next. So the two
//! solvers differ in their search and their threads-or-processes, not in how
//! work is handed out. Workers are kept between batches, as the native
//! engine's threads and DDS's thread pool are, so no batch pays process start
//! or cold caches beyond the first.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use bridge_solver::dds_engine::Tricks;
use bridge_types::Deal;

/// Strain letters in a [`Tricks`] table's strain order, C, D, H, S, NT.
const STRAIN_CHARS: [char; 5] = ['C', 'D', 'H', 'S', 'N'];

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn start(path: &Path) -> Result<Self, String> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("starting {}: {e}", path.display()))?;
        let stdin = child.stdin.take().ok_or("cpp-worker has no stdin")?;
        let stdout = child.stdout.take().ok_or("cpp-worker has no stdout")?;
        Ok(Worker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Solve one strain of one deal: tricks for N, E, S, W as declarer.
    fn solve(&mut self, id: usize, strain: char, pbn: &str) -> Result<[u8; 4], String> {
        writeln!(self.stdin, "{id} {strain} {pbn}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("cpp-worker stopped taking work: {e}"))?;
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("reading from cpp-worker: {e}"))?;
        if n == 0 {
            return Err(format!("cpp-worker exited on deal \"{pbn}\""));
        }
        let mut fields = line.split_whitespace();
        if fields.next() != Some(id.to_string().as_str()) {
            return Err(format!(
                "cpp-worker answered out of turn: \"{}\"",
                line.trim()
            ));
        }
        let mut out = [0u8; 4];
        for slot in &mut out {
            *slot = fields
                .next()
                .and_then(|f| f.parse().ok())
                .filter(|&t: &u8| t <= 13)
                .ok_or_else(|| format!("cpp-worker could not solve \"{pbn}\": {}", line.trim()))?;
        }
        Ok(out)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A worker holds nothing worth saving, so it is stopped outright.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The C++ reference, as a pool of `cpp-worker` processes.
pub struct CppEngine {
    path: PathBuf,
    name: Option<String>,
    version: String,
    workers: Mutex<Vec<Worker>>,
}

impl CppEngine {
    /// An engine running the `cpp-worker` executable at `path`, called `name`
    /// in reports when one is given.
    pub fn new(path: &Path, name: Option<String>) -> Result<Self, String> {
        if !path.is_file() {
            return Err(format!(
                "no cpp-worker at {} (build it with bench/comparison/build-cpp-worker.sh)",
                path.display()
            ));
        }
        Ok(CppEngine {
            path: path.to_path_buf(),
            name,
            version: std::env::var("CPP_WORKER_VERSION").unwrap_or_default(),
            workers: Mutex::new(Vec::new()),
        })
    }

    /// The executable this engine runs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name in reports: the name given, else `bridge-solver-c++` with
    /// the reference's commit when `CPP_WORKER_VERSION` gives one.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            name.clone()
        } else if self.version.is_empty() {
            "bridge-solver-c++".into()
        } else {
            format!("bridge-solver-c++ {}", self.version)
        }
    }

    /// Solve full tables for PBN deals, in order, on `processes` workers
    /// (0 = one per core).
    pub fn solve_tables(&self, pbns: &[&str], processes: usize) -> Result<Vec<Tricks>, String> {
        let hands = pbns
            .iter()
            .map(|p| {
                Deal::from_pbn(p)
                    .map(|d| bridge_solver::Hands::from_deal(&d))
                    .ok_or_else(|| format!("could not parse deal \"{p}\""))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let order = bridge_solver::Hands::hardest_first(&hands);
        let items = pbns.len() * STRAIN_CHARS.len();
        let processes = if processes == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            processes
        }
        .min(items.max(1));

        let mut pool = self
            .workers
            .lock()
            .map_err(|_| "the cpp-worker pool was poisoned".to_string())?;
        while pool.len() < processes {
            pool.push(Worker::start(&self.path)?);
        }

        let next = AtomicUsize::new(0);
        let columns: Mutex<Vec<Option<[u8; 4]>>> = Mutex::new(vec![None; items]);
        let failure: Mutex<Option<String>> = Mutex::new(None);
        std::thread::scope(|scope| {
            for worker in pool.iter_mut().take(processes) {
                let (next, columns, failure, order) = (&next, &columns, &failure, &order);
                scope.spawn(move || loop {
                    let item = next.fetch_add(1, Ordering::Relaxed);
                    if item >= items {
                        return;
                    }
                    let (d, s) = (order[item / 5], item % 5);
                    match worker.solve(item, STRAIN_CHARS[s], pbns[d]) {
                        Ok(column) => {
                            if let Ok(mut c) = columns.lock() {
                                c[d * 5 + s] = Some(column);
                            }
                        }
                        Err(e) => {
                            // Stop everyone: the batch has failed.
                            next.store(items, Ordering::Relaxed);
                            if let Ok(mut f) = failure.lock() {
                                f.get_or_insert(e);
                            }
                            return;
                        }
                    }
                });
            }
        });
        if let Some(e) = failure.into_inner().ok().flatten() {
            // A worker that failed may have died or be mid-answer: start afresh.
            pool.clear();
            return Err(e);
        }
        let columns = columns
            .into_inner()
            .map_err(|_| "the cpp-worker results were poisoned".to_string())?;
        let mut out = vec![[[0u8; 5]; 4]; pbns.len()];
        for (i, column) in columns.into_iter().enumerate() {
            let column = column.ok_or("a cpp-worker item went unsolved")?;
            for (seat, tricks) in column.into_iter().enumerate() {
                out[i / 5][seat][i % 5] = tricks;
            }
        }
        Ok(out)
    }
}
