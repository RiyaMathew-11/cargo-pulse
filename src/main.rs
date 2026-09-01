//! cargo-pulse: abandonment risk for crates.io packages.
//!
//! Pipeline (facts always preempt forecasts):
//!   1. crates.io  - exists? current stats
//!   2. OSV/RustSec - declared unmaintained? -> report the fact, stop
//!   3. forge      - archived / deleted?     -> report the fact, stop
//!   4. age >= 18 months? else "too young to assess"
//!   5. model: live features -> bagged Cox score -> registry percentile
//!      -> gold-calibrated P(abandoned within ~2y) with (score,age) band
//!
//! The report is authored ONCE as markdown, then routed:
//!   default      rendered in-terminal (termimad: headings, bullets)
//!   --markdown   raw markdown (glow-compatible, paste-ready)
//!   --json       machine-readable (CI gates)
//!
//! Usage:  cargo-pulse <crate> [--markdown|--json]   (also: cargo pulse)
//!         cargo-pulse --parity                      (train/serve harness)
//!         env: GITHUB_TOKEN (optional), CARGO_PULSE_DATA (artifact dir)

mod data;
mod features;
mod net;

use anyhow::{Context, Result, bail};
use chrono::Datelike;
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

/// A cargo-style stage line: spins on stderr while working, persists
/// as a checkmark summary. Invisible when stderr is not a terminal, so
/// pipes and --json consumers never see it.
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
    let Some(name) = name else { bail!("usage: cargo-pulse <crate> [--markdown|--json]") };

    let today = chrono::Utc::now().date_naive();
    let mut j = serde_json::json!({ "crate": name, "snapshot": art.meta.snapshot });

    // 1 ─ crates.io
    let pb = stage("crates.io");
    let fetched = net::fetch_crate(&name)?;
    let Some(info) = fetched else {
        done(pb, "not found".into());
        j["status"] = "not_found".into();
        emit(&mode, &format!("# cargo-pulse · {name}\n\n**not found on crates.io**\n"), &j, Sev::Plain);
        return Ok(());
    };
    let age_days = (today - info.created_at).num_days();
    let last_rel = info.versions.last().expect("crates always have >=1 version").0;
    done(pb, format!("{} versions · {} dependents", info.versions.len(), info.reverse_deps));
    let stats = format!(
        "* **{v}** versions · last release **{lr}** ({lrd}) · **{d}** dependents · {o} owner{s} · age {a:.1}y",
        v = info.versions.len(),
        lr = ago((today - last_rel).num_days()),
        lrd = last_rel,
        d = info.reverse_deps,
        o = info.n_owners,
        s = if info.n_owners == 1 { "" } else { "s" },
        a = age_days as f64 / 365.25
    );
    j["stats"] = serde_json::json!({
        "versions": info.versions.len(), "last_release": last_rel.to_string(),
        "dependents": info.reverse_deps, "owners": info.n_owners, "age_days": age_days,
    });

    let mut md = format!("# cargo-pulse · {name}\n\n");

    // 2 ─ RustSec via OSV
    let pb = stage("RustSec/OSV");
    let advisories = net::fetch_advisories(&name)?;
    done(
        pb,
        match advisories.len() {
            0 => "no advisories".into(),
            n => format!("{n} advisor{}", if n == 1 { "y" } else { "ies" }),
        },
    );
    if let Some(a) = advisories.iter().find(|a| a.unmaintained) {
        md += &format!(
            "**✗ DECLARED UNMAINTAINED** — {} (*{}*)\n\n{stats}\n\n---\n*status is a recorded fact; no forecast needed.*\n",
            a.id, a.summary
        );
        j["status"] = "declared_unmaintained".into();
        j["advisory"] = a.id.clone().into();
        emit(&mode, &md, &j, Sev::Red);
        return Ok(());
    }
    let notes: Vec<String> = advisories
        .iter()
        .map(|a| format!("* advisory {} (*{}*)", a.id, a.summary))
        .collect();

    // 3 ─ forge
    let pb = stage("repository");
    let repo = net::check_repo(info.repository.as_deref());
    done(
        pb,
        match &repo {
            net::RepoStatus::Github { archived: true, .. }
            | net::RepoStatus::OtherForge { archived: true, .. } => "ARCHIVED".into(),
            net::RepoStatus::Github { .. } => "github: live".into(),
            net::RepoStatus::OtherForge { forge, .. } => format!("{forge}: live"),
            net::RepoStatus::Gone => "NOT FOUND".into(),
            net::RepoStatus::NoRepo => "none listed".into(),
            net::RepoStatus::Unknown(h) => format!("{h}: unsupported"),
        },
    );
    let repo_line: String;
    match &repo {
        net::RepoStatus::Github { archived: true, .. }
        | net::RepoStatus::OtherForge { archived: true, .. } => {
            md += &format!(
                "**✗ REPOSITORY ARCHIVED** — {}\n\n{stats}\n\n---\n*status is a recorded fact; no forecast needed.*\n",
                info.repository.as_deref().unwrap_or("")
            );
            j["status"] = "archived".into();
            emit(&mode, &md, &j, Sev::Red);
            return Ok(());
        }
        net::RepoStatus::Gone => {
            repo_line =
                "* ⚠ repository listed but **not found** (deleted/renamed/private) — abandonment evidence".into();
            j["repo"] = "gone".into();
        }
        net::RepoStatus::Github { .. } => {
            repo_line = "* repository live, not archived (github)".into();
        }
        net::RepoStatus::OtherForge { forge, .. } => {
            repo_line = format!("* repository not archived ({forge}); activity signals unavailable off-GitHub");
        }
        net::RepoStatus::NoRepo => repo_line = "* no repository listed".into(),
        net::RepoStatus::Unknown(host) => {
            repo_line = format!("* repository host {host} not supported; judged from registry only")
        }
    }

    // 4 ─ age gate
    if age_days < (art.meta.eligibility_months as i64) * 30 {
        md += &format!(
            "**? TOO YOUNG TO ASSESS** — {:.0} months old; the model needs {}+ months of history\n\n{stats}\n",
            age_days as f64 / 30.44,
            art.meta.eligibility_months
        );
        j["status"] = "too_young".into();
        emit(&mode, &md, &j, Sev::Yellow);
        return Ok(());
    }

    // 5 ─ the forecast
    let pb = stage("scoring");
    let owner_date = art.owners.get(&name).copied();
    let rep = features::assemble(&art.meta.features, &info, &repo, owner_date, today);
    let score = art.score(&rep.x);
    let pct = art.percentile(score);
    let age_years = age_days as f64 / 365.25;
    let (p, lo, hi) = art.probability(score, age_years);
    done(pb, format!("{} bags · percentile {:.0}", art.bags.len(), pct));
    eprintln!();

    let (chip, sev) = if hi - lo > 0.60 {
        (format!("**? UNCERTAIN** — the model cannot pin this crate down: roughly **{:.0}–{:.0}%** chance of abandonment within {} *(point {:.0}%)*",
            lo * 100.0, hi * 100.0, art.meta.horizon, p * 100.0), Sev::Yellow)
    } else {
        let (sym, word, sv) = match p {
            x if x >= 0.7 => ("⚠", "HIGH RISK", Sev::Red),
            x if x >= 0.3 => ("⚠", "ELEVATED RISK", Sev::Yellow),
            _ => ("✓", "LOW RISK", Sev::Green),
        };
        (format!("**{sym} {word}** — roughly **{:.0}–{:.0}%** chance of abandonment within {} *(point {:.0}%)*",
            lo * 100.0, hi * 100.0, art.meta.horizon, p * 100.0), sv)
    };
    md += &format!("{chip}\n\n");

    md += "## releases\n\n";
    md += &format!("{stats}\n");
    md += &format!("* rhythm by year: `{}`\n", sparkline(&info.versions, today));
    if let Some(c) = rep.cadence_decay {
        if c > 1.5 && rep.months_since_release >= 6.0 {
            md += &format!("* release cadence **{c:.1}x slower** than this crate's own norm\n");
        }
    }

    md += "\n## activity\n\n";
    if let Some((days, via)) = rep.heartbeat {
        md += &format!("* last sign of maintainer life: **{}** (via {via})\n", ago(days));
        j["heartbeat_days"] = days.into();
    }
    if let net::RepoStatus::Github { commits_180d: Some(c), .. } = &repo {
        md += &format!("* {c} commits on the default branch in 180 days\n");
    }
    md += &format!("{repo_line}\n");
    for n in &notes {
        md += &format!("{n}\n");
    }

    md += "\n## standing\n\n";
    md += &format!(
        "* riskier than **{pct:.0}%** of the eligible registry ({} dependents)\n",
        info.reverse_deps
    );
    md += &format!("\n`{}`\n", gauge(lo, hi, p));

    md += &format!(
        "\n---\n*basis: model {} @ {} snapshot · probability calibrated against a human+LLM-judged \
         gold standard (n=135, horizon {}) · band = 95% bootstrap interval · assumes recent \
         dynamics persist · facts always outrank forecasts.*\n",
        art.meta.model_tag, art.meta.snapshot, art.meta.horizon
    );

    j["status"] = "modeled".into();
    j["probability"] = serde_json::json!({ "point": p, "lo": lo, "hi": hi });
    j["percentile"] = pct.into();
    j["score"] = score.into();
    emit(&mode, &md, &j, sev);
    Ok(())
}

