//! Forge checks (GitHub / GitLab / Codeberg): the archived/deleted facts
//! and, on GitHub, the model's repo-tier activity signals. Serving
//! semantics pinned by the 12/08 manual pipeline test: days_since_commit
//! reads the DEFAULT BRANCH commits endpoint (not pushed_at), commit
//! counts use the Link-header trick, and non-GitHub forges get the
//! explicit archived check only. GitHub calls go through the on-disk
//! ETag cache (a 304 costs no quota). Without GITHUB_TOKEN only the
//! single /repos call is made per repo: facts stay fully checked, the
//! four fine-grained signal calls are skipped (`degraded`).

use crate::net::{date_of, get};
use chrono::NaiveDate;

pub enum RepoStatus {
    /// GitHub repo found: archived flag + the model's repo-tier signals.
    /// `degraded` = anonymous mode, fine-grained signal calls skipped.
    Github {
        archived: bool,
        degraded: bool,
        last_commit: Option<NaiveDate>,   // default branch
        commits_180d: Option<u64>,        // default branch
        last_issue_close: Option<NaiveDate>,
        last_pr_merge: Option<NaiveDate>,
    },
    /// Non-GitHub forge: explicit archived check only (training's repo
    /// tier is GitHub-only, so features serve as has_repo=false).
    OtherForge { forge: String, archived: bool },
    Gone,          // 404: deleted / renamed / private
    NoRepo,        // no repository URL in crate metadata
    Unknown(String),
}

pub fn check_repo(url: Option<&str>) -> RepoStatus {
    let Some(url) = url else { return RepoStatus::NoRepo };
    let path = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let parts: Vec<&str> = path.split('/').collect();
    match parts.first().copied() {
        Some("github.com") if parts.len() >= 3 => github(parts[1], parts[2]),
        Some("gitlab.com") if parts.len() >= 3 => {
            let proj = parts[1..].join("%2F");
            simple_archived(
                &format!("https://gitlab.com/api/v4/projects/{proj}"),
                "gitlab.com",
            )
        }
        Some("codeberg.org") if parts.len() >= 3 => simple_archived(
            &format!("https://codeberg.org/api/v1/repos/{}/{}", parts[1], parts[2]),
            "codeberg.org",
        ),
        Some(host) => RepoStatus::Unknown(host.to_string()),
        None => RepoStatus::NoRepo,
    }
}

/// Anonymous-mode quota probe; GET /rate_limit itself costs nothing.
/// None when a token is set (budget is then a non-issue) or on any failure.
pub fn anon_rate_budget() -> Option<(u64, u64)> {
    if std::env::var("GITHUB_TOKEN").is_ok() {
        return None;
    }
    let v: serde_json::Value = get("https://api.github.com/rate_limit")
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let core = &v["resources"]["core"];
    Some((core["remaining"].as_u64()?, core["reset"].as_u64()?))
}

fn simple_archived(api_url: &str, forge: &str) -> RepoStatus {
    match get(api_url).call() {
        Ok(r) => {
            let v: serde_json::Value = match r.into_json() {
                Ok(v) => v,
                Err(_) => return RepoStatus::Unknown(forge.to_string()),
            };
            RepoStatus::OtherForge {
                forge: forge.to_string(),
                archived: v["archived"].as_bool().unwrap_or(false),
            }
        }
        Err(ureq::Error::Status(404, _)) => RepoStatus::Gone,
        Err(_) => RepoStatus::Unknown(forge.to_string()),
    }
}

