/*
 * bridge_solver.h - C interface to bridge-solver.
 *
 * Double-dummy tables, par, double-dummy play analysis, and claim checking
 * (whether a claim holds however the unseen cards lie), from any language
 * that can call C.
 *
 * Link with -lbridgesolver (libbridgesolver.so / .dylib / .a; bridgesolver.dll
 * with its import library on Windows). The static library also needs the
 * system libraries Rust programs use: on Linux -lgcc_s -lutil -lrt -lpthread
 * -lm -ldl -lc, on macOS nothing extra, on Windows ws2_32 userenv ntdll bcrypt
 * advapi32. See README.md.
 *
 * https://github.com/bridge-craftwork/bridge-solver - MIT OR Apache-2.0.
 *
 * CONVENTIONS
 *
 *  - Strings passed in are NUL-terminated UTF-8.
 *  - A deal is PBN: "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72".
 *    Seats are "N", "E", "S", "W"; strains "C", "D", "H", "S", "NT" (or "N");
 *    cards a suit letter then a rank, e.g. "SA", "HT".
 *  - Functions returning char * return JSON that the caller owns and must
 *    release with bs_free_string(), never free(). They never return NULL.
 *    On failure they return {"error": "<message>"}; a successful response
 *    never has a top-level "error" key.
 *  - Every function may be called from several threads at once, except that
 *    one BsAnalyzer must not be used by two threads at the same time.
 *  - Request and response JSON are the same as the WebAssembly and Python
 *    packages' (see src/json_api.rs in the repository).
 */

#ifndef BRIDGE_SOLVER_H
#define BRIDGE_SOLVER_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The library version, e.g. "2.0.0". Static: do not free it. */
const char *bs_version(void);

/* Release a string returned by this library. NULL is ignored. */
void bs_free_string(char *s);

/* ---- Double dummy ------------------------------------------------------ */

/*
 * The 20-cell double-dummy table:
 *   {"tricks": [[8,7,8,10,9], ...], "total": 13}
 * Rows N, E, S, W; columns C, D, H, S, NT; each the tricks that seat takes
 * as declarer in that strain.
 */
char *bs_dd_table(const char *deal);

/*
 * Many tables at once, by the engine asked for. Request JSON:
 *   {"deals": ["N:...", ...], "engine": "native" | "dds",
 *    "dds_lib": "path/to/dds.dll", "threads": 0}
 * Only "deals" is required. "dds" loads a DDS-compatible library (DDS 2.9,
 * ddss) from "dds_lib", else $BRIDGE_SOLVER_DDS_LIB, else $DDSS_DLL_PATH.
 * Response: {"engine": "...", "tables": [{"tricks": ..., "total": 13}, ...]},
 * each table shaped as bs_dd_table's. Free with bs_free_string.
 */
char *bs_dd_tables(const char *request_json);

/*
 * The same table into tricks[20], row-major: tricks[seat * 5 + strain] with
 * seats N, E, S, W and strains C, D, H, S, NT. Returns 0, or -1 on failure.
 */
int bs_dd_table_tricks(const char *deal, unsigned char tricks[20]);

/*
 * Par, given who is vulnerable: "None", "NS", "EW" or "All".
 *   {"score_ns": 620, "optimum_score": "NS 620", "par_contract": "NS 4S="}
 * par_contract lists every contract tied at par; it is absent when the deal
 * is passed out.
 */
char *bs_par(const char *deal, const char *vulnerable);

/*
 * North-South's double-dummy tricks with `leader` on lead in `trump`, or -1
 * on failure.
 */
int bs_solve(const char *deal, const char *trump, const char *leader);

/* ---- Claims ------------------------------------------------------------ */

/*
 * Check a claim against every layout of the cards the claimer cannot see.
 * Request:
 *   {"dealstr": "<PBN at the start of play>", "trump": "S",
 *    "declarer": "N", "leader": "E", "plays": ["HA", "H5", ...],
 *    "claimer": "E",          optional; defaults to declarer
 *    "total_tricks": 3,       the claiming side's total for the hand,
 *                             counting tricks already won; omit to ask for
 *                             the largest valid claim instead
 *    "budget": 2000000}       optional search node budget
 * Response, with total_tricks:
 *   {"verdict": "valid" | "invalid" | "undecided",
 *    "tricks_won": 2, "tricks_left": 11, "nodes": 19}
 * and without it:
 *   {"guaranteed": 2, "undecided": false, "tricks_won": 2, ...}
 * "undecided" means the budget ran out; treat the claim as rejected, as BBO
 * does. A defender's claim must work whatever partner holds and however
 * partner plays.
 */
char *bs_check_claim(const char *request_json);

/* ---- Play analysis ----------------------------------------------------- */

/*
 * A play analyzer keeps a cache of solved positions, so analysing a hand
 * card by card pays for each new position once. One per thread.
 */
typedef struct BsAnalyzer BsAnalyzer;

BsAnalyzer *bs_analyzer_new(void);
void bs_analyzer_free(BsAnalyzer *analyzer);

/*
 * Every card played with its double-dummy cost. Request:
 *   {"dealstr": "...", "trump": "S", "declarer": "N", "leader": "E",
 *    "plays": ["HA", "H5", ...]}
 * Response: {"contract_tricks": 10, "trace": [{"index": 0, "seat": "E",
 *   "card": "HA", "cost": 0}, ...], "cached": false}
 */
char *bs_analyzer_play(BsAnalyzer *analyzer, const char *request_json);

/* The alternatives at card `node` (0-based) of the play, with their costs. */
char *bs_analyzer_play_node(BsAnalyzer *analyzer, const char *request_json, size_t node);

/* A double-dummy-perfect continuation from card `from` onwards. */
char *bs_analyzer_optimal_line(BsAnalyzer *analyzer, const char *request_json, size_t from);

/* Hand the solver's pooled memory back to the allocator: this thread's, and
 * that of the worker threads bs_dd_tables keeps between batches. */
void bs_release_memory(void);

#ifdef __cplusplus
}
#endif

#endif /* BRIDGE_SOLVER_H */
