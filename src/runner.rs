//! Benchmark orchestration. Tasks run in order and each task measures Read
//! then Write before moving on (unlike CDM's all-reads-then-all-writes — a
//! deliberate product choice); a pause between operations; per operation one
//! unscored warmup run plus N scored runs, of which the best (max bandwidth,
//! min latency) is kept.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::abort::AbortHandle;
use crate::cli::Cli;
use crate::fio::{self, LiveRate, Op, RunSample, Workload};
use crate::spec::TaskSpec;

/// Everything the runner needs, distilled from the CLI.
#[derive(Debug, Clone)]
pub struct Config {
    pub tasks: Vec<TaskSpec>,
    pub target: PathBuf,
    pub size: u64,
    pub runs: u32,
    pub duration: Duration,
    pub warmup: Duration,
    pub interval: Duration,
}

impl Config {
    pub fn from_cli(cli: &Cli, target: PathBuf) -> Self {
        Config {
            tasks: cli.tasks.clone(),
            target,
            size: cli.size,
            runs: cli.runs,
            duration: cli.duration,
            warmup: cli.warmup,
            interval: cli.interval,
        }
    }

    /// Operations in execution order: per task, Read first then Write.
    pub fn cells(&self) -> Vec<Cell> {
        (0..self.tasks.len())
            .flat_map(|task| [Op::Read, Op::Write].map(|op| Cell { task, op }))
            .collect()
    }
}

/// Identifies one table cell: a task row plus a direction column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    pub task: usize,
    pub op: Op,
}

/// What a cell is currently doing.
#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Warmup,
    Measuring { run: u32, total: u32 },
}

/// Aggregated outcome of one operation (one cell).
#[derive(Debug, Clone)]
pub struct OpResult {
    pub spec: TaskSpec,
    pub op: Op,
    /// Best run: max bandwidth (and therefore max IOPS — same block size).
    pub bytes_per_sec: f64,
    pub iops: f64,
    /// Best (lowest) mean latency across runs, in microseconds.
    pub lat_us: f64,
    pub runs: Vec<RunSample>,
}

/// Progress stream from the runner thread to the UI thread.
#[derive(Debug)]
pub enum Event {
    /// Laying out the shared test file.
    Preparing,
    /// Pause between operations, with time left.
    Cooldown {
        next: Cell,
        remaining: Duration,
    },
    Phase {
        cell: Cell,
        phase: Phase,
    },
    Live {
        cell: Cell,
        rate: LiveRate,
    },
    RunDone {
        cell: Cell,
        sample: RunSample,
    },
    OpDone {
        cell: Cell,
        result: OpResult,
    },
    /// All operations completed.
    Finished,
    /// Cancelled by the user; partial results stand.
    Aborted,
    /// Unrecoverable failure (message is user-facing).
    Failed(String),
}

/// Runs the whole benchmark on the current thread (spawn it on a worker
/// thread), reporting progress through `tx`. Always emits a terminal event.
pub fn run(cfg: Config, tx: Sender<Event>, abort: Arc<AbortHandle>) {
    let outcome = run_inner(&cfg, &tx, &abort);
    let terminal = match outcome {
        Ok(true) => Event::Finished,
        Ok(false) => Event::Aborted,
        Err(e) => Event::Failed(format!("{e:#}")),
    };
    let _ = tx.send(terminal);
}

