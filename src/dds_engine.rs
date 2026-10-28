//! Double-dummy tables from this crate's solver or from a DDS-compatible
//! shared library loaded at run time.
//!
//! Any build of DDS's C API works: upstream DDS 2.9 (`dds.dll`, `libdds.so`),
//! the `ddss` fork, or anything else exporting `CalcAllTablesPBN`. When the
//! library also exports `CalcAllTablesPBNx` (ddss's batch entry point, which
//! takes any number of deals) it is used; otherwise deals go to
//! `CalcAllTablesPBN` forty at a time, the most DDS accepts for a full table.
//!
//! Nothing is linked at build time, so the feature costs a build nothing and a
//! machine without DDS loses nothing but the option. Loading by path is also
//! what lets one process hold two DDS builds side by side, which is how the
//! benchmark compares them.
//!
//! Tables come back as `[seat][strain]`, seats N, E, S, W and strains
//! C, D, H, S, NT -- the layout of [`crate::json_api::dd_table_tricks`].

use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use bridge_types::{Deal, Direction, Strain};
use libloading::Library;

use crate::par::TableSolver;

/// Environment variable naming the DDS library to load when none is given.
pub const DDS_LIB_ENV: &str = "BRIDGE_SOLVER_DDS_LIB";
/// Also read, after [`DDS_LIB_ENV`]: the variable the ddss tooling already sets.
pub const DDSS_LIB_ENV: &str = "DDSS_DLL_PATH";

/// One table: `[seat][strain]`, seats N, E, S, W and strains C, D, H, S, NT.
pub type Tricks = [[u8; 5]; 4];

const SEATS: [Direction; 4] = [
    Direction::North,
    Direction::East,
    Direction::South,
    Direction::West,
];
const STRAINS: [Strain; 5] = [
    Strain::Clubs,
    Strain::Diamonds,
    Strain::Hearts,
    Strain::Spades,
    Strain::NoTrump,
];

// ---- DDS's C API -----------------------------------------------------------

const DDS_STRAINS: usize = 5;
const DDS_HANDS: usize = 4;
const MAXNOOFTABLES: usize = 40;
/// Success, DDS's `RETURN_NO_FAULT`.
const RETURN_NO_FAULT: c_int = 1;
/// DDS index of each of our strains C, D, H, S, NT (DDS orders S, H, D, C, NT).
const DDS_STRAIN_FOR: [usize; 5] = [3, 2, 1, 0, 4];

#[repr(C)]
#[derive(Clone, Copy)]
struct DdTableDealPbn {
    cards: [c_char; 80],
}