fn sparkline(versions: &[(chrono::NaiveDate, String)], today: chrono::NaiveDate) -> String {
    let y0 = versions.first().map(|v| v.0.year()).unwrap_or(today.year());
    let y1 = today.year();
    let mut counts = vec![0usize; (y1 - y0 + 1).max(1) as usize];
    for (d, _) in versions {
        counts[(d.year() - y0) as usize] += 1;
    }
    let max = *counts.iter().max().unwrap_or(&1) as f64;
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let line: String = counts
        .iter()
        .map(|&c| {
            if c == 0 {
                '·'
            } else {
                bars[(((c as f64 / max) * 7.0).round() as usize).min(7)]
            }
        })
        .collect();
    format!("{line}  {y0}→{y1}")
}

fn gauge(lo: f64, hi: f64, p: f64) -> String {
    const W: usize = 40;
    let pos = |v: f64| ((v * (W - 1) as f64).round() as usize).min(W - 1);
    let (il, ih, ip) = (pos(lo), pos(hi), pos(p));
    let mut cells: Vec<char> = (0..W)
        .map(|i| if i >= il && i <= ih { '█' } else { '─' })
        .collect();
    cells[ip] = '◆';
    format!("0% {} 100%", cells.into_iter().collect::<String>())
}

fn ago(days: i64) -> String {
    match days {
        d if d < 0 => "in the future?".into(),
        d if d < 45 => format!("{d} days ago"),
        d if d < 730 => format!("{:.0} months ago", d as f64 / 30.44),
        d => format!("{:.1} years ago", d as f64 / 365.25),
    }
}
