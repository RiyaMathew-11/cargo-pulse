//! `cargo pulse audit`: abandonment risk for every direct dependency of a
//! project. Dependencies come from `cargo metadata --no-deps`; each crate
//! then runs the same pipeline as a single lookup (facts first, then the
//! model). Rendering lives in audit_report.rs.

use crate::data::Artifacts;
use crate::{Mode, done, emit, stage};
use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use std::path::Path;

pub struct Dep {
    pub(crate) name: String,
    pub(crate) req: String,
    pub(crate) kind: &'static str, // "", "build" or "dev" (strongest kind wins)
}

pub(crate) enum Outcome {
    Unmaintained { advisory: String, summary: String },
    Archived,
    Modeled { p: f64, lo: f64, hi: f64, pct: f64, uncertain: bool },
    TooYoung { months: f64 },
    NotFound,
    Error(String),
}

/// Registry/repo facts captured along the way, for --detail and --json.
/// Absent when the crate was not found or the lookup failed.
pub(crate) struct Stats {
    pub versions: usize,
    pub last_release: NaiveDate,
    pub dependents: u64,
    pub owners: u64,
    pub age_days: i64,
    pub advisory_notes: usize, // advisories that are not "unmaintained"
    pub repo: String,          // human label; "not checked" after a fact stop
    pub heartbeat: Option<(i64, &'static str)>,
}

pub(crate) struct Assessment {
    pub dep: Dep,
    pub stats: Option<Stats>,
    pub outcome: Outcome,
}

pub fn run(
    path: &Path,
    mode: &Mode,
    art: &Artifacts,
    detail: bool,
    out: Option<&Path>,
) -> Result<()> {
    let today = chrono::Utc::now().date_naive();

    let pb = stage("project");
    let (label, deps) = direct_deps(path)?;
    done(pb, format!("{label} · {} direct dependencies", deps.len()));
    if deps.is_empty() {
        bail!("no crates.io dependencies found in {}", path.display());
    }

    // anonymous mode: warn upfront if the GitHub quota cannot cover the run
    if let Some((remaining, reset)) = crate::forge::anon_rate_budget() {
        let n = deps.len() as u64; // ≤1 GitHub call per dep without a token
        let mins = reset.saturating_sub(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ) / 60;
        if n > remaining {
            eprintln!(
                "⚠ anonymous GitHub budget: {remaining} API call(s) left for up to {n} repo \
                 checks; quota resets in ~{mins} min. set GITHUB_TOKEN (5,000/h) — continuing, \
                 over-budget checks degrade gracefully."
            );
        } else if n > 40 {
            eprintln!(
                "note: large anonymous audit (up to {n} GitHub checks); set GITHUB_TOKEN to \
                 avoid rate limits."
            );
        }
    }

    let pb = stage("auditing");
    let total = deps.len();
    let mut rows: Vec<Assessment> = Vec::new();
    for (i, dep) in deps.into_iter().enumerate() {
        pb.set_message(format!("{} ({}/{total})", dep.name, i + 1));
        let (stats, outcome) = assess(&dep.name, art, today);
        rows.push(Assessment { dep, stats, outcome });
        std::thread::sleep(std::time::Duration::from_millis(300)); // crates.io politeness
    }
    rows.sort_by(|a, b| rank(a).partial_cmp(&rank(b)).unwrap());
    let counts = crate::audit_report::Counts::of(&rows);
    done(pb, format!("{total} dependencies · {}", counts.brief()));
    eprintln!();

    let (md, j, sev) = crate::audit_report::render(&label, &rows, &counts, art, detail, today);
    if let Some(path) = out {
        let content = match path.extension().and_then(|e| e.to_str()) {
            Some("md") => md.clone(),
            Some("json") => serde_json::to_string_pretty(&j).unwrap(),
            _ => bail!("--out needs a .md or .json file path"),
        };
        std::fs::write(path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        eprintln!("report written to {}", path.display());
    }
    emit(mode, &md, &j, sev);
    Ok(())
}

/// Direct, registry-resolvable dependencies of every workspace package,
/// deduped by crate name. Path-only deps (workspace internals) are skipped.
fn direct_deps(path: &Path) -> Result<(String, Vec<Dep>)> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(path)
        .output()
        .context("failed to run `cargo metadata` (is cargo on PATH?)")?;
    if !out.status.success() {
        bail!(
            "`cargo metadata` failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("cargo metadata: bad JSON")?;
    let packages = v["packages"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);

    let mut deps: Vec<Dep> = Vec::new();
    for pkg in packages {
        for d in pkg["dependencies"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if !d["path"].is_null() {
                continue; // local path dep, not on crates.io
            }
            let name = d["name"].as_str().unwrap_or("").to_string();
            let kind = match d["kind"].as_str() {
                Some("dev") => "dev",
                Some("build") => "build",
                _ => "",
            };
            match deps.iter_mut().find(|e| e.name == name) {
                Some(e) => {
                    // normal > build > dev: keep the strongest kind seen
                    if kind.len() < e.kind.len() {
                        e.kind = kind;
                    }
                }
                None => deps.push(Dep {
                    name,
                    req: d["req"].as_str().unwrap_or("*").to_string(),
                    kind,
                }),
            }
        }
    }
    let label = match packages {
        [only] => only["name"].as_str().unwrap_or("project").to_string(),
        many => format!("{} workspace packages", many.len()),
    };
    Ok((label, deps))
}

/// One dependency through the single-crate pipeline. Every failure is an
/// Error row; an audit never aborts over one crate.
fn assess(name: &str, art: &Artifacts, today: NaiveDate) -> (Option<Stats>, Outcome) {
    let info = match crate::net::fetch_crate(name) {
        Ok(Some(i)) => i,
        Ok(None) => return (None, Outcome::NotFound),
        Err(e) => return (None, Outcome::Error(e.to_string())),
    };
    let age_days = (today - info.created_at).num_days();
    let mut stats = Stats {
        versions: info.versions.len(),
        last_release: info.versions.last().expect("crates always have >=1 version").0,
        dependents: info.reverse_deps,
        owners: info.n_owners,
        age_days,
        advisory_notes: 0,
        repo: "not checked".into(),
        heartbeat: None,
    };

    let advisories = match crate::net::fetch_advisories(name) {
        Ok(a) => a,
        Err(e) => return (Some(stats), Outcome::Error(e.to_string())),
    };
    stats.advisory_notes = advisories.iter().filter(|a| !a.unmaintained).count();
    if let Some(a) = advisories.iter().find(|a| a.unmaintained) {
        let out = Outcome::Unmaintained { advisory: a.id.clone(), summary: a.summary.clone() };
        return (Some(stats), out);
    }

    let repo = crate::forge::check_repo(info.repository.as_deref());
    stats.repo = repo_label(&repo);
    if matches!(
        &repo,
        crate::forge::RepoStatus::Github { archived: true, .. }
            | crate::forge::RepoStatus::OtherForge { archived: true, .. }
    ) {
        return (Some(stats), Outcome::Archived);
    }
    if age_days < (art.meta.eligibility_months as i64) * 30 {
        return (Some(stats), Outcome::TooYoung { months: age_days as f64 / 30.44 });
    }

    let owner_date = art.owners.get(name).copied();
    let rep = crate::features::assemble(&art.meta.features, &info, &repo, owner_date, today);
    stats.heartbeat = rep.heartbeat;
    let score = art.score(&rep.x);
    let pct = art.percentile(score);
    let (p, lo, hi) = art.probability(score, age_days as f64 / 365.25);
    (Some(stats), Outcome::Modeled { p, lo, hi, pct, uncertain: hi - lo > 0.60 })
}

fn repo_label(repo: &crate::forge::RepoStatus) -> String {
    use crate::forge::RepoStatus::*;
    match repo {
        Github { archived: true, .. } | OtherForge { archived: true, .. } => "archived".into(),
        Github { degraded: true, .. } => "github: live (detail skipped — anonymous)".into(),
        Github { .. } => "github: live".into(),
        OtherForge { forge, .. } => format!("{forge}: live"),
        Gone => "listed but not found (deleted/renamed/private)".into(),
        NoRepo => "none listed".into(),
        // a supported forge landing here means the API call failed, not the host
        Unknown(h) if matches!(h.as_str(), "github.com" | "gitlab.com" | "codeberg.org") => {
            format!("{h}: check failed (rate-limited? set GITHUB_TOKEN)")
        }
        Unknown(h) => format!("{h}: unsupported"),
    }
}

/// Sort key: recorded facts first, then forecasts by probability,
/// then the unassessable tail.
fn rank(a: &Assessment) -> (u8, f64) {
    match &a.outcome {
        Outcome::Unmaintained { .. } => (0, 0.0),
        Outcome::Archived => (1, 0.0),
        Outcome::Modeled { p, .. } => (2, -p),
        Outcome::TooYoung { .. } => (3, 0.0),
        Outcome::NotFound => (4, 0.0),
        Outcome::Error(_) => (5, 0.0),
    }
}