#[repr(C)]
struct DdTableDealsPbn {
    no_of_tables: c_int,
    deals: [DdTableDealPbn; MAXNOOFTABLES * DDS_STRAINS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DdTableResults {
    res_table: [[c_int; DDS_HANDS]; DDS_STRAINS],
}

#[repr(C)]
struct DdTablesRes {
    no_of_boards: c_int,
    results: [DdTableResults; MAXNOOFTABLES * DDS_STRAINS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ParResults {
    par_score: [[c_char; 16]; 2],
    par_contracts_string: [[c_char; 128]; 2],
}

#[repr(C)]
struct AllParResults {
    presults: [ParResults; MAXNOOFTABLES],
}

#[repr(C)]
struct DdsInfo {
    major: c_int,
    minor: c_int,
    patch: c_int,
    version_string: [c_char; 10],
    system: c_int,
    num_bits: c_int,
    compiler: c_int,
    constructor: c_int,
    num_cores: c_int,
    threading: c_int,
    no_of_threads: c_int,
    thread_sizes: [c_char; 128],
    system_string: [c_char; 1024],
}

type CalcAllTablesPbnFn = unsafe extern "C" fn(
    *mut DdTableDealsPbn,
    c_int,
    *mut c_int,
    *mut DdTablesRes,
    *mut AllParResults,
) -> c_int;
type CalcAllTablesPbnxFn = unsafe extern "C" fn(
    c_int,
    *mut DdTableDealPbn,
    c_int,
    *mut c_int,
    *mut DdTableResults,
    *mut ParResults,
) -> c_int;
type SetMaxThreadsFn = unsafe extern "C" fn(c_int);
type GetDdsInfoFn = unsafe extern "C" fn(*mut DdsInfo);

// ---- A loaded library --------------------------------------------------------

/// A DDS-compatible library, loaded once and kept for the life of the process.
///
/// Calls into it are serialised: DDS keeps its thread pool and memory in
/// process globals, so two concurrent `CalcAllTables` calls into one library
/// would share them. DDS parallelises inside each call instead.
pub struct DdsLibrary {
    path: PathBuf,
    calc_all_tables_pbn: CalcAllTablesPbnFn,
    calc_all_tables_pbnx: Option<CalcAllTablesPbnxFn>,
    set_max_threads: Option<SetMaxThreadsFn>,
    get_dds_info: Option<GetDdsInfoFn>,
    /// The thread cap last applied, under the lock every call takes.
    state: Mutex<Option<usize>>,
    /// Owns the code the function pointers above point into. Never dropped:
    /// libraries are cached for the process (see [`DdsLibrary::load`]).
    _library: Library,
}

impl std::fmt::Debug for DdsLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DdsLibrary")
            .field("path", &self.path)
            .field("batch_api", &self.calc_all_tables_pbnx.is_some())
            .finish()
    }
}

/// What a library says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdsInfoSummary {
    /// DDS's version string, e.g. `2.9.0`.
    pub version: String,
    /// Cores DDS saw.
    pub cores: i32,
    /// Threads DDS built.
    pub threads: i32,
    /// Whether the library has ddss's `CalcAllTablesPBNx`.
    pub batch_api: bool,
}

fn libraries() -> &'static Mutex<HashMap<PathBuf, Arc<DdsLibrary>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<DdsLibrary>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl DdsLibrary {
    /// Load the library at `path`, or return the copy already loaded.
    ///
    /// Libraries are never unloaded. DDS starts worker threads that live in
    /// the library's code, and unloading it under them would crash the process.
    pub fn load(path: &Path) -> Result<Arc<Self>, String> {
        let key = path
            .canonicalize()
            .map_err(|e| format!("DDS library {}: {e}", path.display()))?;
        let mut cache = libraries()
            .lock()
            .map_err(|_| "the DDS library cache was poisoned".to_string())?;
        if let Some(lib) = cache.get(&key) {
            return Ok(Arc::clone(lib));
        }
        let mut lib = Self::open(&key)?;
        // Report the path as given; the canonical form is only the cache key
        // (and on Windows carries a `\\?\` prefix nobody wants to read).
        lib.path = path.to_path_buf();
        let lib = Arc::new(lib);
        cache.insert(key, Arc::clone(&lib));
        Ok(lib)
    }

    fn open(path: &Path) -> Result<Self, String> {
        // SAFETY: loading a library runs its initialisers. The caller named
        // this file as a DDS build, which is the trust the option implies.
        let library = unsafe { Library::new(path) }
            .map_err(|e| format!("could not load DDS library {}: {e}", path.display()))?;
        // SAFETY (all four lookups): each signature is DDS's own from its
        // `dll.h`, unchanged since 2.8 for the three standard calls and as
        // ddss documents for `CalcAllTablesPBNx`. The pointers are copied out
        // of their `Symbol`s and stay valid because `library` is stored
        // alongside them and never dropped.
        let calc_all_tables_pbn = unsafe {
            *library
                .get::<CalcAllTablesPbnFn>(b"CalcAllTablesPBN\0")
                .map_err(|e| {
                    format!(
                        "{} is not a DDS library (no CalcAllTablesPBN): {e}",
                        path.display()
                    )
                })?
        };
        let calc_all_tables_pbnx =
            unsafe { library.get::<CalcAllTablesPbnxFn>(b"CalcAllTablesPBNx\0") }
                .ok()
                .map(|s| *s);
        let set_max_threads = unsafe { library.get::<SetMaxThreadsFn>(b"SetMaxThreads\0") }
            .ok()
            .map(|s| *s);
        let get_dds_info = unsafe { library.get::<GetDdsInfoFn>(b"GetDDSInfo\0") }
            .ok()
            .map(|s| *s);
        Ok(Self {
            path: path.to_path_buf(),
            calc_all_tables_pbn,
            calc_all_tables_pbnx,
            set_max_threads,
            get_dds_info,
            state: Mutex::new(None),
            _library: library,
        })
    }

    /// The file this was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this is a ddss build (it has `CalcAllTablesPBNx`).
    pub fn is_ddss(&self) -> bool {
        self.calc_all_tables_pbnx.is_some()
    }

