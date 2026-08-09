//! Presentation layers: `tui` for interactive terminals, `plain` for pipes,
//! CI logs, and `--json`.

pub mod plain;
pub mod tui;

use crate::cli::Unit;
use crate::report;

/// Exit status of a finished session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Aborted,
    Failed,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Outcome::Finished => 0,
            // Conventional "terminated by SIGINT" code.
            Outcome::Aborted => 130,
            Outcome::Failed => 1,
        }
    }
}

/// Formats a result value in the requested unit ("14721.86" or "86.89").
pub fn format_value(unit: Unit, bytes_per_sec: f64, iops: f64) -> String {
    match unit {
        Unit::MegabytesPerSec => format!("{:.2}", report::mb(bytes_per_sec)),
        Unit::Iops => format!("{iops:.2}"),
    }
}
