//! Generates the README benchmark charts (`docs/assets/bench-*.svg`) from the
//! `bench_real.json` report that the `bench_real` example writes:
//!
//! ```text
//! cargo run --release -p ltk_hashdb --example bench_real -- data/cdragon data/build
//! cargo run -p ltk_hashdb --example gen_charts -- [data/build/bench_real.json]
//! ```
//!
//! The arena-layout chart is the exception: its numbers come from the
//! `compression_lab` study and live in [`ARENA`] below.
//!
//! Emits a light and a dark variant of each chart; the README embeds them with
//! `<picture>` so GitHub picks the one matching the viewer's theme. Backgrounds
//! are transparent - the charts sit directly on GitHub's page surface.

use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use plotters::coord::Shift;
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};

#[path = "../utils/mod.rs"]
mod utils;
use utils::{group_thousands, mib, BenchReport};

type Area<'a> = DrawingArea<SVGBackend<'a>, Shift>;
type ChartResult = Result<(), Box<dyn Error>>;

const FONT: &str = "-apple-system, 'Segoe UI', system-ui, sans-serif";

/// Date of the CommunityDragon txt snapshot the report was measured against
/// (shown in the size chart's subtitle - update alongside the data).
const SNAPSHOT_DATE: &str = "2026-07-07";

/// The frame size the latency chart reads from the sweep - keep in sync with
/// `bench_real`'s `DEFAULT_FRAME_SIZE`.
const CHART_FRAME_SIZE: u32 = 16 << 10;

/// Tables that get their own row in the size chart; the rest fold into
/// "other".
const FEATURED_TABLES: [&str; 3] = ["game", "binentries", "lcu"];

/// Colors for one render mode, validated against GitHub's light `#ffffff` /
/// dark `#0d1117` surfaces (contrast >= 3:1, CVD dE ~55 between accent and
/// context gray).
struct Theme {
    /// Filename suffix (`""` for light, `"-dark"` for dark).
    suffix: &'static str,

    /// Series color for the mimir-side bars.
    accent: RGBColor,

    /// De-emphasis gray for baseline/context bars.
    context: RGBColor,

    /// Primary ink (titles).
    ink: RGBColor,

    /// Secondary ink (row and value labels, legend).
    secondary: RGBColor,

    /// Muted ink (subtitles).
    muted: RGBColor,

    /// Hairline axis rule.
    baseline: RGBColor,
}

const LIGHT: Theme = Theme {
    suffix: "",
    accent: RGBColor(0x2a, 0x78, 0xd6),
    context: RGBColor(0x89, 0x87, 0x81),
    ink: RGBColor(0x1f, 0x23, 0x28),
    secondary: RGBColor(0x52, 0x51, 0x4e),
    muted: RGBColor(0x89, 0x87, 0x81),
    baseline: RGBColor(0xc3, 0xc2, 0xb7),
};

const DARK: Theme = Theme {
    suffix: "-dark",
    accent: RGBColor(0x39, 0x87, 0xe5),
    context: RGBColor(0x89, 0x87, 0x81),
    ink: RGBColor(0xe6, 0xed, 0xf3),
    secondary: RGBColor(0xc3, 0xc2, 0xb7),
    muted: RGBColor(0x89, 0x87, 0x81),
    baseline: RGBColor(0x44, 0x44, 0x3f),
};

/// Compressed arena size, MiB - game string arena (162.5 MiB raw), level 19.
/// From the `compression_lab` study (see docs/BENCHMARKS.md); not part of the
/// `bench_real` report, so kept inline. The `bool` marks the layout mimir
/// ships (accent color).
const ARENA: [(&str, f64, bool); 4] = [
    ("key-order arena", 45.7, false),
    ("key-order + trained dict", 30.0, false),
    ("solid stream (no seeking)", 17.5, false),
    ("path-order arena (mimir)", 10.4, true),
];

