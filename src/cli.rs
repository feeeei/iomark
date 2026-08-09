//! Command-line interface. Defaults mirror CrystalDiskMark 9 with its
//! "NVMe SSD" task preset.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use parse_size::{Config as SizeConfig, UnitSystem};

use crate::spec::TaskSpec;

const DEFAULT_TASKS: &str = "SEQ1MQ8T1,SEQ128KQ32T1,RND4KQ32T16,RND4KQ1T1";

/// A CrystalDiskMark-style disk benchmark powered by an embedded fio.
#[derive(Debug, Parser)]
#[command(name = "iomark", version = crate::version(), about, max_term_width = 100)]
pub struct Cli {
    /// Directory to benchmark (the test file is created here)
    #[arg(value_name = "TARGET_DIR", default_value = ".")]
    pub target: PathBuf,

    /// Comma-separated workloads: (SEQ|RND)<block>Q<depth>T<threads>
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_TASKS, value_name = "LIST")]
    pub tasks: Vec<TaskSpec>,

    /// Initial display unit (press K in the TUI to toggle)
    #[arg(long = "type", value_enum, default_value_t = Unit::MegabytesPerSec, value_name = "UNIT")]
    pub unit: Unit,

    /// Measured runs per read/write operation; the best result is kept
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=99), value_name = "N")]
    pub runs: u32,

    /// Test file size (binary units: 1GiB, 512M, ...)
    #[arg(long, default_value = "1GiB", value_parser = parse_bytes, value_name = "SIZE")]
    pub size: u64,

    /// Duration of each measured run
    #[arg(long, default_value = "5s", value_parser = parse_positive_duration, value_name = "TIME")]
    pub duration: Duration,

    /// Unmeasured warmup before each operation (0s to disable)
    #[arg(long, default_value = "5s", value_parser = parse_duration, value_name = "TIME")]
    pub warmup: Duration,

    /// Pause between operations (0s to disable)
    #[arg(long, default_value = "5s", value_parser = parse_duration, value_name = "TIME")]
    pub interval: Duration,

    /// Emit one JSON object per completed workload (NDJSON, disables the TUI)
    #[arg(long)]
    pub json: bool,
}

/// Display unit for benchmark results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Unit {
    /// Decimal megabytes per second (bytes / 10^6), as in CrystalDiskMark
    #[value(name = "MB/s", alias = "mbs", alias = "mb")]
    MegabytesPerSec,
    /// I/O operations per second
    #[value(name = "IOPS", alias = "iops")]
    Iops,
}

impl Unit {
    pub fn toggled(self) -> Self {
        match self {
            Unit::MegabytesPerSec => Unit::Iops,
            Unit::Iops => Unit::MegabytesPerSec,
        }
    }

    pub fn heading(self) -> &'static str {
        match self {
            Unit::MegabytesPerSec => "MB/s",
            Unit::Iops => "IOPS",
        }
    }
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    SizeConfig::new()
        .with_unit_system(UnitSystem::Binary)
        .parse_size(s)
        .map_err(|e| e.to_string())
        .and_then(|n| {
            if n == 0 {
                Err("size must be non-zero".into())
            } else {
                Ok(n)
            }
        })
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    // Accept both "5s" (humantime) and a bare "5".
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

/// For --duration: fio would silently drop `time_based` on a zero runtime and
/// run a full-size pass instead, so reject it up front.
fn parse_positive_duration(s: &str) -> Result<Duration, String> {
    let duration = parse_duration(s)?;
    if duration.is_zero() {
        return Err("duration must be non-zero".into());
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_cdm9_nvme_preset() {
        let cli = Cli::parse_from(["iomark"]);
        let ids: Vec<String> = cli.tasks.iter().map(TaskSpec::id).collect();
        assert_eq!(
            ids,
            ["SEQ1MQ8T1", "SEQ128KQ32T1", "RND4KQ32T16", "RND4KQ1T1"]
        );
        assert_eq!(cli.runs, 3);
        assert_eq!(cli.size, 1 << 30);
        assert_eq!(cli.duration, Duration::from_secs(5));
        assert_eq!(cli.warmup, Duration::from_secs(5));
        assert_eq!(cli.interval, Duration::from_secs(5));
        assert_eq!(cli.unit, Unit::MegabytesPerSec);
        assert!(!cli.json);
    }

    #[test]
    fn parses_sizes_as_binary_units() {
        let cli = Cli::parse_from(["iomark", "--size", "512M"]);
        assert_eq!(cli.size, 512 << 20);
        let cli = Cli::parse_from(["iomark", "--size", "2GiB"]);
        assert_eq!(cli.size, 2 << 30);
    }

    #[test]
    fn parses_unit_names() {
        assert_eq!(
            Cli::parse_from(["iomark", "--type", "IOPS"]).unit,
            Unit::Iops
        );
        assert_eq!(
            Cli::parse_from(["iomark", "--type", "MB/s"]).unit,
            Unit::MegabytesPerSec
        );
    }

    #[test]
    fn accepts_bare_second_durations() {
        let cli = Cli::parse_from(["iomark", "--duration", "3", "--warmup", "0s"]);
        assert_eq!(cli.duration, Duration::from_secs(3));
        assert_eq!(cli.warmup, Duration::ZERO);
    }
}
