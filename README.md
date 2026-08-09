# iomark

A CrystalDiskMark-style disk benchmark for the terminal — one static binary
with [fio](https://github.com/axboe/fio) embedded as the measurement engine
and a btop-style TUI.

```
╭ iomark 0.1.0 (fio-3.42) ───────────────────────────────────────────────────────────────╮
│ disk Data (/dev/disk4s1, apfs)  ·  3.6 TiB free of 4.0 TiB                             │
│ file 1.0 GiB  ·  3 runs × 5s  ·  warmup 5s  ·  interval 5s                             │
╰────────────────────────────────────────────────────────────────────────────────────────╯
╭ results ───────────────────────────────────────────────────────────────────────────────╮
│                │            Read (MB/s)            │            Write (MB/s)           │
│────────────────┼───────────────────────────────────┼───────────────────────────────────│
│  SEQ1M Q8T1    │ ███████████████████████   6531.10 │ ██████████████████████▊   6273.44 │
│────────────────┼───────────────────────────────────┼───────────────────────────────────│
│  SEQ128K Q32T1 │ ██████████████████████▋   6489.02 │ ██████████████████████    6104.87 │
│────────────────┼───────────────────────────────────┼───────────────────────────────────│
│▶ RND4K Q32T16  │ ███████▎················  2411.90 │ ·······················         – │
│────────────────┼───────────────────────────────────┼───────────────────────────────────│
│  RND4K Q1T1    │ ·······················         – │ ·······················         – │
╰────────────────────────────────────────────────────────────────────────────────────────╯
╭ status ────────────────────────────────────────────────────────────────────────────────╮
│ ▶ RND4K Q32T16 read · run 2/3             ██████▊·········  42% · ~2:41 left           │
╰────────────────────────────────────────────────────────────────────── K unit  Q quit ──╯
```

Press `K` for the IOPS view, which also shows mean latency per cell.

- **One binary, no dependencies** — fio is statically linked in; nothing is
  extracted or installed at runtime.
- **CrystalDiskMark semantics** — same default workloads (the CDM 9
  "NVMe SSD" preset), same scoring (best of N runs), decimal MB/s. Each task
  measures read then write before moving to the next.
- **Live TUI** — big numbers over proportional bars, updating in real time;
  press `K` to flip between MB/s and IOPS, `Q` to quit.
- **Scriptable** — `--json` emits one NDJSON object per completed workload;
  non-TTY output degrades to plain lines for CI logs.

## Usage

```sh
iomark                     # benchmark the current directory's disk
iomark /path/to/mount      # benchmark a specific disk
iomark --json > out.ndjson # machine-readable results
```

```
Options:
      --tasks <LIST>      Comma-separated workloads: (SEQ|RND)<block>Q<depth>T<threads>
                          [default: SEQ1MQ8T1,SEQ128KQ32T1,RND4KQ32T16,RND4KQ1T1]
      --type <UNIT>       Initial display unit (K toggles in the TUI) [default: MB/s]
      --runs <N>          Measured runs per operation, best kept [default: 3]
      --size <SIZE>       Test file size, binary units [default: 1GiB]
      --duration <TIME>   Duration of each measured run [default: 5s]
      --warmup <TIME>     Unmeasured warmup per operation [default: 5s]
      --interval <TIME>   Pause between operations [default: 5s]
      --json              NDJSON output, one object per workload
  -V, --version           Print version (iomark + embedded fio)
```

Custom workloads follow the same grammar as the presets: `RND8KQ4T4` is
random 8 KiB blocks at queue depth 4 with 4 threads.

## Building

```sh
git clone --recurse-submodules https://github.com/feeeei/iomark
cd iomark
cargo build --release
```

Requirements: Rust 1.89+, `make`, a C compiler, `sh`.

| Platform | Notes |
|---|---|
| Linux (x86_64 / ARM64) | `libaio-dev` and `zlib1g-dev` recommended |
| macOS (Apple Silicon / Intel) | works out of the box |
| Windows x86_64 | build inside an MSYS2 **MINGW64** shell with the `x86_64-pc-windows-gnu` toolchain |
| Windows ARM64 | experimental: MSYS2 CLANGARM64 + `aarch64-pc-windows-gnullvm` |

## Methodology

Tasks run in order; each task measures read first, then write (CDM instead
runs every read before any write — a deliberate difference). Each operation
runs one unscored warmup pass, then N scored runs of fixed duration
(`--time_based`); the best bandwidth and the lowest mean latency are
reported, exactly like CrystalDiskMark. The shared test file is written full
of data up front (no sparse-file shortcuts) and deleted on exit — including
on Ctrl-C.

Per-platform fio engines: `libaio` (Linux), `posixaio` (macOS), `windowsaio`
(Windows), always with `direct=1`.

**Comparability notes**

- MB/s is decimal (bytes ÷ 10⁶), matching CDM. IOPS and latency come from
  fio's job statistics.
- On macOS there is no `O_DIRECT`; fio's `direct=1` maps to `F_NOCACHE`,
  which is a weaker cache bypass. macOS numbers are comparable to other
  macOS tools (AmorphousDiskMark), not bit-for-bit to Linux/Windows.
- macOS also caps in-flight POSIX AIO per process (`kern.aioprocmax`,
  default 16), so Q32 workloads effectively run at a shallower queue depth
  there; iomark prints a warning when a task exceeds the limit. The caps
  can be raised until reboot (`sudo sysctl kern.aioprocmax=256
  kern.aiomax=1024`), but gains are modest: the kernel services POSIX AIO
  through a small worker-thread pool (`kern.aiothreads`, default 4).
- CDM drives Windows' DiskSpd; iomark drives fio everywhere. Numbers on the
  same hardware land close to CDM's but are not guaranteed identical.

## License

GPL-2.0-only — iomark statically links fio, which is GPL-2.0. See
[LICENSE](LICENSE).
