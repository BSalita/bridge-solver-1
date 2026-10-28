//! A C interface to bridge-solver, for programs in any language that can call
//! C: C and C++ directly, and Python (`ctypes`), C#, Go, Java, Swift and others
//! through their foreign-function interfaces.
//!
//! The declarations are in `include/bridge_solver.h`, which is the reference
//! for callers; this file is the implementation. Every operation is a thin
//! wrapper over [`bridge_solver::json_api`], so the JSON requests and
//! responses are exactly those of the WebAssembly package and the Python
//! package.
//!
//! The rules at the boundary, all of which the header states too:
//!
//! * Strings in are NUL-terminated UTF-8. Strings out are NUL-terminated
//!   UTF-8 owned by the caller, who releases them with [`bs_free_string`] and
//!   never with `free()`, since Rust's allocator need not be C's.
//! * A JSON-returning call never returns NULL. On failure it returns
//!   `{"error": "<message>"}`; a successful response never has a top-level
//!   `error` key.
//! * No panic crosses into the caller: every entry point catches one and
//!   reports it as an error, because unwinding into C is undefined behaviour.
//! * Calls are safe from several threads at once. A [`BsAnalyzer`] must be
//!   used by one thread at a time.

use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use bridge_solver::json_api::{self, PlayAnalyzer};

/// A play analyzer and its position cache, opaque to C.
pub struct BsAnalyzer {
    inner: PlayAnalyzer,
}

/// The `{"error": ...}` response for `message`.
fn error_json(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for c in message.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    format!("{{\"error\":\"{escaped}\"}}")
}

/// Hand `text` to C as an owned string. Interior NULs cannot occur in JSON
/// from serde, but are replaced rather than trusted.
fn to_c(text: String) -> *mut c_char {
    let text = if text.contains('\0') {
        text.replace('\0', "\u{FFFD}")
    } else {
        text
    };
    CString::new(text).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Read a C string argument, or say why it cannot be read.
///
/// # Safety
///
/// `ptr` must be NULL or point to a NUL-terminated string that stays valid
/// for the duration of the call.
unsafe fn arg<'a>(ptr: *const c_char, name: &str) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(format!("{name} is NULL"));
    }
    // SAFETY: non-NULL, and the caller promises a NUL-terminated string that
    // outlives this call, which is what CStr::from_ptr requires.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| format!("{name} is not valid UTF-8"))
}

/// Run `f`, turning an error or a panic into `{"error": ...}`.
fn respond(f: impl FnOnce() -> Result<String, String>) -> *mut c_char {
    let text = match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(json)) => json,
        Ok(Err(message)) => error_json(&message),
        Err(_) => error_json("internal error: the solver panicked; please report this"),
    };
    to_c(text)
}

/// The library's version, e.g. `"2.0.0"`. Static: do not free it.
#[no_mangle]
pub extern "C" fn bs_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Release a string this library returned. NULL is ignored.
///
/// # Safety
///
/// `s` must be NULL or a pointer returned by this library and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn bs_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: the caller promises `s` came from CString::into_raw in
        // `to_c` and has not been freed, so retaking ownership is sound.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// The 20-cell double-dummy table as JSON; see the header.
///
/// # Safety
///
/// `deal` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bs_dd_table(deal: *const c_char) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    respond(|| json_api::dd_table(unsafe { arg(deal, "deal") }?))
}

/// Many tables by the chosen engine, JSON in and out; see the header.
///
/// # Safety
///
/// `request_json` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bs_dd_tables(request_json: *const c_char) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    respond(|| json_api::dd_tables(unsafe { arg(request_json, "request_json") }?))
}

