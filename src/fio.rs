//! Spawning the embedded fio worker and parsing its output.
//!
//! Each measured run is one worker process: `iomark __fio-worker <args>`.
//! Final statistics are written by fio as JSON to a temp file (`--output`),
//! while live progress is scraped from the ETA lines fio prints to stdout
//! (`[r=5956MiB/s][r=5956 IOPS]`, separated by `\r` or `\n`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::abort::AbortHandle;
use crate::fio_worker::WORKER_ARG;
use crate::spec::{Pattern, TaskSpec};

/// Platform-native asynchronous ioengine (must honor `iodepth`).
#[cfg(target_os = "linux")]
pub const ENGINE: &str = "libaio";
#[cfg(target_os = "macos")]
pub const ENGINE: &str = "posixaio";
#[cfg(target_os = "windows")]
pub const ENGINE: &str = "windowsaio";
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub const ENGINE: &str = "psync";

/// Direction of one benchmark operation (a CDM table cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Write,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
        }
    }

    fn rw(self, pattern: Pattern) -> &'static str {
        match (pattern, self) {
            (Pattern::Sequential, Op::Read) => "read",
            (Pattern::Sequential, Op::Write) => "write",
            (Pattern::Random, Op::Read) => "randread",
            (Pattern::Random, Op::Write) => "randwrite",
        }
    }
}

/// Live throughput scraped from one fio ETA line.
#[derive(Debug, Clone, Copy)]
pub struct LiveRate {
    pub bytes_per_sec: f64,
    pub iops: f64,
    /// Completion percentage of the current run, when fio reports it.
    pub percent: Option<f64>,
}

/// Final statistics of one measured run.
#[derive(Debug, Clone, Copy)]
pub struct RunSample {
    pub bytes_per_sec: f64,
    pub iops: f64,
    pub lat_mean_us: f64,
    pub lat_min_us: f64,
}

/// One fio worker invocation.
pub struct Workload<'a> {
    pub spec: TaskSpec,
    pub op: Op,
    pub file: &'a Path,
    pub size: u64,
    pub runtime: Duration,
}

/// Runs a workload to completion. Returns `None` when aborted.
pub fn run_workload(
    w: &Workload<'_>,
    abort: &AbortHandle,
    on_progress: impl FnMut(LiveRate),
) -> Result<Option<RunSample>> {
    let output = temp_output_path();
    let mut args = vec![
        format!("--name={}-{}", w.spec.id(), w.op.name()),
        format!("--filename={}", w.file.display()),
        format!("--size={}", w.size),
        format!("--ioengine={ENGINE}"),
        "--direct=1".into(),
        "--randrepeat=0".into(),
        // Jobs as threads (CDM's T semantics) and one process to kill on abort.
        "--thread".into(),
        format!("--bs={}", w.spec.block_size),
        format!("--iodepth={}", w.spec.queue_depth),
        format!("--numjobs={}", w.spec.threads),
        "--group_reporting".into(),
        format!("--rw={}", w.op.rw(w.spec.pattern)),
        "--time_based".into(),
        format!("--runtime={}ms", w.runtime.as_millis()),
        "--output-format=json".into(),
        format!("--output={}", output.display()),
        "--eta=always".into(),
        "--eta-newline=250ms".into(),
        "--eta-interval=250ms".into(),
    ];
    if w.op == Op::Write {
        args.push("--end_fsync=1".into());
    }

    let finished = stream_worker(&args, abort, on_progress)?;
    let result = if finished {
        Some(parse_output(&output, w.op)?)
    } else {
        None
    };
    let _ = std::fs::remove_file(&output);
    Ok(result)
}

/// Creates the shared test file, fully written with data (`--create_only=1`).
/// Returns false when aborted.
pub fn prepare_file(file: &Path, size: u64, abort: &AbortHandle) -> Result<bool> {
    let output = temp_output_path();
    let args = vec![
        "--name=prepare".to_owned(),
        format!("--filename={}", file.display()),
        format!("--size={size}"),
        "--rw=write".into(),
        // Without overwrite=1, fio's layout for a write job merely fallocates:
        // the file reaches full size but contains only unwritten extents, which
        // the kernel serves as zeros without touching the device — inflating
        // every read result several-fold.
        "--overwrite=1".into(),
        // The layout job must die with the worker when aborted.
        "--thread".into(),
        format!("--bs={}", size.min(1 << 20)),
        "--create_only=1".into(),
        "--output-format=json".into(),
        format!("--output={}", output.display()),
    ];
    let finished = stream_worker(&args, abort, |_| {});
    let _ = std::fs::remove_file(&output);
    finished
}