    /// Version, cores and threads, as DDS reports them.
    pub fn info(&self) -> DdsInfoSummary {
        let mut summary = DdsInfoSummary {
            version: "?".into(),
            cores: 0,
            threads: 0,
            batch_api: self.is_ddss(),
        };
        if let Some(get) = self.get_dds_info {
            let Ok(_guard) = self.state.lock() else {
                return summary;
            };
            // SAFETY: GetDDSInfo fills a caller-owned struct and reads nothing
            // from it; the struct is zeroed, laid out as DDS's, and outlives
            // the call.
            let mut info: Box<DdsInfo> = Box::new(unsafe { std::mem::zeroed() });
            unsafe { get(info.as_mut()) };
            let bytes: Vec<u8> = info
                .version_string
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            summary.version = String::from_utf8_lossy(&bytes).trim().to_string();
            if summary.version.is_empty() {
                summary.version = format!("{}.{}.{}", info.major, info.minor, info.patch);
            }
            summary.cores = info.num_cores;
            summary.threads = info.no_of_threads;
        }
        summary
    }

    /// A short name: `ddss 2.9.0` or `dds 2.9.0`.
    pub fn label(&self) -> String {
        let kind = if self.is_ddss() { "ddss" } else { "dds" };
        format!("{kind} {}", self.info().version)
    }

    /// Solve full tables for PBN deals. `threads` caps DDS's thread pool;
    /// 0 lets DDS use every core.
    pub fn solve_tables(&self, pbns: &[&str], threads: usize) -> Result<Vec<Tricks>, String> {
        let canonical = pbns
            .iter()
            .map(|p| canonical_pbn(p))
            .collect::<Result<Vec<_>, _>>()?;
        let mut last = self
            .state
            .lock()
            .map_err(|_| "a previous DDS call panicked".to_string())?;
        // DDS rebuilds its thread memory on every SetMaxThreads, even to the
        // same value, and that rebuild is not always safe to repeat (see
        // `src/bin/solver-bench/dds.rs`). Only call it on a change.
        if *last != Some(threads) {
            if let Some(set) = self.set_max_threads {
                // SAFETY: takes an int by value and sets DDS's globals.
                unsafe { set(threads as c_int) };
            }
            *last = Some(threads);
        }
        let mut out = Vec::with_capacity(pbns.len());
        if let Some(pbnx) = self.calc_all_tables_pbnx {
            solve_batch_pbnx(pbnx, &canonical, &mut out)?;
        } else {
            for chunk in canonical.chunks(MAXNOOFTABLES) {
                solve_chunk_pbn(self.calc_all_tables_pbn, chunk, &mut out)?;
            }
        }
        Ok(out)
    }
}

/// The deal in the exact form DDS reads: `N:` then four hands, empty suits
/// for voids. Also rejects a bad deal here, with a message, rather than as a
/// bare DDS error code.
fn canonical_pbn(pbn: &str) -> Result<Vec<u8>, String> {
    let deal = Deal::from_pbn(pbn).ok_or_else(|| format!("could not parse deal \"{pbn}\""))?;
    let text = deal.to_pbn(Direction::North);
    if text.len() >= 80 {
        return Err(format!(
            "deal string is {} bytes, DDS allows 79",
            text.len()
        ));
    }
    Ok(text.into_bytes())
}

fn fill(slot: &mut DdTableDealPbn, pbn: &[u8]) {
    for (dst, &b) in slot.cards.iter_mut().zip(pbn) {
        *dst = b as c_char;
    }
}

fn to_tricks(res: &DdTableResults) -> Result<Tricks, String> {
    let mut one = [[0u8; 5]; 4];
    for (hand, row) in one.iter_mut().enumerate() {
        for (strain, cell) in row.iter_mut().enumerate() {
            let tricks = res.res_table[DDS_STRAIN_FOR[strain]][hand];
            *cell = u8::try_from(tricks)
                .ok()
                .filter(|&t| t <= 13)
                .ok_or_else(|| format!("DDS returned {tricks} tricks"))?;
        }
    }
    Ok(one)
}