/// One row of the size chart, in bytes.
struct SizeRow {
    label: String,
    txt_len: u64,
    db_len: u64,
}

/// One row of the latency chart.
struct LatencyRow {
    label: &'static str,
    ns: u64,
}

// ---------------------------------------------------------------------------
// Drawing helpers.

fn text_style(size: i32, color: RGBColor, h: HPos, v: VPos) -> TextStyle<'static> {
    (FONT, size).into_font().color(&color).pos(Pos::new(h, v))
}

/// Horizontal bar growing right from `(x, y)`: square at the left baseline,
/// pill-rounded data-end on the right.
fn draw_bar(root: &Area, x: i32, y: i32, w: i32, h: i32, color: RGBColor) -> ChartResult {
    let r = h / 2;
    if w > r {
        root.draw(&Rectangle::new(
            [(x, y), (x + w - r, y + h)],
            color.filled(),
        ))?;
        root.draw(&Circle::new((x + w - r, y + r), r, color.filled()))?;
    } else {
        root.draw(&Rectangle::new(
            [(x, y), (x + w.max(1), y + h)],
            color.filled(),
        ))?;
    }

    Ok(())
}

/// Chart title + subtitle, plus an optional right-aligned swatch legend on
/// the title row.
fn draw_header(
    root: &Area,
    t: &Theme,
    width: i32,
    title: &str,
    subtitle: &str,
    legend: &[(&str, RGBColor)],
) -> ChartResult {
    let bold = (FONT, 14)
        .into_font()
        .style(FontStyle::Bold)
        .color(&t.ink)
        .pos(Pos::new(HPos::Left, VPos::Top));
    root.draw(&Text::new(title, (0, 4), bold))?;
    root.draw(&Text::new(
        subtitle,
        (0, 25),
        text_style(11, t.muted, HPos::Left, VPos::Top),
    ))?;

    let mut x = width;
    for (label, color) in legend.iter().rev() {
        root.draw(&Text::new(
            *label,
            (x, 12),
            text_style(11, t.secondary, HPos::Right, VPos::Center),
        ))?;
        x -= (label.len() as f64 * 6.2) as i32 + 6;
        root.draw(&Rectangle::new([(x - 10, 7), (x, 17)], color.filled()))?;
        x -= 26;
    }

    Ok(())
}

/// Hairline vertical rule the bars grow from.
fn draw_axis(root: &Area, t: &Theme, x: i32, top: i32, bottom: i32) -> ChartResult {
    root.draw(&PathElement::new(
        vec![(x, top - 4), (x, bottom + 4)],
        t.baseline.stroke_width(1),
    ))?;

    Ok(())
}

fn row_label(root: &Area, t: &Theme, x: i32, y: i32, label: &str) -> ChartResult {
    root.draw(&Text::new(
        label,
        (x, y),
        text_style(12, t.secondary, HPos::Right, VPos::Center),
    ))?;

    Ok(())
}

fn value_label(root: &Area, t: &Theme, x: i32, y: i32, label: &str) -> ChartResult {
    root.draw(&Text::new(
        label,
        (x, y),
        text_style(11, t.secondary, HPos::Left, VPos::Center),
    ))?;

    Ok(())
}

/// Latency label: ns below 1 µs, µs with one decimal above.
fn fmt_latency(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns} ns")
    } else {
        format!("{:.1} µs", ns as f64 / 1e3)
    }
}

const MIB: f64 = (1 << 20) as f64;

// ---------------------------------------------------------------------------
// Chart 1 - disk footprint, paired bars (txt = context gray, .hashdb = accent).

