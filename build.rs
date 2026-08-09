//! Builds the vendored fio source tree into `libfio.a` and links it into the
//! iomark binary.
//!
//! Strategy:
//! 1. Copy `third_party/fio` into OUT_DIR (fio's build is in-tree).
//! 2. `./configure` with feature-trimming flags, then build only the object
//!    files listed in the Makefile's `$(OBJS)` — never link the fio binary.
//! 3. Compile `fio.o` separately with `-Dmain=fio_main` so the fio entry point
//!    is callable from Rust without clashing with Rust's own `main`.
//! 4. Archive everything into `libfio.a` and emit cargo link directives,
//!    forwarding the exact `$(LIBS)` the fio Makefile would have used.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Makefile snippet that exposes any variable of the included fio Makefile as
/// a `print-<VAR>` target. Used to extract `$(OBJS)` and `$(LIBS)`.
const PRINTVAR_MK: &str = "include Makefile\nprint-%:\n\t@echo $($*)\n";

const CONFIGURE_FLAGS: &[&str] = &[
    // Trim optional engines/deps that iomark never uses, so the external
    // link dependencies stay minimal and portable.
    "--disable-http",
    "--disable-rdma",
    "--disable-rados",
    "--disable-rbd",
    "--disable-gfapi",
    "--disable-libnfs",
    "--disable-xnvme",
    "--disable-pmem",
    "--disable-dfs",
    "--disable-libzbc",
    "--disable-libblkio",
    "--disable-isal",
    "--disable-tcmalloc",
    // No -march=native: release binaries must run on any CPU of the target arch.
    "--disable-native",
    // Avoid the lex/yacc expression parser objects (not needed, extra deps).
    "--disable-lex",
];

/// Unix only: back fio's thread_data with anonymous mmap instead of SysV SHM.
/// iomark kills workers with SIGKILL on abort, and killed processes leak SysV
/// segments (tiny quota on macOS) — mmap memory dies with the process.
const UNIX_CONFIGURE_FLAGS: &[&str] = &["--disable-shm"];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let fio_src = manifest_dir.join("third_party/fio");
    let build_dir = out_dir.join("fio-build");
    let lib = build_dir.join("libfio.a");
    let libs_cache = build_dir.join("iomark-libs.txt");

    println!("cargo:rerun-if-changed=third_party/fio");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");

    if !fio_src.join("configure").exists() {
        panic!("third_party/fio is missing or empty — run `git submodule update --init` first");
    }

    // Rebuild when the configure flags or the fio sources change, not only
    // when libfio.a is gone.
    let fingerprint = format!(
        "{} {} unix={} fio={}",
        CONFIGURE_FLAGS.join(" "),
        env::var("CC").unwrap_or_default(),
        cfg_unix(),
        fio_source_identity(&fio_src),
    );
    let fingerprint_file = build_dir.join("iomark-configure.txt");
    let stale = fs::read_to_string(&fingerprint_file).ok().as_deref() != Some(fingerprint.as_str());
    if !lib.exists() || stale {
        build_libfio(&fio_src, &build_dir, &libs_cache);
        fs::write(&fingerprint_file, &fingerprint).unwrap();
    }

    // Surface the embedded fio version in `iomark --version`.
    let fio_version = fs::read_to_string(build_dir.join("FIO-VERSION-FILE"))
        .ok()
        .and_then(|s| s.split('=').nth(1).map(|v| v.trim().to_owned()))
        .unwrap_or_else(|| "fio-unknown".into());
    println!("cargo:rustc-env=IOMARK_FIO_VERSION={fio_version}");

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    // whole-archive is required: fio registers its built-in ioengines through
    // ELF/Mach-O constructors that nothing references by symbol, so a normal
    // archive link would drop them and fio would find no engines at runtime.
    println!("cargo:rustc-link-lib=static:+whole-archive=fio");
    emit_link_libs(&libs_cache);
}

