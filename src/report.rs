//! The single-crate report: pipeline steps 1–5 for one named crate

use crate::data::Artifacts;
use crate::{Mode, Sev, ago, done, emit, stage};
use crate::{features, forge, net};
use anyhow::Result;
use chrono::Datelike;

pub fn run(name: &str, mode: &Mode, art: &Artifacts) -> Result<()> {
    let today = chrono::Utc::now().date_naive();
    let mut j = serde_json::json!({ "crate": name, "snapshot": art.meta.snapshot });

    // 1 ─ crates.io
    let pb = stage("crates.io");
    let fetched = net::fetch_crate(name)?;
    let Some(info) = fetched else {
        done(pb, "not found".into());
        j["status"] = "not_found".into();
        emit(mode, &format!("# cargo-pulse · {name}\n\n**not found on crates.io**\n"), &j, Sev::Plain);
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
    let advisories = net::fetch_advisories(name)?;
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
        emit(mode, &md, &j, Sev::Red);
        return Ok(());
    }
    let notes: Vec<String> = advisories
        .iter()
        .map(|a| format!("* advisory {} (*{}*)", a.id, a.summary))
        .collect();

    // 3 ─ forge
    let pb = stage("repository");
    let repo = forge::check_repo(info.repository.as_deref())?;
    done(
        pb,
        match &repo {
            forge::RepoStatus::Github { archived: true, .. }
            | forge::RepoStatus::OtherForge { archived: true, .. } => "ARCHIVED".into(),
            forge::RepoStatus::Github { .. } => "github: live".into(),
            forge::RepoStatus::OtherForge { forge, .. } => format!("{forge}: live"),
            forge::RepoStatus::Gone => "NOT FOUND".into(),
            forge::RepoStatus::NoRepo => "none listed".into(),
            forge::RepoStatus::Unknown(h) => format!("{h}: unsupported"),
        },
    );
    let repo_line: String;
    match &repo {
        forge::RepoStatus::Github { archived: true, .. }
        | forge::RepoStatus::OtherForge { archived: true, .. } => {
            md += &format!(
                "**✗ REPOSITORY ARCHIVED** — {}\n\n{stats}\n\n---\n*status is a recorded fact; no forecast needed.*\n",
                info.repository.as_deref().unwrap_or("")
            );
            j["status"] = "archived".into();
            emit(mode, &md, &j, Sev::Red);
            return Ok(());
        }
        forge::RepoStatus::Gone => {
            repo_line =
                "* ⚠ repository listed but **not found** (deleted/renamed/private) — abandonment evidence".into();
            j["repo"] = "gone".into();
        }
        forge::RepoStatus::Github { .. } => {
            repo_line = "* repository live, not archived (github)".into();
        }
        forge::RepoStatus::OtherForge { forge, .. } => {
            repo_line = format!("* repository not archived ({forge}); activity signals unavailable off-GitHub");
        }
        forge::RepoStatus::NoRepo => repo_line = "* no repository listed".into(),
        forge::RepoStatus::Unknown(host) => {
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
        emit(mode, &md, &j, Sev::Yellow);
        return Ok(());
    }

    // 5 ─ the forecast
    let pb = stage("scoring");
    let owner_date = art.owners.get(name).copied();
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
    if let forge::RepoStatus::Github { commits_180d: Some(c), .. } = &repo {
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
    emit(mode, &md, &j, sev);
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
