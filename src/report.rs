//! NDJSON emission for `--json`: one object per completed workload.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::disk::DiskInfo;
use crate::fio;
use crate::runner::{Config, OpResult};
use crate::spec::Pattern;

#[derive(Serialize)]
struct WorkloadReport<'a> {
    schema: u32,
    timestamp_unix: u64,
    task: String,
    label: String,
    pattern: &'static str,
    block_size: u64,
    queue_depth: u32,
    threads: u32,
    op: &'static str,
    runs: u32,
    duration_s: f64,
    warmup_s: f64,
    best: Score,
    all_runs: Vec<RunScore>,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: Option<DiskReport<'a>>,
    size_bytes: u64,
    engine: &'static str,
    fio_version: &'a str,
}

/// Identity of the volume under test, captured at startup.
#[derive(Serialize)]
struct DiskReport<'a> {
    source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<&'a str>,
    file_system: &'a str,
    mount: String,
    total_bytes: u64,
    available_bytes: u64,
}

#[derive(Serialize)]
struct Score {
    mb_s: f64,
    iops: f64,
    lat_us: f64,
}

#[derive(Serialize)]
struct RunScore {
    mb_s: f64,
    iops: f64,
    lat_us: f64,
    lat_min_us: f64,
}

/// Renders one completed operation as a single NDJSON line.
pub fn ndjson_line(result: &OpResult, cfg: &Config, disk: Option<&DiskInfo>) -> String {
    let report = WorkloadReport {
        schema: 1,
        timestamp_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        task: result.spec.id(),
        label: result.spec.to_string(),
        pattern: match result.spec.pattern {
            Pattern::Sequential => "sequential",
            Pattern::Random => "random",
        },
        block_size: result.spec.block_size,
        queue_depth: result.spec.queue_depth,
        threads: result.spec.threads,
        op: result.op.name(),
        runs: result.runs.len() as u32,
        duration_s: cfg.duration.as_secs_f64(),
        warmup_s: cfg.warmup.as_secs_f64(),
        best: Score {
            mb_s: mb(result.bytes_per_sec),
            iops: result.iops,
            lat_us: result.lat_us,
        },
        all_runs: result
            .runs
            .iter()
            .map(|s| RunScore {
                mb_s: mb(s.bytes_per_sec),
                iops: s.iops,
                lat_us: s.lat_mean_us,
                lat_min_us: s.lat_min_us,
            })
            .collect(),
        target: cfg.target.display().to_string(),
        disk: disk.map(|d| DiskReport {
            source: &d.source,
            volume: d.volume.as_deref(),
            file_system: &d.file_system,
            mount: d.mount.display().to_string(),
            total_bytes: d.total,
            available_bytes: d.available,
        }),
        size_bytes: cfg.size,
        engine: fio::ENGINE,
        fio_version: crate::fio_version(),
    };
    serde_json::to_string(&report).expect("report serialization cannot fail")
}

/// CrystalDiskMark's MB/s: decimal megabytes (bytes / 10^6).
pub fn mb(bytes_per_sec: f64) -> f64 {
    bytes_per_sec / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fio::{Op, RunSample};
    use crate::runner::OpResult;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn ndjson_line_is_valid_json_with_expected_fields() {
        let cfg = Config {
            tasks: vec!["SEQ1MQ8T1".parse().unwrap()],
            target: PathBuf::from("/tmp"),
            size: 1 << 30,
            runs: 2,
            duration: Duration::from_secs(5),
            warmup: Duration::from_secs(5),
            interval: Duration::from_secs(5),
        };
        let sample = RunSample {
            bytes_per_sec: 14_721_860_000.0,
            iops: 14_040.0,
            lat_mean_us: 568.4,
            lat_min_us: 120.0,
        };
        let result = OpResult {
            spec: cfg.tasks[0],
            op: Op::Read,
            bytes_per_sec: sample.bytes_per_sec,
            iops: sample.iops,
            lat_us: sample.lat_mean_us,
            runs: vec![sample],
        };
        let disk = DiskInfo {
            source: "/dev/disk3s5".into(),
            volume: Some("Macintosh HD - Data".into()),
            mount: PathBuf::from("/System/Volumes/Data"),
            file_system: "apfs".into(),
            total: 500,
            available: 100,
        };
        let line = ndjson_line(&result, &cfg, Some(&disk));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["task"], "SEQ1MQ8T1");
        assert_eq!(value["op"], "read");
        assert_eq!(value["block_size"], 1 << 20);
        assert!((value["best"]["mb_s"].as_f64().unwrap() - 14721.86).abs() < 1e-6);
        assert_eq!(value["all_runs"].as_array().unwrap().len(), 1);
        assert_eq!(value["disk"]["source"], "/dev/disk3s5");
        assert_eq!(value["disk"]["volume"], "Macintosh HD - Data");
        assert_eq!(value["disk"]["file_system"], "apfs");
        assert_eq!(value["disk"]["total_bytes"], 500);

        let line = ndjson_line(&result, &cfg, None);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(value.get("disk").is_none());
    }
}
