//! cargo-pulse: abandonment risk for crates.io packages.
//!
//! Pipeline:
//!   1. crates.io  - exists? current stats
//!   2. OSV/RustSec - declared unmaintained? -> report the fact, stop
//!   3. forge      - archived / deleted?     -> report the fact, stop
//!   4. age >= 18 months? else "too young to assess"
//!   5. model: live features -> bagged Cox score -> registry percentile
//!      -> gold-calibrated P(abandoned within ~2y) with (score,age) band
//!
//! The report is authored:
//!   default      rendered in-terminal (termimad: headings, bullets)
//!   --markdown   raw markdown (glow-compatible)
//!   --json       machine-readable (CI gates)
//!
//! Usage:  cargo-pulse <crate> [--markdown|--json]   (also: cargo pulse)
//!         cargo-pulse audit [path] [--detail] [--out <f>] [--markdown|--json]
//!         cargo-pulse --parity                      (train/serve harness)
//!         env: GITHUB_TOKEN (optional; needed once the 60/h anonymous limit hits), CARGO_PULSE_DATA (artifact dir)

mod audit;
mod audit_report;
mod cache;
mod data;
mod features;
mod forge;
mod net;
mod report;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

enum Mode {
    Pretty,
    Markdown,
    Json,
}

/// Verdict severity - drives the accent color of the rendered report.
#[derive(Clone, Copy)]
enum Sev {
    Red,    // facts (declared/archived) and high risk
    Yellow, // uncertain, elevated, too-young
    Green,  // low risk
    Plain,  // neutral (not found etc.)
}

fn skin_for(sev: Sev) -> termimad::MadSkin {
    use std::io::IsTerminal;
    use termimad::crossterm::style::Color::*;
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        return termimad::MadSkin::no_style();
    }
    let mut s = termimad::MadSkin::default();
    let accent = match sev {
        Sev::Red => Red,
        Sev::Yellow => Yellow,
        Sev::Green => Green,
        Sev::Plain => Cyan,
    };
    s.bold.set_fg(accent);
    s.inline_code.set_fg(accent);
    s.italic.set_fg(AnsiValue(245));
    for h in s.headers.iter_mut() {
        h.set_fg(Cyan);
    }
    s
}


fn stage(label: &str) -> indicatif::ProgressBar {
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.cyan} {prefix:<12} {msg}")
            .unwrap(),
    );
    pb.set_prefix(label.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(90));
    pb
}

fn done(pb: indicatif::ProgressBar, msg: String) {
    pb.set_style(indicatif::ProgressStyle::with_template("{prefix:<14} {msg}").unwrap());
    pb.set_prefix(format!("✓ {}", pb.prefix()));
    pb.finish_with_message(msg);
}

/// An explicit artifact directory, if one is available: `CARGO_PULSE_DATA`,
/// a local `./data`, or an executable-relative `data/` (dev layout). Returns
/// `None` when the tool should fall back to its embedded artifacts.
fn artifact_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CARGO_PULSE_DATA") {
        return Some(PathBuf::from(d));
    }
    let local = PathBuf::from("data");
    if local.join("meta.json").exists() {
        return Some(local);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(|r| r.join("data")))
        .filter(|d| d.join("meta.json").exists())
}

fn emit(mode: &Mode, md: &str, j: &serde_json::Value, sev: Sev) {
    match mode {
        Mode::Json => println!("{}", serde_json::to_string_pretty(j).unwrap()),
        Mode::Markdown => println!("{md}"),
        Mode::Pretty => skin_for(sev).print_text(md),
    }
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("pulse") {
        args.remove(0);
    }
    let mode = if args.iter().any(|a| a == "--json") {
        Mode::Json
    } else if args.iter().any(|a| a == "--markdown") {
        Mode::Markdown
    } else {
        Mode::Pretty
    };
    let parity = args.iter().any(|a| a == "--parity");
    let mut out: Option<PathBuf> = None;
    if let Some(i) = args.iter().position(|a| a == "--out") {
        args.remove(i);
        if i >= args.len() {
            bail!("--out needs a file path (.md or .json)");
        }
        out = Some(PathBuf::from(args.remove(i)));
    }
    let name = args.iter().find(|a| !a.starts_with('-')).cloned();

    // Prefer an explicit data directory (dev / CARGO_PULSE_DATA); otherwise use
    // the artifacts compiled into the binary, so an installed crate is self-contained.
    let dir = artifact_dir();
    let art = match &dir {
        Some(d) => data::Artifacts::load(d)
            .map_err(|e| anyhow::anyhow!("failed to load artifacts from {} ({e})", d.display()))?,
        None => data::Artifacts::load_embedded().context("failed to load embedded artifacts")?,
    };
    if parity {
        let json = match &dir {
            Some(d) => std::fs::read_to_string(d.join("parity_sample.json"))?,
            None => data::EMBEDDED_PARITY.to_string(),
        };
        return data::parity_check(&json, &art);
    }
    if name.as_deref() == Some("audit") {
        let path = args
            .iter()
            .filter(|a| !a.starts_with('-'))
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let detail = args.iter().any(|a| a == "--detail");
        return audit::run(&path, &mode, &art, detail, out.as_deref());
    }
    let Some(name) = name else {
        bail!("usage: cargo-pulse <crate>|audit [path] [--detail] [--out <file>] [--markdown|--json]")
    };
    report::run(&name, &mode, &art)
}

fn ago(days: i64) -> String {
    match days {
        d if d < 0 => "in the future?".into(),
        d if d < 45 => format!("{d} days ago"),
        d if d < 730 => format!("{:.0} months ago", d as f64 / 30.44),
        d => format!("{:.1} years ago", d as f64 / 365.25),
    }
}