fn chart_sizes(t: &Theme, out: &Path, rows: &[SizeRow], subtitle: &str) -> ChartResult {
    const GUTTER: i32 = 130;
    const PLOT: f64 = 440.0;
    const BAR: i32 = 14;
    const ROW: i32 = BAR * 2 + 2 + 16; // paired bars + 2px gap + row spacing
    const TOP: i32 = 56;

    let height = (TOP + rows.len() as i32 * ROW) as u32;
    let root = SVGBackend::new(out, (720, height)).into_drawing_area();
    draw_header(
        &root,
        t,
        720,
        "Disk footprint",
        subtitle,
        &[("hashes.*.txt", t.context), (".hashdb (zstd)", t.accent)],
    )?;

    // Scale so the longest txt bar spans the plot, on a clean 10 MiB boundary.
    let max_txt = rows.iter().map(|r| r.txt_len).max().unwrap_or(1) as f64 / MIB;
    let max_mib = (max_txt / 10.0).ceil() * 10.0;
    let scale = |bytes: u64| (bytes as f64 / MIB / max_mib * PLOT) as i32;

    for (i, row) in rows.iter().enumerate() {
        let y = TOP + i as i32 * ROW;
        let pct = 100.0 * row.db_len as f64 / row.txt_len as f64;
        row_label(&root, t, GUTTER - 10, y + BAR + 1, &row.label)?;

        draw_bar(&root, GUTTER, y, scale(row.txt_len), BAR, t.context)?;
        value_label(
            &root,
            t,
            GUTTER + scale(row.txt_len) + 6,
            y + BAR / 2,
            &mib(row.txt_len),
        )?;

        let y_db = y + BAR + 2;
        draw_bar(&root, GUTTER, y_db, scale(row.db_len), BAR, t.accent)?;
        value_label(
            &root,
            t,
            GUTTER + scale(row.db_len) + 6,
            y_db + BAR / 2,
            &format!("{} ({pct:.0}%)", mib(row.db_len)),
        )?;
    }

    draw_axis(&root, t, GUTTER, TOP, TOP + rows.len() as i32 * ROW - 16)?;
    root.present()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Chart 2 - lookup latency, single series (accent).

fn chart_latency(
    t: &Theme,
    out: &Path,
    rows: &[LatencyRow],
    title: &str,
    subtitle: &str,
) -> ChartResult {
    const GUTTER: i32 = 170;
    const PLOT: f64 = 430.0;
    const BAR: i32 = 16;
    const ROW: i32 = BAR + 18;
    const TOP: i32 = 56;

    let height = (TOP + rows.len() as i32 * ROW - 2) as u32;
    let root = SVGBackend::new(out, (720, height)).into_drawing_area();
    draw_header(&root, t, 720, title, subtitle, &[])?;

    let max_ns = rows.iter().map(|r| r.ns).max().unwrap_or(1) as f64 * 1.06;
    for (i, row) in rows.iter().enumerate() {
        let y = TOP + i as i32 * ROW;
        let w = (row.ns as f64 / max_ns * PLOT) as i32;
        row_label(&root, t, GUTTER - 10, y + BAR / 2, row.label)?;
        draw_bar(&root, GUTTER, y, w, BAR, t.accent)?;
        value_label(&root, t, GUTTER + w + 6, y + BAR / 2, &fmt_latency(row.ns))?;
    }

    draw_axis(&root, t, GUTTER, TOP, TOP + rows.len() as i32 * ROW - 18)?;
    root.present()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Chart 3 - arena ordering, emphasis (path order = accent, rest = context).

fn chart_arena(t: &Theme, out: &Path) -> ChartResult {
    const GUTTER: i32 = 200;
    const PLOT: f64 = 400.0;
    const BAR: i32 = 16;
    const ROW: i32 = BAR + 18;
    const TOP: i32 = 56;
    const MAX_MIB: f64 = 48.0;

    let root = SVGBackend::new(out, (720, 190)).into_drawing_area();
    draw_header(
        &root,
        t,
        720,
        "String-arena compression by layout",
        "game table arena, 162.5 MiB of raw paths · zstd level 19, 16 KiB seekable frames · lower is better",
        &[],
    )?;

    for (i, (label, mib, is_mimir)) in ARENA.iter().enumerate() {
        let y = TOP + i as i32 * ROW;
        let w = (mib / MAX_MIB * PLOT) as i32;
        let color = if *is_mimir { t.accent } else { t.context };
        row_label(&root, t, GUTTER - 10, y + BAR / 2, label)?;
        draw_bar(&root, GUTTER, y, w, BAR, color)?;
        value_label(
            &root,
            t,
            GUTTER + w + 6,
            y + BAR / 2,
            &format!("{mib:.1} MiB"),
        )?;
    }

    draw_axis(&root, t, GUTTER, TOP, TOP + ARENA.len() as i32 * ROW - 18)?;
    root.present()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Report -> chart data.

fn size_rows(report: &BenchReport) -> Vec<SizeRow> {
    let mut rows: Vec<SizeRow> = FEATURED_TABLES
        .iter()
        .filter_map(|name| report.tables.iter().find(|t| t.table == *name))
        .map(|t| SizeRow {
            label: t.table.clone(),
            txt_len: t.txt_len,
            db_len: t.zstd_len,
        })
        .collect();

    let rest: Vec<_> = report
        .tables
        .iter()
        .filter(|t| !FEATURED_TABLES.contains(&t.table.as_str()))
        .collect();
    if !rest.is_empty() {
        rows.push(SizeRow {
            label: format!("other ({} tables)", rest.len()),
            txt_len: rest.iter().map(|t| t.txt_len).sum(),
            db_len: rest.iter().map(|t| t.zstd_len).sum(),
        });
    }

    rows
}

fn main() -> ChartResult {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("data/build/bench_real.json"));

    let file = File::open(&report_path).map_err(|e| {
        format!(
            "cannot read {}: {e}\nrun `cargo run --release -p ltk_hashdb \
             --example bench_real -- data/cdragon data/build` first",
            report_path.display()
        )
    })?;
    let report: BenchReport = serde_json::from_reader(BufReader::new(file))?;

    // Size chart: featured tables + folded rest, subtitle from the totals.
    let sizes = size_rows(&report);
    let total_entries: u64 = report.tables.iter().map(|t| t.entries).sum();
    let size_subtitle = format!(
        "{} CommunityDragon tables, ~{:.2} M entries · snapshot {SNAPSHOT_DATE} · lower is better",
        report.tables.len(),
        (total_entries as f64 / 1e4).floor() / 100.0,
    );

    // Latency chart: the game sweep row at the default frame size.
    let sweep = report
        .sweeps
        .iter()
        .find(|s| s.table == "game")
        .ok_or("no `game` sweep in report")?;
    let row = sweep
        .rows
        .iter()
        .find(|r| r.frame_size == Some(CHART_FRAME_SIZE))
        .ok_or("no 16 KiB row in the `game` sweep")?;
    let latency = [
        LatencyRow {
            label: "point hit (get)",
            ns: row.hit_ns,
        },
        LatencyRow {
            label: "batch hit (get_batch)",
            ns: row.batch_hit_ns,
        },
        LatencyRow {
            label: "miss",
            ns: row.miss_ns,
        },
    ];
    let latency_title = format!(
        "Per-lookup latency - game table ({} entries)",
        group_thousands(sweep.entries)
    );
    let latency_subtitle = format!(
        "zstd .hashdb, {} KiB frames, warm page cache · a miss never touches string data · lower is better",
        CHART_FRAME_SIZE >> 10
    );

    let assets = workspace.join("docs/assets");
    for t in [&LIGHT, &DARK] {
        chart_sizes(
            t,
            &assets.join(format!("bench-size{}.svg", t.suffix)),
            &sizes,
            &size_subtitle,
        )?;
        chart_latency(
            t,
            &assets.join(format!("bench-latency{}.svg", t.suffix)),
            &latency,
            &latency_title,
            &latency_subtitle,
        )?;
        chart_arena(t, &assets.join(format!("bench-arena{}.svg", t.suffix)))?;
    }

    println!("wrote 6 charts to {}", assets.display());
    Ok(())
}
