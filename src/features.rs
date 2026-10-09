//! Assemble the model's feature vector from live data, replicating the
//! training formulas EXACTLY (from features_at.py / cox_model.py):
//!   months_since_release = calendar-month boundary count
//!   cadence gaps over ALL versions (incl yanked), median interpolated
//!   is_pre_1_0 = max leading major over all versions == 0
//!   log features = ln(1+x); missing = NaN (trees route via default)

use crate::forge::RepoStatus;
use crate::net::CrateInfo;
use chrono::{Datelike, NaiveDate};

pub struct FeatureReport {
    pub x: Vec<f64>,                 // in meta.features order
    pub months_since_release: f64,
    pub cadence_decay: Option<f64>,
    pub heartbeat: Option<(i64, &'static str)>, // (days ago, channel)
}

fn months_between(a: NaiveDate, b: NaiveDate) -> f64 {
    ((b.year() * 12 + b.month() as i32) - (a.year() * 12 + a.month() as i32)) as f64
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 }
}

pub fn assemble(
    order: &[String],
    info: &CrateInfo,
    repo: &RepoStatus,
    last_owner_added: Option<NaiveDate>,
    today: NaiveDate,
) -> FeatureReport {
    let n_versions = info.versions.len() as f64;
    let last_release = info.versions.last().expect(">=1 release checked upstream").0;
    let months_since_release = months_between(last_release, today);

    let mut majors: Vec<i64> = Vec::new();
    for (_, num) in &info.versions {
        if let Some(m) = num.split('.').next().and_then(|s| s.parse::<i64>().ok()) {
            majors.push(m);
        }
    }
    let is_pre_1_0 = if majors.iter().copied().max() == Some(0) { 1.0 } else { 0.0 };

    let gaps: Vec<f64> = info
        .versions
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).num_days() as f64)
        .collect();
    let cadence_decay = if !gaps.is_empty() {
        let mut sorted = gaps.clone();
        sorted.sort_by(f64::total_cmp);
        let med = median(&sorted);
        if med > 0.0 { Some(*gaps.last().unwrap() / med) } else { None }
    } else {
        None
    };

    let days_since_owner_change = last_owner_added
        .map(|d| (today - d).num_days() as f64)
        .unwrap_or(f64::NAN);

    // repo tier: GitHub-only, matching training (non-GitHub forges and
    // gone/no-repo crates all serve as has_repo=false with NaN tier)
    let (has_repo, commits_180d, d_commit, d_issue, d_pr, i_cens, p_cens) = match repo {
        RepoStatus::Github { archived: false, last_commit, commits_180d,
                             last_issue_close, last_pr_merge, .. } => {
            let dc = last_commit.map(|d| (today - d).num_days() as f64).unwrap_or(f64::NAN);
            let di = last_issue_close.map(|d| (today - d).num_days() as f64);
            let dp = last_pr_merge.map(|d| (today - d).num_days() as f64);
            (
                1.0,
                commits_180d.map(|c| c as f64).unwrap_or(f64::NAN),
                dc,
                di.unwrap_or(f64::NAN),
                dp.unwrap_or(f64::NAN),
                if di.is_some() { 0.0 } else { 1.0 },
                if dp.is_some() { 0.0 } else { 1.0 },
            )
        }
        _ => (0.0, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN),
    };

    let value = |name: &str| -> f64 {
        match name {
            "log_n_versions" => (n_versions + 1.0).ln(),
            "log_reverse_deps" => (info.reverse_deps as f64 + 1.0).ln(),
            "log_commits_180d_at_T" => {
                if commits_180d.is_nan() { f64::NAN } else { (commits_180d + 1.0).ln() }
            }
            "n_owners" => info.n_owners as f64,
            "release_cadence_decay" => cadence_decay.unwrap_or(f64::NAN),
            "days_since_owner_change" => days_since_owner_change,
            "months_since_release" => months_since_release,
            "days_since_commit_at_T" => d_commit,
            "days_since_issue_close_at_T" => d_issue,
            "days_since_pr_merge_at_T" => d_pr,
            "is_pre_1_0" => is_pre_1_0,
            "issue_censored" => i_cens,
            "pr_censored" => p_cens,
            "has_repo_at" => has_repo,
            other => panic!("unknown feature in meta.json: {other}"),
        }
    };
    let x: Vec<f64> = order.iter().map(|f| value(f)).collect();

    // last sign of maintainer life across all available channels
    let mut beats: Vec<(i64, &'static str)> =
        vec![((today - last_release).num_days(), "release")];
    if let RepoStatus::Github { last_commit, last_issue_close, last_pr_merge, .. } = repo {
        if let Some(d) = last_commit {
            beats.push(((today - *d).num_days(), "commit"));
        }
        if let Some(d) = last_issue_close {
            beats.push(((today - *d).num_days(), "issue closed"));
        }
        if let Some(d) = last_pr_merge {
            beats.push(((today - *d).num_days(), "PR merged"));
        }
    }
    let heartbeat = beats.into_iter().min_by_key(|(d, _)| *d);

    FeatureReport { x, months_since_release, cadence_decay, heartbeat }
}
