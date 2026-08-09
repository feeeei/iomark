//! Interactive btop-style TUI: tight bordered panels, per-task read/write
//! rows with gradient ratio bars, a live status footer. `K` toggles
//! MB/s ↔ IOPS, `Q`/`Esc`/`Ctrl-C` aborts (or exits the result screen).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event as InputEvent, KeyCode, KeyEventKind, KeyModifiers, poll, read,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::abort::AbortHandle;
use crate::cli::{ColorMode, Unit};
use crate::disk::{DiskInfo, human_bytes};
use crate::fio::{LiveRate, Op};
use crate::runner::{Cell, Config, Event, OpResult, Phase};
use crate::ui::Outcome;

/// Content is capped at this width and horizontally centered beyond it.
const MAX_WIDTH: u16 = 100;

/// Whether crossterm can read key events from this process.
///
/// Being attached to a terminal is not enough. crossterm polls the input
/// descriptor through mio, and on macOS a descriptor opened on `/dev/tty` —
/// what a `curl | sh` handoff produces — cannot be registered with kqueue
/// (`EINVAL`), so the reader never starts and every poll fails. crossterm
/// swallows that at construction time and only reports it on first use, which
/// would kill a benchmark that is already running; probing up front lets the
/// caller fall back to the plain renderer instead.
pub fn input_available() -> bool {
    poll(Duration::ZERO).is_ok()
}

