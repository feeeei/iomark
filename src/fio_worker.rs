//! The `__fio-worker` entry point: turns the current process into a plain fio
//! binary (busybox style).
//!
//! INVARIANT: the embedded fio entry must only ever run in a dedicated child
//! process spawned via [`WORKER_ARG`]. fio installs its own signal handlers,
//! mutates process-wide global state, calls `exit()` on argument errors, and
//! forks job processes — calling it from the long-lived iomark process would
//! corrupt it. See CLAUDE.md.

use std::ffi::{CString, OsString};
use std::os::raw::{c_char, c_int};

/// Hidden argv[1] that selects the fio worker mode.
pub const WORKER_ARG: &str = "__fio-worker";

unsafe extern "C" {
    /// fio's `main()`, renamed at compile time via `-Dmain=fio_main` (build.rs).
    fn fio_main(argc: c_int, argv: *mut *mut c_char, envp: *mut *mut c_char) -> c_int;
}

/// Runs fio with the given arguments and exits with fio's exit code.
/// Never returns.
pub fn run(args: impl Iterator<Item = OsString>) -> ! {
    // argv[0] is cosmetic for fio; keep it recognizable in `ps` output.
    let argv_owned: Vec<CString> = std::iter::once(OsString::from("iomark-fio"))
        .chain(args)
        .map(to_cstring)
        .collect();
    let envp_owned: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| {
            let mut kv = k;
            kv.push("=");
            kv.push(v);
            to_cstring(kv)
        })
        .collect();

    let argc = argv_owned.len() as c_int;
    // Hand fio owned, mutable buffers: getopt is allowed to permute argv and
    // POSIX allows it to modify the strings. The allocations are intentionally
    // leaked — this process is about to become fio and then exit.
    let mut argv: Vec<*mut c_char> = argv_owned.into_iter().map(CString::into_raw).collect();
    argv.push(std::ptr::null_mut());
    let mut envp: Vec<*mut c_char> = envp_owned.into_iter().map(CString::into_raw).collect();
    envp.push(std::ptr::null_mut());

    // SAFETY: argv/envp are NUL-terminated C string arrays with a trailing
    // null pointer, argc matches argv's length, and both outlive the call
    // (leaked). fio_main is the C entry point linked in by build.rs.
    let code = unsafe { fio_main(argc, argv.as_mut_ptr(), envp.as_mut_ptr()) };
    std::process::exit(code);
}

/// Converts an OS string to a C string, preserving raw bytes on Unix.
fn to_cstring(s: OsString) -> CString {
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(s.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = s.to_string_lossy().into_owned().into_bytes();
    // Interior NUL cannot appear in real argv/env values; replace defensively
    // rather than aborting the worker.
    CString::new(bytes).unwrap_or_else(|e| {
        let sanitized: Vec<u8> = e.into_vec().into_iter().filter(|&b| b != 0).collect();
        CString::new(sanitized).expect("NUL-free bytes")
    })
}