fn solve_chunk_pbn(
    calc: CalcAllTablesPbnFn,
    pbns: &[Vec<u8>],
    out: &mut Vec<Tricks>,
) -> Result<(), String> {
    // Hundreds of kilobytes each, so boxed.
    // SAFETY: plain C structs of integers and chars; all-zero is valid.
    let mut deals: Box<DdTableDealsPbn> = Box::new(unsafe { std::mem::zeroed() });
    let mut res: Box<DdTablesRes> = Box::new(unsafe { std::mem::zeroed() });
    let mut par: Box<AllParResults> = Box::new(unsafe { std::mem::zeroed() });
    for (slot, pbn) in deals.deals.iter_mut().zip(pbns) {
        fill(slot, pbn);
    }
    deals.no_of_tables = pbns.len() as c_int;
    let mut trump_filter = [0 as c_int; DDS_STRAINS];
    // SAFETY: every pointer is to a live, zeroed struct of DDS's layout that
    // outlives the call; `no_of_tables` is at most MAXNOOFTABLES because the
    // caller chunked; each deal is NUL-terminated (zeroed, under 80 bytes);
    // mode -1 means DDS does not write par results.
    let rc = unsafe {
        calc(
            deals.as_mut(),
            -1,
            trump_filter.as_mut_ptr(),
            res.as_mut(),
            par.as_mut(),
        )
    };
    if rc != RETURN_NO_FAULT {
        return Err(format!("DDS CalcAllTablesPBN returned error {rc}"));
    }
    for r in &res.results[..pbns.len()] {
        out.push(to_tricks(r)?);
    }
    Ok(())
}

fn solve_batch_pbnx(
    calc: CalcAllTablesPbnxFn,
    pbns: &[Vec<u8>],
    out: &mut Vec<Tricks>,
) -> Result<(), String> {
    if pbns.is_empty() {
        return Ok(());
    }
    let mut deals = vec![DdTableDealPbn { cards: [0; 80] }; pbns.len()];
    for (slot, pbn) in deals.iter_mut().zip(pbns) {
        fill(slot, pbn);
    }
    let mut res = vec![
        DdTableResults {
            res_table: [[0; DDS_HANDS]; DDS_STRAINS]
        };
        pbns.len()
    ];
    let mut trump_filter = [0 as c_int; DDS_STRAINS];
    let count = c_int::try_from(pbns.len()).map_err(|_| "too many deals for DDS".to_string())?;
    // SAFETY: `deals` and `res` each hold `count` elements of DDS's layout and
    // outlive the call; mode -1 asks for no par, so a null par pointer is
    // never written (ddss's documented contract).
    let rc = unsafe {
        calc(
            count,
            deals.as_mut_ptr(),
            -1,
            trump_filter.as_mut_ptr(),
            res.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if rc != RETURN_NO_FAULT {
        return Err(format!("ddss CalcAllTablesPBNx returned error {rc}"));
    }
    for r in &res {
        out.push(to_tricks(r)?);
    }
    Ok(())
}

// ---- Choosing an engine --------------------------------------------------------

/// Which solver computes tables.
#[derive(Debug, Clone)]
pub enum Engine {
    /// This crate's own solver.
    Native,
    /// A DDS-compatible library.
    Dds(Arc<DdsLibrary>),
}

/// The DDS library named by `BRIDGE_SOLVER_DDS_LIB`, else `DDSS_DLL_PATH`.
pub fn default_dds_lib() -> Option<PathBuf> {
    [DDS_LIB_ENV, DDSS_LIB_ENV]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl Engine {
    /// Pick an engine by name: `native` (or `bridge-solver`), or `dds`
    /// (or `ddss`), which loads `dds_lib`, falling back to
    /// [`default_dds_lib`].
    pub fn from_name(name: &str, dds_lib: Option<&Path>) -> Result<Self, String> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "native" | "bridge-solver" | "bridge_solver" => Ok(Engine::Native),
            "dds" | "ddss" => {
                let path = dds_lib
                    .map(Path::to_path_buf)
                    .or_else(default_dds_lib)
                    .ok_or_else(|| {
                        format!(
                            "engine \"{name}\" needs a DDS library: pass its path, or set \
                         {DDS_LIB_ENV} (or {DDSS_LIB_ENV})"
                        )
                    })?;
                Ok(Engine::Dds(DdsLibrary::load(&path)?))
            }
            other => Err(format!(
                "unknown engine \"{other}\"; use \"native\" or \"dds\""
            )),
        }
    }

    /// A short name for reports: `bridge-solver 2.1.0`, `ddss 2.9.0`.
    pub fn label(&self) -> String {
        match self {
            Engine::Native => format!("bridge-solver {}", env!("CARGO_PKG_VERSION")),
            Engine::Dds(lib) => lib.label(),
        }
    }

    /// Solve full tables for PBN deals, in order, on up to `threads` threads
    /// (0 = every core).
    pub fn solve_tables(&self, pbns: &[&str], threads: usize) -> Result<Vec<Tricks>, String> {
        match self {
            Engine::Native => solve_tables_native(pbns, threads),
            Engine::Dds(lib) => lib.solve_tables(pbns, threads),
        }
    }
}

/// This crate's solver over a batch, one (deal, strain) pair per work item,
/// as the `bridge-solver` CLI schedules it.
pub fn solve_tables_native(pbns: &[&str], threads: usize) -> Result<Vec<Tricks>, String> {
    let deals = pbns
        .iter()
        .map(|p| Deal::from_pbn(p).ok_or_else(|| format!("could not parse deal \"{p}\"")))
        .collect::<Result<Vec<_>, _>>()?;
    let items = deals.len() * STRAINS.len();
    let threads = if threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        threads
    }
    .min(items.max(1));

    let mut out = vec![[[0u8; 5]; 4]; deals.len()];
    if threads <= 1 {
        let mut solver = TableSolver::new();
        for (table, deal) in out.iter_mut().zip(&deals) {
            for (s, &strain) in STRAINS.iter().enumerate() {
                for (seat, tricks) in solver.solve_strain(deal, strain).into_iter().enumerate() {
                    table[seat][s] = tricks;
                }
            }
        }
        return Ok(out);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let harvest: Vec<Vec<(usize, usize, [u8; 4])>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut solver = TableSolver::new();
                    let mut mine = Vec::new();
                    loop {
                        let item = next.fetch_add(1, Ordering::Relaxed);
                        if item >= items {
                            break;
                        }
                        let (d, s) = (item / STRAINS.len(), item % STRAINS.len());
                        mine.push((d, s, solver.solve_strain(&deals[d], STRAINS[s])));
                    }
                    mine
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap_or_default())
            .collect()
    });
    let mut filled = 0usize;
    for (d, s, column) in harvest.into_iter().flatten() {
        for (seat, tricks) in column.into_iter().enumerate() {
            out[d][seat][s] = tricks;
        }
        filled += 1;
    }
    if filled != items {
        return Err("a solver thread panicked".into());
    }
    Ok(out)
}