fn run_inner(cfg: &Config, tx: &Sender<Event>, abort: &AbortHandle) -> Result<bool> {
    let file = TestFile::create_path(&cfg.target)?;

    let _ = tx.send(Event::Preparing);
    if !fio::prepare_file(&file.path, cfg.size, abort)? {
        return Ok(false);
    }

    let mut first = true;
    for cell in cfg.cells() {
        if !std::mem::take(&mut first) && !pause_between_ops(cfg, tx, abort, cell) {
            return Ok(false);
        }
        if !run_operation(cfg, tx, abort, cell, &file)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Runs warmup plus the N scored runs of one cell. Returns false when aborted.
fn run_operation(
    cfg: &Config,
    tx: &Sender<Event>,
    abort: &AbortHandle,
    cell: Cell,
    file: &TestFile,
) -> Result<bool> {
    let spec = cfg.tasks[cell.task];
    let workload = |runtime| Workload {
        spec,
        op: cell.op,
        file: &file.path,
        size: cfg.size,
        runtime,
    };

    if !cfg.warmup.is_zero() {
        let _ = tx.send(Event::Phase {
            cell,
            phase: Phase::Warmup,
        });
        let live = |rate| {
            let _ = tx.send(Event::Live { cell, rate });
        };
        if fio::run_workload(&workload(cfg.warmup), abort, live)?.is_none() {
            return Ok(false);
        }
    }

    let mut samples = Vec::with_capacity(cfg.runs as usize);
    for run in 1..=cfg.runs {
        let _ = tx.send(Event::Phase {
            cell,
            phase: Phase::Measuring {
                run,
                total: cfg.runs,
            },
        });
        let live = |rate| {
            let _ = tx.send(Event::Live { cell, rate });
        };
        match fio::run_workload(&workload(cfg.duration), abort, live)? {
            Some(sample) => {
                let _ = tx.send(Event::RunDone { cell, sample });
                samples.push(sample);
            }
            None => return Ok(false),
        }
    }

    let result = aggregate(spec, cell.op, samples);
    let _ = tx.send(Event::OpDone { cell, result });
    Ok(true)
}

/// CDM scoring: best bandwidth/IOPS, lowest mean latency.
fn aggregate(spec: TaskSpec, op: Op, runs: Vec<RunSample>) -> OpResult {
    let bytes_per_sec = runs.iter().map(|s| s.bytes_per_sec).fold(0.0, f64::max);
    let iops = runs.iter().map(|s| s.iops).fold(0.0, f64::max);
    let lat_us = runs
        .iter()
        .map(|s| s.lat_mean_us)
        .fold(f64::INFINITY, f64::min);
    OpResult {
        spec,
        op,
        bytes_per_sec,
        iops,
        lat_us,
        runs,
    }
}

/// Abort-responsive pause between operations. Returns false when aborted.
fn pause_between_ops(cfg: &Config, tx: &Sender<Event>, abort: &AbortHandle, next: Cell) -> bool {
    const TICK: Duration = Duration::from_millis(100);
    let mut remaining = cfg.interval;
    let _ = tx.send(Event::Cooldown { next, remaining });
    while !remaining.is_zero() {
        if abort.is_aborted() {
            return false;
        }
        let step = remaining.min(TICK);
        std::thread::sleep(step);
        remaining -= step;
        // ~2 updates per second keeps the countdown alive without spam.
        if remaining.subsec_millis() % 500 < 100 {
            let _ = tx.send(Event::Cooldown { next, remaining });
        }
    }
    !abort.is_aborted()
}

/// The shared benchmark file; removed on drop (normal exit, abort, or error).
struct TestFile {
    path: PathBuf,
}

/// Location of the shared benchmark file for this process.
pub fn test_file_path(target: &std::path::Path) -> PathBuf {
    target.join(format!("iomark-{}.tmp", std::process::id()))
}

impl TestFile {
    fn create_path(target: &std::path::Path) -> Result<TestFile> {
        let path = test_file_path(target);
        // Fail early with a clear message if the directory is not writable.
        std::fs::File::create(&path)
            .and_then(|_| std::fs::remove_file(&path))
            .with_context(|| format!("cannot create test file in {}", target.display()))?;
        Ok(TestFile { path })
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(bw: f64, lat: f64) -> RunSample {
        RunSample {
            bytes_per_sec: bw,
            iops: bw / 4096.0,
            lat_mean_us: lat,
            lat_min_us: lat / 2.0,
        }
    }

    #[test]
    fn aggregation_takes_best_bandwidth_and_lowest_latency() {
        let spec: TaskSpec = "RND4KQ1T1".parse().unwrap();
        let result = aggregate(
            spec,
            Op::Read,
            vec![sample(100.0, 9.0), sample(300.0, 12.0), sample(200.0, 7.0)],
        );
        assert_eq!(result.bytes_per_sec, 300.0);
        assert_eq!(result.lat_us, 7.0);
        assert_eq!(result.runs.len(), 3);
    }

    #[test]
    fn cells_run_read_then_write_per_task() {
        let cfg = Config {
            tasks: vec!["SEQ1MQ8T1".parse().unwrap(), "RND4KQ1T1".parse().unwrap()],
            target: PathBuf::from("."),
            size: 1,
            runs: 1,
            duration: Duration::from_secs(1),
            warmup: Duration::ZERO,
            interval: Duration::ZERO,
        };
        let cells = cfg.cells();
        let ops: Vec<(usize, Op)> = cells.iter().map(|c| (c.task, c.op)).collect();
        assert_eq!(
            ops,
            [(0, Op::Read), (0, Op::Write), (1, Op::Read), (1, Op::Write)]
        );
    }
}