/// Spawns the worker, streams progress, reaps it. Returns false when aborted.
fn stream_worker(
    args: &[String],
    abort: &AbortHandle,
    mut on_progress: impl FnMut(LiveRate),
) -> Result<bool> {
    if abort.is_aborted() {
        return Ok(false);
    }
    let exe = std::env::current_exe().context("cannot locate the iomark executable")?;
    let mut child = Command::new(exe)
        .arg(WORKER_ARG)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn the fio worker")?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // Drain stderr on the side so a chatty fio can never deadlock the pipe.
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    abort.adopt(child);

    // ETA lines are separated by `\r` (same-line refresh) or `\n`.
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&chunk[..n]);
        while let Some(pos) = pending.iter().position(|&b| b == b'\r' || b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            if let Some(rate) = parse_eta_line(&String::from_utf8_lossy(&line)) {
                on_progress(rate);
            }
        }
    }

    let status = abort
        .reap()
        .unwrap_or_else(child_gone)
        .context("failed to reap the fio worker")?;
    let stderr_text = stderr_thread.join().unwrap_or_default();

    if abort.is_aborted() {
        return Ok(false);
    }
    if !status.success() {
        bail!("fio worker failed ({status}): {}", stderr_text.trim());
    }
    Ok(true)
}

fn child_gone() -> std::io::Result<std::process::ExitStatus> {
    Err(std::io::Error::other("fio worker already reaped"))
}

static RATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[(?:r|w)=([0-9]+(?:\.[0-9]+)?)((?:[KMGTP]i)?B)/s\]\[(?:r|w)=([0-9]+(?:\.[0-9]+)?)([kKMG]?) IOPS\]",
    )
    .expect("valid regex")
});
static PERCENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([0-9]+(?:\.[0-9]+)?)%\]").expect("valid regex"));

/// Extracts the live rate from one fio ETA line, if it carries one.
fn parse_eta_line(line: &str) -> Option<LiveRate> {
    let caps = RATE_RE.captures(line)?;
    let bw: f64 = caps[1].parse().ok()?;
    let bw_unit = match &caps[2] {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0f64.powi(3),
        "TiB" => 1024.0f64.powi(4),
        "PiB" => 1024.0f64.powi(5),
        _ => return None,
    };
    let iops: f64 = caps[3].parse().ok()?;
    let iops_unit = match &caps[4] {
        "" => 1.0,
        "k" | "K" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        _ => return None,
    };
    let percent = PERCENT_RE.captures(line).and_then(|c| c[1].parse().ok());
    Some(LiveRate {
        bytes_per_sec: bw * bw_unit,
        iops: iops * iops_unit,
        percent,
    })
}

/// fio `--output-format=json` (the subset iomark consumes).
#[derive(Deserialize)]
struct FioOutput {
    jobs: Vec<FioJob>,
}

#[derive(Deserialize)]
struct FioJob {
    read: FioOpStats,
    write: FioOpStats,
}

#[derive(Deserialize)]
struct FioOpStats {
    bw_bytes: f64,
    iops: f64,
    lat_ns: FioLat,
}

#[derive(Deserialize)]
struct FioLat {
    min: f64,
    mean: f64,
}

fn parse_output(path: &Path, op: Op) -> Result<RunSample> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("fio produced no JSON output at {}", path.display()))?;
    let parsed: FioOutput = serde_json::from_str(&text).context("unexpected fio JSON output")?;
    let job = parsed.jobs.first().context("fio JSON contains no jobs")?;
    let stats = match op {
        Op::Read => &job.read,
        Op::Write => &job.write,
    };
    Ok(RunSample {
        bytes_per_sec: stats.bw_bytes,
        iops: stats.iops,
        lat_mean_us: stats.lat_ns.mean / 1e3,
        lat_min_us: stats.lat_ns.min / 1e3,
    })
}

static OUTPUT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_output_path() -> PathBuf {
    let seq = OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("iomark-fio-{}-{seq}.json", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eta_line_with_rates() {
        let line = "Jobs: 1 (f=1): [R(1)][75.0%][r=5956MiB/s][r=5956 IOPS][eta 00m:01s]";
        let rate = parse_eta_line(line).unwrap();
        assert!((rate.bytes_per_sec - 5956.0 * 1024.0 * 1024.0).abs() < 1.0);
        assert!((rate.iops - 5956.0).abs() < f64::EPSILON);
        assert_eq!(rate.percent, Some(75.0));
    }

    #[test]
    fn parses_kilo_iops_and_write_direction() {
        let line = "Jobs: 1 (f=1): [W(1)][12.5%][w=812KiB/s][w=31.5k IOPS][eta 01m:10s]";
        let rate = parse_eta_line(line).unwrap();
        assert!((rate.bytes_per_sec - 812.0 * 1024.0).abs() < 1.0);
        assert!((rate.iops - 31_500.0).abs() < 1e-6);
    }

    #[test]
    fn ignores_lines_without_rates() {
        assert!(parse_eta_line("Jobs: 1 (f=1)").is_none());
        assert!(parse_eta_line("").is_none());
    }

    #[test]
    fn parses_fio_json_subset() {
        let json = r#"{"jobs":[{"read":{"bw_bytes":5038611948,"iops":4805.1,
            "lat_ns":{"min":250000.0,"mean":1663700.0}},
            "write":{"bw_bytes":0,"iops":0.0,"lat_ns":{"min":0.0,"mean":0.0}}}]}"#;
        let parsed: FioOutput = serde_json::from_str(json).unwrap();
        let read = &parsed.jobs[0].read;
        assert_eq!(read.bw_bytes, 5038611948.0);
        assert!((read.lat_ns.mean / 1e3 - 1663.7).abs() < 1e-9);
    }
}
