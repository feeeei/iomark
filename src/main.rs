//! iomark — a CrystalDiskMark-style disk benchmark for the terminal, powered
//! by an embedded fio.

mod abort;
mod cli;
mod disk;
mod fio;
mod fio_worker;
mod report;
mod runner;
mod spec;
mod ui;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Context, Result, ensure};
use clap::Parser;

use crate::abort::AbortHandle;
use crate::cli::Cli;

/// Version string shown by `--version`: crate version plus embedded fio.
pub(crate) fn version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("IOMARK_FIO_VERSION"),
        ")"
    )
}

pub(crate) fn fio_version() -> &'static str {
    env!("IOMARK_FIO_VERSION")
}

fn main() -> ExitCode {
    // The hidden worker mode must be dispatched before anything else so the
    // embedded fio starts from a pristine process (see CLAUDE.md).
    let mut args = std::env::args_os();
    let _exe = args.next();
    if let Some(first) = args.next()
        && first == fio_worker::WORKER_ARG
    {
        fio_worker::run(args);
    }

    let cli = Cli::parse();
    match run(cli) {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(error) => {
            eprintln!("iomark: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ui::Outcome> {
    let target = cli
        .target
        .canonicalize()
        .with_context(|| format!("target directory {} not found", cli.target.display()))?;
    ensure!(
        target.is_dir(),
        "target {} is not a directory",
        target.display()
    );

    let disk = disk::lookup(&target);
    if let Some(d) = &disk {
        // Refuse to fill the volume: require the test file plus some slack.
        let needed = cli.size + (64 << 20);
        ensure!(
            d.available >= needed,
            "not enough free space on {}: {} available, {} needed",
            d.mount.display(),
            disk::human_bytes(d.available),
            disk::human_bytes(needed),
        );
    }

    // A 1M-block task cannot run against a 512K file; catch it up front.
    if let Some(task) = cli.tasks.iter().find(|t| t.block_size > cli.size) {
        anyhow::bail!(
            "--size {} is smaller than {task}'s block size — increase --size or drop the task",
            disk::human_bytes(cli.size),
        );
    }

    let cfg = runner::Config::from_cli(&cli, target);
    let warnings = platform_warnings(&cfg);
    let abort = Arc::new(AbortHandle::default());
    {
        let abort = abort.clone();
        let test_file = runner::test_file_path(&cfg.target);
        ctrlc::set_handler(move || {
            if !abort.abort() {
                // Second signal: the worker may be stuck in uninterruptible
                // I/O — force-quit with best-effort cleanup.
                let _ = std::fs::remove_file(&test_file);
                std::process::exit(130);
            }
        })
        .context("cannot install the Ctrl-C handler")?;
    }

    let (tx, rx) = mpsc::channel();
    let runner_thread = {
        let cfg = cfg.clone();
        let abort = abort.clone();
        std::thread::spawn(move || runner::run(cfg, tx, abort))
    };

    let interactive =
        !cli.json && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let outcome = if interactive {
        ui::tui::run(rx, &cfg, disk.as_ref(), cli.unit, &abort, &warnings)
    } else {
        Ok(ui::plain::run(
            rx,
            &cfg,
            disk.as_ref(),
            cli.unit,
            cli.json,
            &warnings,
            &abort,
        ))
    };

    // Whatever the UI did (including erroring out), shut the runner down so
    // the test-file cleanup (Drop) runs before the process exits.
    abort.abort();
    let _ = runner_thread.join();
    outcome
}

/// Platform quirks worth surfacing next to the results.
fn platform_warnings(cfg: &runner::Config) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(limit) = macos_aio_limit()
        && cfg
            .tasks
            .iter()
            .any(|t| u64::from(t.queue_depth) * u64::from(t.threads) > limit)
    {
        warnings.push(format!(
            "macOS caps in-flight POSIX AIO at {limit} per process (kern.aioprocmax); \
             deeper queues run effectively shallower"
        ));
    }
    warnings
}

/// The per-process POSIX AIO limit on macOS, if discoverable.
fn macos_aio_limit() -> Option<u64> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "kern.aioprocmax"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