fn build_libfio(fio_src: &Path, build_dir: &Path, libs_cache: &Path) {
    // Start from a clean tree so stale configure output never leaks in.
    if build_dir.exists() {
        fs::remove_dir_all(build_dir).expect("failed to clean fio build dir");
    }
    copy_tree(fio_src, build_dir);

    let mut extra_cflags = String::from("-fPIC");
    if let Some(arch) = apple_cross_arch() {
        // Cross-building the other macOS architecture on the same host.
        extra_cflags.push_str(&format!(" -arch {arch}"));
    }

    let mut configure = Command::new("sh");
    configure
        .arg("./configure")
        .args(CONFIGURE_FLAGS)
        .arg(format!("--extra-cflags={extra_cflags}"))
        .current_dir(build_dir);
    if cfg_unix() {
        configure.args(UNIX_CONFIGURE_FLAGS);
    }
    if let Ok(cc) = env::var("CC") {
        configure.arg(format!("--cc={cc}"));
    }
    run(configure, "fio configure");

    fs::write(build_dir.join("printvar.mk"), PRINTVAR_MK).unwrap();
    // Generate FIO-VERSION-FILE up front so its side-channel output cannot
    // pollute the variable dumps below.
    run(
        make(build_dir, &["-s", "FIO-VERSION-FILE"]),
        "fio version gen",
    );

    let objs = print_var(build_dir, "OBJS");
    assert!(
        !objs.is_empty(),
        "fio Makefile returned an empty $(OBJS) list"
    );

    // Build only the objects; linking the fio binary is pointless here and
    // would break cross builds.
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut args: Vec<String> = vec![format!("-j{jobs}")];
    args.extend(objs.iter().cloned());
    run(
        make(
            build_dir,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        "fio make objects",
    );
    // fio.o is compiled on its own with the entry point renamed. EXTFLAGS is
    // unused by fio's configure, so this cannot clobber configured CFLAGS.
    run(
        make(build_dir, &["fio.o", "EXTFLAGS=-Dmain=fio_main"]),
        "fio make fio.o",
    );

    let ar = env::var("AR").unwrap_or_else(|_| "ar".into());
    let mut archive = Command::new(ar);
    archive
        .arg("rcs")
        .arg("libfio.a")
        .args(&objs)
        .arg("fio.o")
        .current_dir(build_dir);
    run(archive, "ar libfio.a");

    let libs = print_var(build_dir, "LIBS");
    fs::write(libs_cache, libs.join(" ")).unwrap();
}

/// Translates the fio Makefile's `$(LIBS)` tokens into cargo link directives.
fn emit_link_libs(libs_cache: &Path) {
    let libs = fs::read_to_string(libs_cache).expect("missing cached fio LIBS");
    for token in libs.split_whitespace() {
        if let Some(name) = token.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        } else {
            println!("cargo:rustc-link-arg={token}");
        }
    }
}

fn print_var(build_dir: &Path, var: &str) -> Vec<String> {
    let out = Command::new("make")
        .args(["-s", "-f", "printvar.mk", &format!("print-{var}")])
        .current_dir(build_dir)
        .output()
        .expect("failed to run make");
    if !out.status.success() {
        panic!(
            "extracting $({var}) failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        // FIO-VERSION-GEN may announce the version on stdout; skip such noise.
        .filter(|l| !l.contains("FIO_VERSION"))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
}

fn make(build_dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("make");
    cmd.args(args).current_dir(build_dir);
    cmd
}

fn run(mut cmd: Command, what: &str) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{what}: failed to spawn: {e}"));
    if !out.status.success() {
        panic!(
            "{what} failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// True when the *target* (not the build host) is a Unix platform.
fn cfg_unix() -> bool {
    env::var("CARGO_CFG_UNIX").is_ok()
}

/// Identifies the vendored fio sources: submodule commit plus a hash of any
/// local modifications. Without this, bumping or patching fio would silently
/// keep linking the previously built libfio.a.
fn fio_source_identity(fio_src: &Path) -> String {
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(fio_src)
            .args(args)
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    };
    let Some(head) = git(&["rev-parse", "HEAD"]) else {
        // Not a git checkout (release tarball): sources are immutable enough.
        return "no-git".into();
    };
    let diff = git(&["diff", "HEAD"]).unwrap_or_default();
    format!(
        "{}+{:016x}",
        String::from_utf8_lossy(&head).trim(),
        fnv1a(&diff)
    )
}

/// Tiny dependency-free content hash (FNV-1a) for the dirty-diff fingerprint.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Returns the `-arch` value when cross-building macOS-on-macOS, else None.
fn apple_cross_arch() -> Option<&'static str> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return None;
    }
    let target = env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    let host_is_target = env::var("HOST").is_ok_and(|h| h.starts_with(&target));
    if host_is_target {
        return None;
    }
    match target.as_str() {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}