/// Seats and strains in the order of a [`Tricks`] table.
pub fn table_order() -> ([Direction; 4], [Strain; 5]) {
    (SEATS, STRAINS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEAL: &str = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";

    #[test]
    fn native_batch_matches_single_solves() {
        let deal = Deal::from_pbn(DEAL).expect("deal");
        let table = crate::par::solve_dd_table(&deal);
        let one = SEATS.map(|seat| STRAINS.map(|strain| table.tricks(seat, strain)));
        for threads in [1, 4] {
            let got = solve_tables_native(&[DEAL, DEAL], threads).expect("solve");
            assert_eq!(got, vec![one, one], "threads {threads}");
        }
    }

    #[test]
    fn hands_write_the_pbn_dds_reads() {
        let hands = crate::Hands::from_pbn(DEAL).expect("hands");
        assert_eq!(hands.to_pbn(), DEAL);
        // A lenient spelling -- no seat, `-` for a void -- comes out strict.
        let lenient =
            "AKQJT98765432.-.-.- -.AKQJT98765432.-.- -.-.AKQJT98765432.- -.-.-.AKQJT98765432";
        let strict = crate::Hands::from_pbn(lenient).expect("hands").to_pbn();
        assert_eq!(
            strict,
            "N:AKQJT98765432... .AKQJT98765432.. ..AKQJT98765432. ...AKQJT98765432"
        );
    }

    #[test]
    fn unknown_engine_is_an_error() {
        assert!(Engine::from_name("magic", None).is_err());
        assert!(matches!(
            Engine::from_name("native", None),
            Ok(Engine::Native)
        ));
    }

    /// Runs only where a DDS library is named by `BRIDGE_SOLVER_DDS_LIB`:
    /// every table it returns must equal this crate's, at one thread and many,
    /// across more deals than one `CalcAllTablesPBN` call takes.
    #[test]
    fn dds_library_agrees_with_native() {
        let Some(path) = std::env::var_os(DDS_LIB_ENV) else {
            return;
        };
        let engine = Engine::from_name("dds", Some(Path::new(&path))).expect("load DDS");
        let pbns: Vec<&str> = (0..45).filter_map(crate::dd_bench::deal_pbn).collect();
        let native = solve_tables_native(&pbns, 0).expect("native");
        for threads in [0, 1] {
            let theirs = engine.solve_tables(&pbns, threads).expect("dds");
            assert_eq!(theirs, native, "{} at {threads} threads", engine.label());
        }
    }

    #[test]
    fn bad_deal_is_an_error() {
        assert!(solve_tables_native(&["nonsense"], 1).is_err());
    }
}
