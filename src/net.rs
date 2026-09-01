//! Live data clients: crates.io, OSV.dev (RustSec advisories), and the
//! forge APIs (GitHub / GitLab / Codeberg). Serving semantics pinned by
//! the 12/08 manual pipeline test: days_since_commit reads the DEFAULT
//! BRANCH commits endpoint (not pushed_at - branch pushes are not
//! maintainer commits to main), commit counts use the Link-header
//! trick, and non-GitHub forges get the explicit archived check only
//! (the model's repo tier was trained on GitHub data alone, so
//! non-GitHub crates serve as has_repo=false, matching training).

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use std::sync::OnceLock;
use std::time::Duration;

const UA: &str = "cargo-pulse/0.1 (dissertation research tool)";

fn agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .build()
    })
}

fn get(url: &str) -> ureq::Request {
    agent().get(url).set("User-Agent", UA)
}

/// GET with polite pacing and retries on transient transport errors -
/// including resets MID-BODY (large responses like serde_yaml's version
/// list can drop while streaming). None = 404.
fn get_json(url: &str) -> Result<Option<serde_json::Value>> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(900));
        }
        match get(url).call() {
            Ok(r) => match r.into_string() {
                Ok(body) => match serde_json::from_str(&body) {
                    Ok(v) => return Ok(Some(v)),
                    Err(e) => last = Some(anyhow!("{url}: bad JSON: {e}")),
                },
                Err(e) => last = Some(anyhow!("{url}: body read: {e}")),
            },
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(ureq::Error::Status(code, _)) => {
                return Err(anyhow!("{url}: HTTP {code}"));
            }
            Err(e) => last = Some(anyhow!("{url}: {e}")),
        }
    }
    Err(last.unwrap())
}

// ---------------------------------------------------------------- crates.io

pub struct CrateInfo {
    pub created_at: NaiveDate,
    pub repository: Option<String>,
    pub versions: Vec<(NaiveDate, String)>, // (created_at, num) - ALL, incl yanked
    pub reverse_deps: u64,
    pub n_owners: u64,
}

pub fn fetch_crate(name: &str) -> Result<Option<CrateInfo>> {
    let Some(v) = get_json(&format!("https://crates.io/api/v1/crates/{name}"))? else {
        return Ok(None);
    };
    let c = &v["crate"];
    let created_at = date_of(&c["created_at"]).context("crate.created_at")?;
    let repository = c["repository"].as_str().map(String::from);
    let mut versions = Vec::new();
    for ver in v["versions"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if let (Some(d), Some(num)) = (date_of(&ver["created_at"]), ver["num"].as_str()) {
            versions.push((d, num.to_string()));
        }
    }
    versions.sort();

    std::thread::sleep(Duration::from_millis(400)); // crates.io politeness
    let reverse_deps = get_json(&format!(
        "https://crates.io/api/v1/crates/{name}/reverse_dependencies?per_page=1"
    ))?
    .map(|rd| rd["meta"]["total"].as_u64().unwrap_or(0))
    .unwrap_or(0);

    std::thread::sleep(Duration::from_millis(400));
    let n_owners = get_json(&format!("https://crates.io/api/v1/crates/{name}/owners"))?
        .map(|ow| ow["users"].as_array().map(|a| a.len() as u64).unwrap_or(0))
        .unwrap_or(0);

    Ok(Some(CrateInfo { created_at, repository, versions, reverse_deps, n_owners }))
}

// ------------------------------------------------------------------ OSV.dev

pub struct Advisory {
    pub id: String,
    pub summary: String,
    pub unmaintained: bool,
}

