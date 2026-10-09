//! Live registry/advisory clients: crates.io and OSV.dev (RustSec).
//! The forge clients (GitHub / GitLab / Codeberg) live in forge.rs and
//! share this module's HTTP agent and date parsing.

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

pub(crate) fn get(url: &str) -> ureq::Request {
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

pub(crate) fn date_of(v: &serde_json::Value) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|d| d.date_naive())
}