/// The double-dummy table into `tricks[20]`, rows N, E, S, W and columns
/// C, D, H, S, NT. Returns 0 on success and -1 on failure.
///
/// # Safety
///
/// `deal` must be NULL or a valid NUL-terminated string, and `tricks` NULL
/// or writable for 20 bytes.
#[no_mangle]
pub unsafe extern "C" fn bs_dd_table_tricks(deal: *const c_char, tricks: *mut u8) -> c_int {
    if tricks.is_null() {
        return -1;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded from this function's own contract.
        json_api::dd_table_tricks(unsafe { arg(deal, "deal") }?)
    }));
    match result {
        Ok(Ok(table)) => {
            let values = table.concat();
            // SAFETY: `tricks` is non-NULL and the caller promises 20
            // writable bytes; `values` holds exactly 20.
            unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), tricks, values.len()) };
            0
        }
        _ => -1,
    }
}

/// Par for a deal and vulnerability, as JSON; see the header.
///
/// # Safety
///
/// `deal` and `vulnerable` must each be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bs_par(deal: *const c_char, vulnerable: *const c_char) -> *mut c_char {
    respond(|| {
        // SAFETY: forwarded from this function's own contract.
        let (deal, vulnerable) = unsafe { (arg(deal, "deal")?, arg(vulnerable, "vulnerable")?) };
        json_api::par(deal, vulnerable)
    })
}

/// North-South's double-dummy tricks with `leader` on lead in `trump`, or -1
/// on failure.
///
/// # Safety
///
/// Each argument must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bs_solve(
    deal: *const c_char,
    trump: *const c_char,
    leader: *const c_char,
) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<u8, String> {
        // SAFETY: forwarded from this function's own contract.
        let (deal, trump, leader) = unsafe {
            (
                arg(deal, "deal")?,
                arg(trump, "trump")?,
                arg(leader, "leader")?,
            )
        };
        json_api::solve_contract(deal, trump, leader)
    }));
    match result {
        Ok(Ok(tricks)) => c_int::from(tricks),
        _ => -1,
    }
}

/// Check a claim, or find the largest valid one; JSON in and out, see the
/// header.
///
/// # Safety
///
/// `request_json` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bs_check_claim(request_json: *const c_char) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    respond(|| json_api::check_claim(unsafe { arg(request_json, "request_json") }?))
}

/// A new play analyzer with an empty cache. Release it with
/// [`bs_analyzer_free`].
#[no_mangle]
pub extern "C" fn bs_analyzer_new() -> *mut BsAnalyzer {
    Box::into_raw(Box::new(BsAnalyzer {
        inner: PlayAnalyzer::new(),
    }))
}

/// Release an analyzer. NULL is ignored.
///
/// # Safety
///
/// `analyzer` must be NULL or a pointer from [`bs_analyzer_new`] not yet
/// freed.
#[no_mangle]
pub unsafe extern "C" fn bs_analyzer_free(analyzer: *mut BsAnalyzer) {
    if !analyzer.is_null() {
        // SAFETY: the caller promises it came from Box::into_raw in
        // bs_analyzer_new and has not been freed.
        drop(unsafe { Box::from_raw(analyzer) });
    }
}

/// Run `f` on an analyzer, or report a NULL one.
///
/// # Safety
///
/// `analyzer` must be NULL or a live pointer from [`bs_analyzer_new`] that no
/// other thread is using.
unsafe fn with_analyzer(
    analyzer: *mut BsAnalyzer,
    f: impl FnOnce(&mut PlayAnalyzer) -> Result<String, String>,
) -> *mut c_char {
    respond(|| {
        // SAFETY: non-NULL is checked here; liveness and exclusive use are
        // the caller's promise.
        let a = unsafe { analyzer.as_mut() }.ok_or("analyzer is NULL")?;
        f(&mut a.inner)
    })
}

/// The running trace of a hand: every card played with its double-dummy
/// cost. JSON in and out; see the header.
///
/// # Safety
///
/// As for [`with_analyzer`], and `request_json` NULL or a valid string.
#[no_mangle]
pub unsafe extern "C" fn bs_analyzer_play(
    analyzer: *mut BsAnalyzer,
    request_json: *const c_char,
) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    unsafe { with_analyzer(analyzer, |a| a.dd_play(arg(request_json, "request_json")?)) }
}