fn gh(url: &str) -> ureq::Request {
    let mut req = get(url).set("Accept", "application/vnd.github+json");
    if let Ok(tok) = std::env::var("GITHUB_TOKEN") {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    req
}

enum GhResp {
    Ok(serde_json::Value, Option<String>), // (body, link header)
    NotFound,
    Fail,
}

/// GitHub GET through the ETag cache: send If-None-Match when a validator
/// is stored; a 304 reuses the cached body (quota-free), a 200 refreshes it.
fn gh_get(url: &str) -> GhResp {
    let cached = crate::cache::load(url);
    let mut req = gh(url);
    if let Some(etag) = cached.as_ref().and_then(|c| c.etag.as_deref()) {
        req = req.set("If-None-Match", etag);
    }
    match req.call() {
        // ureq only errors on 4xx/5xx, so a 304 revalidation arrives as Ok
        Ok(r) if r.status() == 304 => match cached {
            Some(c) => GhResp::Ok(c.body, c.link),
            None => GhResp::Fail,
        },
        Ok(r) => {
            let etag = r.header("etag").map(str::to_string);
            let link = r.header("link").map(str::to_string);
            match r.into_json::<serde_json::Value>() {
                Ok(body) => {
                    crate::cache::store(url, etag.as_deref(), link.as_deref(), &body);
                    GhResp::Ok(body, link)
                }
                Err(_) => GhResp::Fail,
            }
        }
        Err(ureq::Error::Status(404, _)) => GhResp::NotFound,
        Err(_) => GhResp::Fail,
    }
}

fn github(owner: &str, repo: &str) -> RepoStatus {
    let base = format!("https://api.github.com/repos/{owner}/{repo}");
    let v = match gh_get(&base) {
        GhResp::Ok(v, _) => v,
        GhResp::NotFound => return RepoStatus::Gone,
        GhResp::Fail => return RepoStatus::Unknown("github.com".into()),
    };
    if v["archived"].as_bool().unwrap_or(false) {
        return RepoStatus::Github {
            archived: true,
            degraded: false,
            last_commit: None,
            commits_180d: None,
            last_issue_close: None,
            last_pr_merge: None,
        };
    }
    if std::env::var("GITHUB_TOKEN").is_err() {
        // anonymous: the facts are checked, the 4 signal calls are skipped
        return RepoStatus::Github {
            archived: false,
            degraded: true,
            last_commit: None,
            commits_180d: None,
            last_issue_close: None,
            last_pr_merge: None,
        };
    }

    // latest DEFAULT-BRANCH commit (not pushed_at - see module docs)
    let last_commit = match gh_get(&format!("{base}/commits?per_page=1")) {
        GhResp::Ok(c, _) => date_of(&c[0]["commit"]["committer"]["date"]),
        _ => None,
    };

    // commit count in the last 180 days via the Link-header trick; the
    // window start is snapped to UTC midnight so the URL (= cache key)
    // stays stable within a day
    let since = (chrono::Utc::now() - chrono::Duration::days(180))
        .format("%Y-%m-%dT00:00:00Z")
        .to_string();
    let commits_180d = match gh_get(&format!("{base}/commits?since={since}&per_page=1")) {
        GhResp::Ok(body, link) => Some(link_count(link.as_deref(), &body)),
        _ => None,
    };

    // most recently updated closed issue that is NOT a pull request
    let last_issue_close = match gh_get(&format!(
        "{base}/issues?state=closed&sort=updated&direction=desc&per_page=20"
    )) {
        GhResp::Ok(v, _) => v.as_array().and_then(|a| {
            a.iter()
                .find(|i| i.get("pull_request").is_none())
                .and_then(|i| date_of(&i["closed_at"]))
        }),
        _ => None,
    };

    // most recently updated closed PR that was actually merged
    let last_pr_merge = match gh_get(&format!(
        "{base}/pulls?state=closed&sort=updated&direction=desc&per_page=20"
    )) {
        GhResp::Ok(v, _) => v.as_array().and_then(|a| {
            a.iter()
                .find(|p| !p["merged_at"].is_null())
                .and_then(|p| date_of(&p["merged_at"]))
        }),
        _ => None,
    };

    RepoStatus::Github {
        archived: false,
        degraded: false,
        last_commit,
        commits_180d,
        last_issue_close,
        last_pr_merge,
    }
}

fn link_count(link: Option<&str>, body: &serde_json::Value) -> u64 {
    if let Some(n) = link
        .unwrap_or("")
        .split(',')
        .find(|s| s.contains("rel=\"last\""))
        .and_then(|s| s.split("page=").last())
        .and_then(|s| s.split('>').next())
        .and_then(|s| s.parse::<u64>().ok())
    {
        n
    } else {
        body.as_array().map(|a| a.len() as u64).unwrap_or(0)
    }
}
