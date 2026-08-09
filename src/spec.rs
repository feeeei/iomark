//! Task specification grammar: `(SEQ|RND)<block-size>Q<queue-depth>T<threads>`,
//! e.g. `SEQ1MQ8T1` or `RND4KQ32T16` (case-insensitive). Block-size units are
//! binary (K = KiB, M = MiB, G = GiB), matching CrystalDiskMark.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// Access pattern of a benchmark task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    Sequential,
    Random,
}

/// One benchmark workload shape (CDM row), e.g. SEQ1M Q8T1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSpec {
    pub pattern: Pattern,
    /// Block size in bytes.
    pub block_size: u64,
    pub queue_depth: u32,
    pub threads: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    #[error("task must start with SEQ or RND: {0:?}")]
    Prefix(String),
    #[error("invalid block size in {0:?} (expected e.g. 4K, 128K, 1M)")]
    BlockSize(String),
    #[error("invalid queue/thread section in {0:?} (expected Q<n>T<n>)")]
    QueueThreads(String),
    #[error("queue depth and threads must be at least 1: {0:?}")]
    Zero(String),
}

impl FromStr for TaskSpec {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let upper = s.trim().to_ascii_uppercase();
        let (pattern, rest) = if let Some(rest) = upper.strip_prefix("SEQ") {
            (Pattern::Sequential, rest)
        } else if let Some(rest) = upper.strip_prefix("RND") {
            (Pattern::Random, rest)
        } else {
            return Err(SpecError::Prefix(s.to_owned()));
        };

        // The block size runs until the first 'Q'; 'K'/'M'/'G' are unit
        // suffixes so they cannot terminate the size section.
        let q_pos = rest
            .find('Q')
            .ok_or_else(|| SpecError::QueueThreads(s.to_owned()))?;
        let block_size =
            parse_block_size(&rest[..q_pos]).ok_or_else(|| SpecError::BlockSize(s.to_owned()))?;

        let qt = &rest[q_pos + 1..];
        let t_pos = qt
            .find('T')
            .ok_or_else(|| SpecError::QueueThreads(s.to_owned()))?;
        let queue_depth: u32 = qt[..t_pos]
            .parse()
            .map_err(|_| SpecError::QueueThreads(s.to_owned()))?;
        let threads: u32 = qt[t_pos + 1..]
            .parse()
            .map_err(|_| SpecError::QueueThreads(s.to_owned()))?;
        if queue_depth == 0 || threads == 0 {
            return Err(SpecError::Zero(s.to_owned()));
        }

        Ok(TaskSpec {
            pattern,
            block_size,
            queue_depth,
            threads,
        })
    }
}

fn parse_block_size(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (digits, multiplier) = match s.as_bytes()[s.len() - 1] {
        b'K' => (&s[..s.len() - 1], 1u64 << 10),
        b'M' => (&s[..s.len() - 1], 1u64 << 20),
        b'G' => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    let value: u64 = digits.parse().ok()?;
    (value > 0).then(|| value.checked_mul(multiplier)).flatten()
}

impl TaskSpec {
    /// Compact identifier, e.g. `SEQ1MQ8T1` (stable, used in JSON output).
    pub fn id(&self) -> String {
        format!(
            "{}{}Q{}T{}",
            match self.pattern {
                Pattern::Sequential => "SEQ",
                Pattern::Random => "RND",
            },
            format_block_size(self.block_size),
            self.queue_depth,
            self.threads
        )
    }

    /// Two-line CDM-style label: `("SEQ1M", "Q8T1")`.
    pub fn label(&self) -> (String, String) {
        (
            format!(
                "{}{}",
                match self.pattern {
                    Pattern::Sequential => "SEQ",
                    Pattern::Random => "RND",
                },
                format_block_size(self.block_size)
            ),
            format!("Q{}T{}", self.queue_depth, self.threads),
        )
    }
}

impl fmt::Display for TaskSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (row, qt) = self.label();
        write!(f, "{row} {qt}")
    }
}

/// Formats a byte count using the largest exact binary unit, CDM style
/// (1048576 → "1M", 131072 → "128K").
fn format_block_size(bytes: u64) -> String {
    const UNITS: &[(u64, &str)] = &[(1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")];
    for &(factor, suffix) in UNITS {
        if bytes >= factor && bytes.is_multiple_of(factor) {
            return format!("{}{suffix}", bytes / factor);
        }
    }
    bytes.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cdm_default_tasks() {
        let spec: TaskSpec = "SEQ1MQ8T1".parse().unwrap();
        assert_eq!(
            spec,
            TaskSpec {
                pattern: Pattern::Sequential,
                block_size: 1 << 20,
                queue_depth: 8,
                threads: 1
            }
        );
        let spec: TaskSpec = "RND4KQ32T16".parse().unwrap();
        assert_eq!(
            spec,
            TaskSpec {
                pattern: Pattern::Random,
                block_size: 4096,
                queue_depth: 32,
                threads: 16
            }
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(
            "seq128kq32t1".parse::<TaskSpec>().unwrap(),
            "SEQ128KQ32T1".parse::<TaskSpec>().unwrap()
        );
    }

    #[test]
    fn round_trips_through_id() {
        for id in [
            "SEQ1MQ8T1",
            "SEQ128KQ32T1",
            "RND4KQ32T16",
            "RND4KQ1T1",
            "RND8KQ4T4",
        ] {
            assert_eq!(id.parse::<TaskSpec>().unwrap().id(), id);
        }
    }

    #[test]
    fn rejects_malformed_specs() {
        for bad in [
            "",
            "FOO4KQ1T1",
            "SEQQ1T1",
            "SEQ4K",
            "SEQ4KQ0T1",
            "SEQ4KQ1T0",
            "RND4KQXT1",
        ] {
            assert!(bad.parse::<TaskSpec>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn displays_cdm_style_label() {
        let spec: TaskSpec = "RND4KQ32T16".parse().unwrap();
        assert_eq!(spec.to_string(), "RND4K Q32T16");
    }
}
