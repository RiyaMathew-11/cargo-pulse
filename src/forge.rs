//! Forge checks (GitHub / GitLab / Codeberg): the archived/deleted facts
//! and, on GitHub, the model's repo-tier activity signals.

use crate::net::{date_of, get};
use chrono::NaiveDate;

pub enum RepoStatus {
    /// GitHub repo found: archived flag + the model's repo-tier signals.
    Github {
        archived: bool,
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

/// Err only when GitHub rate-limits us; every other failure is a status.
pub fn check_repo(url: Option<&str>) -> anyhow::Result<RepoStatus> {
    let Some(url) = url else { return Ok(RepoStatus::NoRepo) };
    let path = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let parts: Vec<&str> = path.split('/').collect();
    Ok(match parts.first().copied() {
        Some("github.com") if parts.len() >= 3 => github(parts[1], parts[2])?,
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
    })
}

fn token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
}

/// The error a rate-limited GitHub call stops the run with.
fn rate_limited(reset: Option<u64>) -> anyhow::Error {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let retry = reset
        .map(|r| format!("retry in ~{} min", r.saturating_sub(now) / 60 + 1))
        .unwrap_or_else(|| "retry later".into());
    match token() {
        None => anyhow::anyhow!(
            "GitHub rate limit reached (60 requests/h without a token); {retry}.\n\
             To keep going now, set a token (no scopes needed):\n  \
             export GITHUB_TOKEN=<token>"
        ),
        Some(_) => anyhow::anyhow!("GitHub rate limit reached for GITHUB_TOKEN; {retry}."),
    }
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
    if let Some(tok) = token() {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    req
}

enum GhResp {
    Ok(serde_json::Value, Option<String>), // (body, link header)
    NotFound,
    Fail,
    RateLimited(Option<u64>), // reset time if GitHub sent one
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
        // primary limit: 403 + remaining=0; secondary limit: 403/429 + retry-after
        Err(ureq::Error::Status(code @ (403 | 429), r))
            if code == 429
                || r.header("x-ratelimit-remaining") == Some("0")
                || r.header("retry-after").is_some() =>
        {
            GhResp::RateLimited(r.header("x-ratelimit-reset").and_then(|s| s.parse().ok()))
        }
        Err(_) => GhResp::Fail,
    }
}

/// A signal call: the body on success, None on an ordinary failure,
/// Err when rate-limited (the run must stop, not serve a missing signal).
fn signal(url: &str) -> anyhow::Result<Option<(serde_json::Value, Option<String>)>> {
    match gh_get(url) {
        GhResp::Ok(v, link) => Ok(Some((v, link))),
        GhResp::RateLimited(reset) => Err(rate_limited(reset)),
        GhResp::NotFound | GhResp::Fail => Ok(None),
    }
}

fn github(owner: &str, repo: &str) -> anyhow::Result<RepoStatus> {
    let base = format!("https://api.github.com/repos/{owner}/{repo}");
    let v = match gh_get(&base) {
        GhResp::Ok(v, _) => v,
        GhResp::NotFound => return Ok(RepoStatus::Gone),
        GhResp::Fail => return Ok(RepoStatus::Unknown("github.com".into())),
        GhResp::RateLimited(reset) => return Err(rate_limited(reset)),
    };
    if v["archived"].as_bool().unwrap_or(false) {
        return Ok(RepoStatus::Github {
            archived: true,
            last_commit: None,
            commits_180d: None,
            last_issue_close: None,
            last_pr_merge: None,
        });
    }

    // latest DEFAULT-BRANCH commit (not pushed_at - see module docs)
    let last_commit = signal(&format!("{base}/commits?per_page=1"))?
        .and_then(|(c, _)| date_of(&c[0]["commit"]["committer"]["date"]));

    // commit count in the last 180 days via the Link-header trick; the
    // window start is snapped to UTC midnight so the URL (= cache key)
    // stays stable within a day
    let since = (chrono::Utc::now() - chrono::Duration::days(180))
        .format("%Y-%m-%dT00:00:00Z")
        .to_string();
    let commits_180d = signal(&format!("{base}/commits?since={since}&per_page=1"))?
        .map(|(body, link)| link_count(link.as_deref(), &body));

    // most recently updated closed issue that is NOT a pull request
    let last_issue_close = signal(&format!(
        "{base}/issues?state=closed&sort=updated&direction=desc&per_page=20"
    ))?
    .and_then(|(v, _)| {
        v.as_array().and_then(|a| {
            a.iter()
                .find(|i| i.get("pull_request").is_none())
                .and_then(|i| date_of(&i["closed_at"]))
        })
    });

    // most recently updated closed PR that was actually merged
    let last_pr_merge = signal(&format!(
        "{base}/pulls?state=closed&sort=updated&direction=desc&per_page=20"
    ))?
    .and_then(|(v, _)| {
        v.as_array().and_then(|a| {
            a.iter()
                .find(|p| !p["merged_at"].is_null())
                .and_then(|p| date_of(&p["merged_at"]))
        })
    });

    Ok(RepoStatus::Github {
        archived: false,
        last_commit,
        commits_180d,
        last_issue_close,
        last_pr_merge,
    })
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