pub fn fetch_advisories(name: &str) -> Result<Vec<Advisory>> {
    let body = serde_json::json!({
        "package": {"name": name, "ecosystem": "crates.io"}
    });
    let mut v = serde_json::Value::Null;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(900));
        }
        match agent()
            .post("https://api.osv.dev/v1/query")
            .set("User-Agent", UA)
            .send_json(body.clone())
        {
            Ok(r) => match r.into_string() {
                Ok(s) => match serde_json::from_str(&s) {
                    Ok(parsed) => {
                        v = parsed;
                        last = None;
                        break;
                    }
                    Err(e) => last = Some(anyhow!("osv: bad JSON: {e}")),
                },
                Err(e) => last = Some(anyhow!("osv: body read: {e}")),
            },
            Err(e) => last = Some(anyhow!("osv: {e}")),
        }
    }
    if let Some(e) = last {
        return Err(e);
    }
    let mut out = Vec::new();
    for vuln in v["vulns"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let id = vuln["id"].as_str().unwrap_or("").to_string();
        let summary = vuln["summary"].as_str().unwrap_or("").to_string();
        let details = vuln["details"].as_str().unwrap_or("");
        let text = format!("{summary} {details}").to_lowercase();
        let unmaintained = id.starts_with("RUSTSEC") && text.contains("unmaintained");
        out.push(Advisory { id, summary, unmaintained });
    }
    Ok(out)
}

// -------------------------------------------------------------------- forge

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

fn github(owner: &str, repo: &str) -> RepoStatus {
    let base = format!("https://api.github.com/repos/{owner}/{repo}");
    let v: serde_json::Value = match gh(&base).call() {
        Ok(r) => match r.into_json() {
            Ok(v) => v,
            Err(_) => return RepoStatus::Unknown("github.com".into()),
        },
        Err(ureq::Error::Status(404, _)) => return RepoStatus::Gone,
        Err(_) => return RepoStatus::Unknown("github.com".into()),
    };
    if v["archived"].as_bool().unwrap_or(false) {
        return RepoStatus::Github {
            archived: true,
            last_commit: None,
            commits_180d: None,
            last_issue_close: None,
            last_pr_merge: None,
        };
    }

    // latest DEFAULT-BRANCH commit (not pushed_at - see module docs)
    let last_commit = gh(&format!("{base}/commits?per_page=1"))
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .and_then(|c| date_of(&c[0]["commit"]["committer"]["date"]));

    // commit count in the last 180 days via the Link-header trick
    let since = (chrono::Utc::now() - chrono::Duration::days(180))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let commits_180d = gh(&format!("{base}/commits?since={since}&per_page=1"))
        .call()
        .ok()
        .map(|r| {
            let link = r.header("link").unwrap_or("").to_string();
            if let Some(n) = link
                .split(',')
                .find(|s| s.contains("rel=\"last\""))
                .and_then(|s| s.split("page=").last())
                .and_then(|s| s.split('>').next())
                .and_then(|s| s.parse::<u64>().ok())
            {
                n
            } else {
                r.into_json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.as_array().map(|a| a.len() as u64))
                    .unwrap_or(0)
            }
        });

    // most recently updated closed issue that is NOT a pull request
    let last_issue_close = gh(&format!(
        "{base}/issues?state=closed&sort=updated&direction=desc&per_page=20"
    ))
    .call()
    .ok()
    .and_then(|r| r.into_json::<serde_json::Value>().ok())
    .and_then(|v| {
        v.as_array()?.iter().find(|i| i.get("pull_request").is_none()).map(|i| {
            date_of(&i["closed_at"])
        })?
    });

    // most recently updated closed PR that was actually merged
    let last_pr_merge = gh(&format!(
        "{base}/pulls?state=closed&sort=updated&direction=desc&per_page=20"
    ))
    .call()
    .ok()
    .and_then(|r| r.into_json::<serde_json::Value>().ok())
    .and_then(|v| {
        v.as_array()?
            .iter()
            .find(|p| !p["merged_at"].is_null())
            .map(|p| date_of(&p["merged_at"]))?
    });

    RepoStatus::Github { archived: false, last_commit, commits_180d, last_issue_close, last_pr_merge }
}

fn date_of(v: &serde_json::Value) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|d| d.date_naive())
}
