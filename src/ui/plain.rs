//! Line-oriented output for non-interactive sessions. In `--json` mode the
//! NDJSON stream owns stdout and human-readable status goes to stderr.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::abort::AbortHandle;
use crate::cli::Unit;
use crate::disk::{DiskInfo, human_bytes};
use crate::report;
use crate::runner::{Cell, Config, Event};
use crate::ui::Outcome;

#[allow(clippy::too_many_arguments)]
pub fn run(
    rx: Receiver<Event>,
    cfg: &Config,
    disk: Option<&DiskInfo>,
    unit: Unit,
    json: bool,
    warnings: &[String],
    abort: &Arc<AbortHandle>,
) -> Outcome {
    // Status lines must not pollute the NDJSON stream.
    let mut status: Box<dyn Write> = if json {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };
    let mut say = move |line: String| {
        let _ = writeln!(status, "{line}");
    };

    say(format!("iomark {}", crate::version()));
    if let Some(d) = disk {
        say(format!(
            "Disk: {}, {} free of {}",
            d.label(),
            human_bytes(d.available),
            human_bytes(d.total),
        ));
    }
    say(format!("Target: {}", cfg.target.display()));
    say(format!(
        "Plan: {} | file {} | {} run(s) x {:?} (warmup {:?}, interval {:?})",
        cfg.tasks
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>()
            .join(", "),
        human_bytes(cfg.size),
        cfg.runs,
        cfg.duration,
        cfg.warmup,
        cfg.interval,
    ));
    for warning in warnings {
        say(format!("Warning: {warning}"));
    }

    let total_ops = cfg.cells().len();
    let mut op_no = 0usize;
    let mut announced: HashMap<(usize, &'static str), ()> = HashMap::new();
    // Once the NDJSON consumer hangs up, stop the benchmark instead of
    // panicking on the next write (Rust ignores SIGPIPE).
    let mut stream_open = true;

    loop {
        let Ok(event) = rx.recv() else {
            // Runner hung up without a terminal event: treat as failure.
            return Outcome::Failed;
        };
        match event {
            Event::Preparing => say(format!(
                "Preparing test file ({})...",
                human_bytes(cfg.size)
            )),
            Event::Cooldown { .. } | Event::Live { .. } => {}
            Event::Phase { cell, phase } => {
                if announced.insert(cell_key(&cell), ()).is_none() {
                    op_no += 1;
                    let spec = cfg.tasks[cell.task];
                    say(format!("[{op_no}/{total_ops}] {} {}", spec, cell.op.name()));
                }
                let _ = phase;
            }
            Event::RunDone { cell, sample } => {
                let _ = cell;
                say(format!(
                    "    run: {:.2} MB/s | {:.0} IOPS | {:.1} us",
                    report::mb(sample.bytes_per_sec),
                    sample.iops,
                    sample.lat_mean_us,
                ));
            }
            Event::OpDone { cell, result } => {
                let _ = cell;
                let value = crate::ui::format_value(unit, result.bytes_per_sec, result.iops);
                say(format!(
                    "    best: {value} {} ({:.0} IOPS, {:.1} us)",
                    unit.heading(),
                    result.iops,
                    result.lat_us,
                ));
                if json
                    && stream_open
                    && writeln!(
                        std::io::stdout(),
                        "{}",
                        report::ndjson_line(&result, cfg, disk)
                    )
                    .is_err()
                {
                    stream_open = false;
                    abort.abort();
                }
            }
            Event::Finished => {
                say("Done.".to_owned());
                return if stream_open {
                    Outcome::Finished
                } else {
                    Outcome::Aborted
                };
            }
            Event::Aborted => {
                say("Aborted.".to_owned());
                return Outcome::Aborted;
            }
            Event::Failed(message) => {
                eprintln!("iomark: {message}");
                return Outcome::Failed;
            }
        }
    }
}

fn cell_key(cell: &Cell) -> (usize, &'static str) {
    (cell.task, cell.op.name())
}