pub fn run(
    rx: Receiver<Event>,
    cfg: &Config,
    disk: Option<&DiskInfo>,
    unit: Unit,
    color: ColorMode,
    abort: &Arc<AbortHandle>,
    warnings: &[String],
) -> Result<Outcome> {
    // crossterm drops every color sequence on the floor when NO_COLOR is set.
    // That is the right default, but an explicit --color is a deliberate
    // override and has to reach the terminal.
    match color {
        ColorMode::Auto => {}
        ColorMode::Never => ratatui::crossterm::style::force_color_output(false),
        _ => ratatui::crossterm::style::force_color_output(true),
    }
    let mut terminal = ratatui::init();
    let mut app = App::new(cfg, unit, color);
    let result = event_loop(&mut terminal, &rx, &mut app, cfg, disk, abort, warnings);
    // Always restore the terminal before touching stderr.
    ratatui::restore();
    if let Some(message) = &app.failure {
        eprintln!("iomark: {message}");
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    rx: &Receiver<Event>,
    app: &mut App,
    cfg: &Config,
    disk: Option<&DiskInfo>,
    abort: &Arc<AbortHandle>,
    warnings: &[String],
) -> Result<Outcome> {
    loop {
        // Drain runner progress. A disconnect without a terminal event means
        // the runner died (panicked) — fail instead of hanging forever.
        loop {
            match rx.try_recv() {
                Ok(event) => app.apply(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if app.outcome.is_none() {
                        app.failure = Some("the benchmark runner stopped unexpectedly".into());
                        app.outcome = Some(Outcome::Failed);
                    }
                    break;
                }
            }
        }
        terminal.draw(|frame| draw(frame, app, cfg, disk, warnings))?;

        // Abort/failure leave immediately; Finished keeps the result table on
        // screen (CDM-style) until the user quits.
        if let Some(outcome @ (Outcome::Aborted | Outcome::Failed)) = app.outcome {
            return Ok(outcome);
        }

        if poll(Duration::from_millis(50))? {
            match read()? {
                InputEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('k') | KeyCode::Char('K') => app.unit = app.unit.toggled(),
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => match app.outcome {
                        Some(outcome) => return Ok(outcome),
                        None => {
                            abort.abort();
                        }
                    },
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        match app.outcome {
                            Some(outcome) => return Ok(outcome),
                            None => {
                                abort.abort();
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

/// What the UI currently knows about one cell.
#[derive(Default)]
struct CellView {
    live: Option<LiveRate>,
    phase: Option<Phase>,
    result: Option<OpResult>,
}

impl CellView {
    /// Value shown in the given unit; completed beats live.
    fn value(&self, unit: Unit) -> Option<f64> {
        if let Some(r) = &self.result {
            return Some(match unit {
                Unit::MegabytesPerSec => r.bytes_per_sec / 1e6,
                Unit::Iops => r.iops,
            });
        }
        self.live.map(|l| match unit {
            Unit::MegabytesPerSec => l.bytes_per_sec / 1e6,
            Unit::Iops => l.iops,
        })
    }
}

enum Activity {
    Idle,
    Preparing,
    Cooldown { next: Cell, remaining: Duration },
    Running { cell: Cell },
}

struct App {
    unit: Unit,
    cells: HashMap<(usize, Op), CellView>,
    activity: Activity,
    outcome: Option<Outcome>,
    failure: Option<String>,
    /// Set once the prepare phase ends; drives the overall ETA.
    bench_started: Option<Instant>,
    planned: Duration,
    pal: Palette,
}

impl App {
    fn new(cfg: &Config, unit: Unit, color: ColorMode) -> Self {
        // Saturating arithmetic: absurd --duration values must not panic here.
        let ops = cfg.cells().len() as u32;
        let per_op = cfg
            .warmup
            .saturating_add(cfg.duration.saturating_mul(cfg.runs));
        let planned = per_op
            .saturating_mul(ops)
            .saturating_add(cfg.interval.saturating_mul(ops.saturating_sub(1)));
        App {
            unit,
            cells: HashMap::new(),
            activity: Activity::Idle,
            outcome: None,
            failure: None,
            bench_started: None,
            planned,
            pal: Palette::new(color),
        }
    }

    fn cell_mut(&mut self, cell: Cell) -> &mut CellView {
        self.cells.entry((cell.task, cell.op)).or_default()
    }

    fn mark_bench_started(&mut self) {
        if self.bench_started.is_none() {
            self.bench_started = Some(Instant::now());
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Preparing => self.activity = Activity::Preparing,
            Event::Cooldown { next, remaining } => {
                self.mark_bench_started();
                self.activity = Activity::Cooldown { next, remaining };
            }
            Event::Phase { cell, phase } => {
                self.mark_bench_started();
                self.activity = Activity::Running { cell };
                let view = self.cell_mut(cell);
                view.phase = Some(phase);
                view.live = None;
            }
            Event::Live { cell, rate } => self.cell_mut(cell).live = Some(rate),
            Event::RunDone { .. } => {}
            Event::OpDone { cell, result } => {
                let view = self.cell_mut(cell);
                view.result = Some(result);
                view.live = None;
                view.phase = None;
            }
            Event::Finished => {
                self.activity = Activity::Idle;
                self.outcome = Some(Outcome::Finished);
            }
            Event::Aborted => {
                self.activity = Activity::Idle;
                self.outcome = Some(Outcome::Aborted);
            }
            Event::Failed(message) => {
                self.failure = Some(message);
                self.outcome = Some(Outcome::Failed);
            }
        }
    }

    fn eta(&self) -> Option<Duration> {
        let started = self.bench_started?;
        Some(self.planned.saturating_sub(started.elapsed()))
    }

    /// Overall completion in [0, 1]: finished ops plus the current op's runs.
    fn progress(&self, cfg: &Config) -> f64 {
        if self.outcome == Some(Outcome::Finished) {
            return 1.0;
        }
        let total = cfg.cells().len() as f64;
        if total == 0.0 {
            return 0.0;
        }
        let done = self.cells.values().filter(|c| c.result.is_some()).count() as f64;
        let current = if let Activity::Running { cell } = self.activity {
            let view = self.cells.get(&(cell.task, cell.op));
            match view.and_then(|v| v.phase) {
                Some(Phase::Measuring { run, total: runs }) if runs > 0 => {
                    let pct = view
                        .and_then(|v| v.live)
                        .and_then(|l| l.percent)
                        .unwrap_or(0.0)
                        / 100.0;
                    (f64::from(run - 1) + pct.clamp(0.0, 1.0)) / f64::from(runs)
                }
                _ => 0.0,
            }
        } else {
            0.0
        };
        ((done + current) / total).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Theme

/// How much color the terminal can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// None at all: NO_COLOR, a `dumb` terminal, or `--color never`.
    None,
    /// The 16 ANSI colors, whose actual hues come from the user's theme.
    Ansi,
    /// The xterm 256-color palette — coarse, but still a gradient.
    Indexed,
    /// 24-bit RGB.
    TrueColor,
}

impl Depth {
    fn detect(mode: ColorMode) -> Self {
        Self::resolve(mode, |key| std::env::var(key).ok())
    }

    /// Detection has to be optimistic. `COLORTERM` is the only portable
    /// signal that a terminal speaks 24-bit color, and it is *local* to the
    /// emulator: ssh does not forward it (it is not in the default
    /// `SendEnv`), sudo strips it, and `docker run` never sets it. Treating
    /// its absence as "no truecolor" is what made this binary paint a smooth
    /// gradient on a local macOS terminal and two flat ANSI colors on the
    /// Linux box behind ssh. So assume 24-bit — like btop does — and only
    /// step down for terminals known not to handle it.
    ///
    /// `env` is injected so the policy can be unit-tested.
    fn resolve(mode: ColorMode, env: impl Fn(&str) -> Option<String>) -> Self {
        match mode {
            ColorMode::TrueColor => return Depth::TrueColor,
            ColorMode::Indexed => return Depth::Indexed,
            ColorMode::Ansi => return Depth::Ansi,
            ColorMode::Never => return Depth::None,
            ColorMode::Auto => {}
        }

        let var = |key: &str| env(key).filter(|v| !v.is_empty());
        // https://no-color.org — any non-empty value opts out. crossterm
        // enforces this at the write layer too; deciding it here as well keeps
        // the palette honest about what the user will actually see.
        if var("NO_COLOR").is_some() {
            return Depth::None;
        }

        let term = var("TERM").unwrap_or_default().to_ascii_lowercase();
        // TERM is normally unset under Windows consoles; anywhere else an
        // empty or `dumb` TERM means a terminal that cannot even do ANSI.
        if !cfg!(windows) && (term.is_empty() || term == "dumb") {
            return Depth::None;
        }

        let colorterm = var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Depth::TrueColor;
        }
        // terminfo's direct-color entries (xterm-direct, tmux-direct, …).
        if term.ends_with("-direct") {
            return Depth::TrueColor;
        }

        // The terminals that genuinely cannot do 24-bit:
        //   * the Linux/BSD text console — 16 colors and no palette to speak of
        if matches!(term.as_str(), "linux" | "console" | "ansi") || term.starts_with("vt") {
            return Depth::Ansi;
        }
        //   * Apple's Terminal.app — 256 colors, ignores `38;2` outright
        if var("TERM_PROGRAM").as_deref() == Some("Apple_Terminal") {
            return Depth::Indexed;
        }
        //   * GNU screen before 5.0 — swallows the sequence. tmux reports the
        //     same TERM but quantizes 24-bit input itself, so it is fine.
        if term.starts_with("screen") && var("TMUX").is_none() {
            return Depth::Indexed;
        }

        Depth::TrueColor
    }
}

/// btop-style palette: low-saturation gradients, quantized to whatever the
/// terminal can actually render.
#[derive(Clone, Copy)]
struct Palette {
    depth: Depth,
}

impl Palette {
    fn new(mode: ColorMode) -> Self {
        Palette {
            depth: Depth::detect(mode),
        }
    }

    /// `rgb` is the intended color, `ansi` the nearest of the 16 named ones.
    fn pick(self, rgb: (u8, u8, u8), ansi: Color) -> Color {
        match self.depth {
            Depth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            Depth::Indexed => Color::Indexed(xterm256(rgb)),
            Depth::Ansi => ansi,
            Depth::None => Color::Reset,
        }
    }

    fn border(self) -> Color {
        self.pick((68, 76, 72), Color::DarkGray)
    }
    fn title(self) -> Color {
        self.pick((152, 195, 121), Color::Green)
    }
    fn text(self) -> Color {
        self.pick((214, 220, 224), Color::White)
    }
    fn dim(self) -> Color {
        self.pick((124, 132, 132), Color::DarkGray)
    }
    fn live(self) -> Color {
        self.pick((229, 192, 123), Color::Yellow)
    }
    fn warn(self) -> Color {
        self.pick((198, 156, 88), Color::Yellow)
    }
    fn error(self) -> Color {
        self.pick((224, 108, 117), Color::Red)
    }
    fn track(self) -> Color {
        self.pick((54, 60, 57), Color::DarkGray)
    }

    /// Bar/progress gradient along `t` in [0, 1]; `dimmed` for live values.
    fn grad(self, t: f64, dimmed: bool) -> Color {
        let (r, g, b) = gradient(t);
        let f = if dimmed { 0.55 } else { 1.0 };
        let scale = |c: u8| (f64::from(c) * f) as u8;
        let ansi = if dimmed {
            Color::DarkGray
        } else if t > 0.7 {
            Color::Yellow
        } else {
            Color::Green
        };
        self.pick((scale(r), scale(g), scale(b)), ansi)
    }
}

/// Nearest xterm-256 index for an RGB triple. The candidates are the 6×6×6
/// color cube (16..232) and the 24-step gray ramp (232..), never 0..16 —
/// those are whatever the user's theme makes of them.
fn xterm256((r, g, b): (u8, u8, u8)) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| l.abs_diff(v))
            .map_or(0, |(i, _)| i)
    };
    let dist = |c: (u8, u8, u8)| {
        let d = |x: u8, y: u8| i32::from(x.abs_diff(y)).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };

    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);

    // Ramp shades are 8, 18, … 238; subtracting 3 makes the division round.
    let avg = usize::from((u16::from(r) + u16::from(g) + u16::from(b)) / 3);
    let ramp = (avg.saturating_sub(3) / 10).min(23);
    let shade = (8 + ramp * 10) as u8;

    if dist((shade, shade, shade)) < dist(cube) {
        232 + ramp as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

/// Three-stop gradient: muted green → light green → amber.
fn gradient(t: f64) -> (u8, u8, u8) {
    const STOPS: [(f64, (u8, u8, u8)); 3] = [
        (0.0, (74, 130, 92)),
        (0.55, (140, 194, 112)),
        (1.0, (226, 196, 120)),
    ];
    let t = t.clamp(0.0, 1.0);
    let (mut lo, mut hi) = (STOPS[0], STOPS[1]);
    if t > STOPS[1].0 {
        (lo, hi) = (STOPS[1], STOPS[2]);
    }
    let span = (hi.0 - lo.0).max(f64::EPSILON);
    let k = (t - lo.0) / span;
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * k) as u8;
    (
        mix(lo.1.0, hi.1.0),
        mix(lo.1.1, hi.1.1),
        mix(lo.1.2, hi.1.2),
    )
}

// ---------------------------------------------------------------------------
// Rendering

fn draw(
    frame: &mut Frame<'_>,
    app: &App,
    cfg: &Config,
    disk: Option<&DiskInfo>,
    warnings: &[String],
) {
    let area = frame.area();
    let width = area.width.min(MAX_WIDTH);
    let x = area.x + (area.width.saturating_sub(width)) / 2;

    let tasks = cfg.tasks.len() as u16;
    let header_h = 2 + warnings.len() as u16 + 2;
    let status_h = 3;
    // One row per task, plus the column headings and their rule line.
    let results_h = tasks * 2 + 3;

    let total_h = (header_h + results_h + status_h).min(area.height);
    let content = Rect::new(x, area.y, width, total_h);
    let chunks = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Length(results_h),
        Constraint::Length(status_h),
    ])
    .split(content);

    draw_header(frame, chunks[0], app, cfg, disk, warnings);
    draw_results(frame, chunks[1], app, cfg);
    draw_status(frame, chunks[2], app, cfg);
}

fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    cfg: &Config,
    disk: Option<&DiskInfo>,
    warnings: &[String],
) {
    let pal = app.pal;
    let block = panel(pal).title(
        Line::from(format!(" iomark {} ", crate::version()))
            .fg(pal.title())
            .bold(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let mut lines = vec![match disk {
        Some(d) => {
            let free = human_bytes(d.available);
            let total = human_bytes(d.total);
            // Squeeze the identity so the capacity facts always stay visible.
            let fixed = 5 + 5 + 9 + free.chars().count() + total.chars().count();
            let label = truncate(&d.label(), width.saturating_sub(fixed));
            Line::from(vec![
                Span::styled("disk ", Style::new().fg(pal.dim())),
                Span::styled(label, Style::new().fg(pal.text())),
                Span::styled("  ·  ", Style::new().fg(pal.dim())),
                Span::styled(free, Style::new().fg(pal.text())),
                Span::styled(" free of ", Style::new().fg(pal.dim())),
                Span::styled(total, Style::new().fg(pal.text())),
            ])
        }
        None => Line::from(vec![
            Span::styled("target ", Style::new().fg(pal.dim())),
            Span::styled(
                shorten_path(&cfg.target, width.saturating_sub(8)),
                Style::new().fg(pal.text()),
            ),
        ]),
    }];
    lines.push(
        Line::from(format!(
            "file {}  ·  {} runs × {}  ·  warmup {}  ·  interval {}",
            human_bytes(cfg.size),
            cfg.runs,
            fmt_dur(cfg.duration),
            fmt_dur(cfg.warmup),
            fmt_dur(cfg.interval),
        ))
        .fg(pal.dim()),
    );
    for warning in warnings {
        lines.push(
            Line::from(truncate(&format!("⚠ {warning}"), inner.width as usize)).fg(pal.warn()),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_results(frame: &mut Frame<'_>, area: Rect, app: &App, cfg: &Config) {
    let pal = app.pal;
    let block = panel(pal).title(Line::from(" results ").fg(pal.title()).bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 || inner.width < 40 {
        return;
    }

    // CDM grid, one row per task: label | Read cell | Write cell, with
    // rule lines separating the columns.
    let label_w: u16 = 16;
    let sep1 = inner.x + label_w;
    let col_w = inner.width.saturating_sub(label_w + 2) / 2;
    let read_x = sep1 + 1;
    let sep2 = read_x + col_w;
    let write_x = sep2 + 1;

    let buf = frame.buffer_mut();
    let head = Style::new().fg(pal.title()).bold();
    let unit = app.unit.heading();
    put_centered(
        buf,
        inner,
        read_x,
        inner.y,
        col_w,
        &format!("Read ({unit})"),
        head,
    );
    put_centered(
        buf,
        inner,
        write_x,
        inner.y,
        col_w,
        &format!("Write ({unit})"),
        head,
    );

    // Column separators over the full panel height, then horizontal rules
    // under the headings and between task rows, with `┼` junctions.
    let line_style = Style::new().fg(pal.border());
    for y in inner.y..inner.bottom() {
        for x in [sep1, sep2] {
            put(buf, inner, x, y, "│", line_style);
        }
    }
    let rule_ys = (0..cfg.tasks.len() as u16).map(|i| inner.y + 1 + i * 2);
    for ry in rule_ys {
        if ry >= inner.bottom() {
            break;
        }
        for x in inner.x..inner.right() {
            let symbol = if x == sep1 || x == sep2 { "┼" } else { "─" };
            put(buf, inner, x, ry, symbol, line_style);
        }
    }

    // CDM-style bars: each column scales against its own best value.
    let col_max = |op: Op| -> f64 {
        (0..cfg.tasks.len())
            .filter_map(|i| app.cells.get(&(i, op)).and_then(|c| c.value(app.unit)))
            .fold(0.0, f64::max)
    };
    let columns = [
        (Op::Read, read_x, col_max(Op::Read)),
        (Op::Write, write_x, col_max(Op::Write)),
    ];

    for (i, spec) in cfg.tasks.iter().enumerate() {
        let y = inner.y + 2 + i as u16 * 2;
        if y >= inner.bottom() {
            break;
        }
        let task_active = app.outcome.is_none()
            && matches!(app.activity, Activity::Running { cell } if cell.task == i);
        if task_active {
            put(
                buf,
                inner,
                inner.x,
                y,
                "▶",
                Style::new().fg(pal.live()).bold(),
            );
        }
        let (name, qt) = spec.label();
        put(
            buf,
            inner,
            inner.x + 2,
            y,
            &name,
            Style::new().fg(pal.text()).bold(),
        );
        put(
            buf,
            inner,
            inner.x + 2 + name.chars().count() as u16 + 1,
            y,
            &qt,
            Style::new().fg(pal.dim()),
        );
        for (op, x, max) in columns {
            let cell = Cell { task: i, op };
            draw_cell(buf, inner, Rect::new(x, y, col_w, 1), app, cell, max);
        }
    }
}

/// One single-line CDM cell: gradient bar on the left, number on the right
/// (in the IOPS view, mean latency sits next to the number).
fn draw_cell(buf: &mut Buffer, bounds: Rect, area: Rect, app: &App, cell: Cell, col_max: f64) {
    let pal = app.pal;
    let view = app.cells.get(&(cell.task, cell.op));
    let active =
        app.outcome.is_none() && matches!(app.activity, Activity::Running { cell: c } if c == cell);
    let done = view.is_some_and(|v| v.result.is_some());
    let value = view.and_then(|v| v.value(app.unit));

    let value_text = match value {
        Some(v) => fmt_value(app.unit, v),
        None => "–".into(),
    };
    let value_style = if done {
        Style::new().fg(pal.text()).bold()
    } else if value.is_some() {
        Style::new().fg(pal.live())
    } else {
        Style::new().fg(pal.dim())
    };
    // Fixed-width, right-aligned numbers keep the latency column tidy.
    let vx = area.right().saturating_sub(9);
    put(
        buf,
        bounds,
        vx,
        area.y,
        &format!("{value_text:>8}"),
        value_style,
    );

    // Latency belongs to the IOPS view only, keeping the MB/s view clean.
    let mut info_x = vx;
    if app.unit == Unit::Iops
        && let Some(result) = view.and_then(|v| v.result.as_ref())
    {
        let lat = fmt_latency(result.lat_us);
        let lx = vx.saturating_sub(lat.chars().count() as u16 + 2);
        if lx > area.x + 4 {
            put(buf, bounds, lx, area.y, &lat, Style::new().fg(pal.dim()));
            info_x = lx;
        }
    }

    let bar_w = info_x.saturating_sub(area.x + 3);
    if bar_w >= 3 {
        let frac = match value {
            Some(v) if col_max > 0.0 => v / col_max,
            _ => 0.0,
        };
        let dimmed = !done && (active || value.is_some());
        draw_bar(
            buf,
            Rect::new(area.x + 1, area.y, bar_w, 1),
            frac,
            pal,
            dimmed,
        );
    }
}

/// Text centered within a column starting at `x`.
fn put_centered(
    buf: &mut Buffer,
    bounds: Rect,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) {
    let len = text.chars().count() as u16;
    let cx = x + width.saturating_sub(len) / 2;
    put(buf, bounds, cx, y, text, style);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App, cfg: &Config) {
    let pal = app.pal;
    let key = |k: &str, label: &str| {
        vec![
            Span::styled(format!(" {k} "), Style::new().fg(pal.title()).bold()),
            Span::styled(format!("{label} "), Style::new().fg(pal.dim())),
        ]
    };
    let mut keys = key("K", "unit");
    keys.extend(key("Q", "quit"));
    let block = panel(pal)
        .title(Line::from(" status ").fg(pal.title()).bold())
        .title_bottom(Line::from(keys).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let text = status_line(app, cfg, pal);
    // Right side: overall progress bar + percentage + ETA.
    let right_w: u16 = if inner.width >= 64 { 36 } else { 0 };
    let left = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(right_w + 2),
        1,
    );
    frame.render_widget(Paragraph::new(text), left);

    if right_w > 0 && app.outcome.is_none() {
        let progress = app.progress(cfg);
        let bar = Rect::new(inner.right() - right_w, inner.y, 16, 1);
        draw_bar(frame.buffer_mut(), bar, progress, pal, false);
        let mut tail = format!(" {:>3.0}%", progress * 100.0);
        if let Some(eta) = app.eta() {
            let secs = eta.as_secs();
            tail.push_str(&format!(" · ~{}:{:02} left", secs / 60, secs % 60));
        }
        put(
            frame.buffer_mut(),
            inner,
            bar.right(),
            inner.y,
            &tail,
            Style::new().fg(pal.dim()),
        );
    }
}

fn status_line(app: &App, cfg: &Config, pal: Palette) -> Line<'static> {
    if let Some(failure) = &app.failure {
        return Line::from(Span::styled(
            format!("error: {failure}"),
            Style::new().fg(pal.error()),
        ));
    }
    if app.outcome == Some(Outcome::Finished) {
        return Line::from(Span::styled(
            "complete — press Q to exit",
            Style::new().fg(pal.title()).bold(),
        ));
    }
    if app.outcome == Some(Outcome::Aborted) {
        return Line::from(Span::styled("aborted", Style::new().fg(pal.warn())));
    }
    match &app.activity {
        Activity::Idle => Line::from(Span::styled("starting…", Style::new().fg(pal.dim()))),
        Activity::Preparing => Line::from(vec![
            Span::styled("preparing test file ", Style::new().fg(pal.text())),
            Span::styled(
                format!("({})", human_bytes(cfg.size)),
                Style::new().fg(pal.dim()),
            ),
        ]),
        Activity::Cooldown { next, remaining } => Line::from(vec![
            Span::styled("next ", Style::new().fg(pal.dim())),
            Span::styled(
                format!("{} {}", cfg.tasks[next.task], next.op.name()),
                Style::new().fg(pal.text()),
            ),
            Span::styled(
                format!(" in {}s", remaining.as_secs() + 1),
                Style::new().fg(pal.dim()),
            ),
        ]),
        Activity::Running { cell } => {
            let phase = app
                .cells
                .get(&(cell.task, cell.op))
                .and_then(|v| v.phase)
                .map(|phase| match phase {
                    Phase::Warmup => " · warmup".to_owned(),
                    Phase::Measuring { run, total } => format!(" · run {run}/{total}"),
                })
                .unwrap_or_default();
            Line::from(vec![
                Span::styled("▶ ", Style::new().fg(pal.live()).bold()),
                Span::styled(
                    format!("{} {}", cfg.tasks[cell.task], cell.op.name()),
                    Style::new().fg(pal.text()),
                ),
                Span::styled(phase, Style::new().fg(pal.dim())),
            ])
        }
    }
}

// ---------------------------------------------------------------------------
// Primitives

fn panel(pal: Palette) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(pal.border()))
}

const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// A one-line ratio bar with sub-cell precision and a per-cell gradient;
/// the unfilled remainder renders as a dotted track.
fn draw_bar(buf: &mut Buffer, area: Rect, frac: f64, pal: Palette, dimmed: bool) {
    if area.width == 0 {
        return;
    }
    let w = area.width as usize;
    let filled8 = (frac.clamp(0.0, 1.0) * w as f64 * 8.0).round() as usize;
    for i in 0..w {
        let x = area.x + i as u16;
        let t = i as f64 / (w.saturating_sub(1).max(1)) as f64;
        let start8 = i * 8;
        let (symbol, style) = if start8 + 8 <= filled8 {
            ('█', Style::new().fg(pal.grad(t, dimmed)))
        } else if start8 < filled8 {
            (
                EIGHTHS[filled8 - start8 - 1],
                Style::new().fg(pal.grad(t, dimmed)),
            )
        } else {
            ('·', Style::new().fg(pal.track()))
        };
        buf.set_string(x, area.y, symbol.to_string(), style);
    }
}

/// Bounds-checked string draw, clipped to `bounds`.
fn put(buf: &mut Buffer, bounds: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y >= bounds.bottom() || x >= bounds.right() {
        return;
    }
    let max = (bounds.right() - x) as usize;
    buf.set_stringn(x, y, text, max, style);
}

fn fmt_value(unit: Unit, value: f64) -> String {
    match unit {
        _ if value >= 10_000.0 => format!("{value:.0}"),
        Unit::MegabytesPerSec => format!("{value:.2}"),
        Unit::Iops => format!("{value:.1}"),
    }
}

fn fmt_latency(lat_us: f64) -> String {
    if lat_us >= 1000.0 {
        format!("{:.2} ms", lat_us / 1000.0)
    } else {
        format!("{lat_us:.1} µs")
    }
}

fn fmt_dur(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Char-aware truncation with an ellipsis.
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Fits a path into `max` chars, keeping the (more informative) tail.
fn shorten_path(path: &std::path::Path, max: usize) -> String {
    let s = path.display().to_string();
    let count = s.chars().count();
    if count <= max {
        return s;
    }
    let tail: String = s.chars().skip(count + 1 - max.max(1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn depth(mode: ColorMode, env: &[(&str, &str)]) -> Depth {
        Depth::resolve(mode, |key| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    #[test]
    fn a_terminal_that_hides_colorterm_still_gets_truecolor() {
        // The regression this guards: ssh does not forward COLORTERM, so the
        // same binary drew a gradient locally and two flat colors remotely.
        assert_eq!(
            depth(ColorMode::Auto, &[("TERM", "xterm-256color")]),
            Depth::TrueColor
        );
        assert_eq!(
            depth(ColorMode::Auto, &[("TERM", "xterm")]),
            Depth::TrueColor
        );
    }

    #[test]
    fn takes_the_explicit_truecolor_signals() {
        assert_eq!(
            depth(
                ColorMode::Auto,
                &[("TERM", "xterm"), ("COLORTERM", "TrueColor")]
            ),
            Depth::TrueColor
        );
        assert_eq!(
            depth(ColorMode::Auto, &[("TERM", "xterm-direct")]),
            Depth::TrueColor
        );
    }

    #[test]
    fn steps_down_for_terminals_without_24_bit() {
        assert_eq!(depth(ColorMode::Auto, &[("TERM", "linux")]), Depth::Ansi);
        assert_eq!(
            depth(
                ColorMode::Auto,
                &[
                    ("TERM", "xterm-256color"),
                    ("TERM_PROGRAM", "Apple_Terminal")
                ]
            ),
            Depth::Indexed
        );
        assert_eq!(
            depth(ColorMode::Auto, &[("TERM", "screen-256color")]),
            Depth::Indexed
        );
        // tmux reports the same TERM but quantizes 24-bit input itself.
        assert_eq!(
            depth(
                ColorMode::Auto,
                &[("TERM", "screen-256color"), ("TMUX", "/tmp/tmux-0/default")]
            ),
            Depth::TrueColor
        );
    }

    #[test]
    fn no_color_wins_over_detection_but_loses_to_the_flag() {
        assert_eq!(
            depth(
                ColorMode::Auto,
                &[("TERM", "xterm-256color"), ("NO_COLOR", "1")]
            ),
            Depth::None
        );
        assert_eq!(
            depth(ColorMode::TrueColor, &[("NO_COLOR", "1")]),
            Depth::TrueColor
        );
        // An empty NO_COLOR does not count as set (no-color.org).
        assert_eq!(
            depth(
                ColorMode::Auto,
                &[("TERM", "xterm-256color"), ("NO_COLOR", "")]
            ),
            Depth::TrueColor
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn a_dumb_terminal_gets_no_color() {
        assert_eq!(depth(ColorMode::Auto, &[("TERM", "dumb")]), Depth::None);
        assert_eq!(depth(ColorMode::Auto, &[]), Depth::None);
    }

    #[test]
    fn quantizes_to_the_xterm_cube_and_gray_ramp() {
        assert_eq!(xterm256((0, 0, 0)), 16);
        assert_eq!(xterm256((255, 255, 255)), 231);
        assert_eq!(xterm256((255, 0, 0)), 196);
        assert_eq!(xterm256((128, 128, 128)), 244);
    }

    #[test]
    fn the_gradient_survives_the_256_color_fallback() {
        let pal = Palette {
            depth: Depth::Indexed,
        };
        let steps: HashSet<Color> = (0..=20)
            .map(|i| pal.grad(f64::from(i) / 20.0, false))
            .collect();
        assert!(
            steps.len() >= 4,
            "gradient collapsed to {} colors",
            steps.len()
        );
    }
}