/// The alternatives at card `node` of the play, each with its cost.
///
/// # Safety
///
/// As for [`with_analyzer`], and `request_json` NULL or a valid string.
#[no_mangle]
pub unsafe extern "C" fn bs_analyzer_play_node(
    analyzer: *mut BsAnalyzer,
    request_json: *const c_char,
    node: usize,
) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    unsafe {
        with_analyzer(analyzer, |a| {
            a.dd_play_node(arg(request_json, "request_json")?, node)
        })
    }
}

/// A double-dummy-perfect continuation from card `from` onwards.
///
/// # Safety
///
/// As for [`with_analyzer`], and `request_json` NULL or a valid string.
#[no_mangle]
pub unsafe extern "C" fn bs_analyzer_optimal_line(
    analyzer: *mut BsAnalyzer,
    request_json: *const c_char,
    from: usize,
) -> *mut c_char {
    // SAFETY: forwarded from this function's own contract.
    unsafe {
        with_analyzer(analyzer, |a| {
            a.dd_optimal_line(arg(request_json, "request_json")?, from)
        })
    }
}

/// Hand the solver's pooled memory back to the allocator: this thread's, and
/// that of the worker threads `bs_dd_tables` keeps between batches.
#[no_mangle]
pub extern "C" fn bs_release_memory() {
    let _ = catch_unwind(|| {
        bridge_solver::dds_engine::release_workers();
        bridge_solver::drain_pool();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEAL: &str = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\0";

    fn take(s: *mut c_char) -> String {
        assert!(!s.is_null());
        // SAFETY: a fresh string from this library, freed straight after.
        let text = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
        unsafe { bs_free_string(s) };
        text
    }

    #[test]
    fn table_both_ways() {
        let json = take(unsafe { bs_dd_table(DEAL.as_ptr().cast()) });
        assert!(json.starts_with("{\"tricks\":[[8,7,8,10,9]"), "{json}");
        let mut tricks = [0u8; 20];
        assert_eq!(
            unsafe { bs_dd_table_tricks(DEAL.as_ptr().cast(), tricks.as_mut_ptr()) },
            0
        );
        assert_eq!(&tricks[..5], &[8, 7, 8, 10, 9]);
    }

    #[test]
    fn errors_are_json_and_nulls_are_errors() {
        assert_eq!(
            take(unsafe { bs_dd_table(c"junk".as_ptr()) }),
            r#"{"error":"could not parse deal \"junk\""}"#
        );
        assert_eq!(
            take(unsafe { bs_dd_table(std::ptr::null()) }),
            r#"{"error":"deal is NULL"}"#
        );
        assert_eq!(
            unsafe { bs_solve(std::ptr::null(), c"S".as_ptr(), c"E".as_ptr()) },
            -1
        );
        assert_eq!(
            take(unsafe { bs_analyzer_play(std::ptr::null_mut(), c"{}".as_ptr()) }),
            r#"{"error":"analyzer is NULL"}"#
        );
    }

    #[test]
    fn analyzer_lifecycle() {
        let a = bs_analyzer_new();
        let request = c"{\"dealstr\":\"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\",\"trump\":\"S\",\"declarer\":\"N\",\"leader\":\"E\",\"plays\":[\"HA\"]}";
        let json = take(unsafe { bs_analyzer_play(a, request.as_ptr()) });
        assert!(json.contains("\"contract_tricks\":10"), "{json}");
        unsafe { bs_analyzer_free(a) };
    }

    #[test]
    fn version_is_static() {
        let v = unsafe { CStr::from_ptr(bs_version()) };
        assert_eq!(v.to_str().ok(), Some(env!("CARGO_PKG_VERSION")));
    }
}
