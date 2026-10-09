//! Rendering for `cargo pulse audit`: the ranked table, the optional
//! --detail blocks, and the JSON record. Authored once as markdown and
//! routed by main's emit(), like the single-crate report.

use crate::Sev;
use crate::ago;
use crate::audit::{Assessment, Outcome};
use crate::data::Artifacts;
use chrono::NaiveDate;

pub(crate) struct Counts {
    facts: usize,
    high: usize,
    elevated: usize,
    low: usize,
    uncertain: usize,
    too_young: usize,
    not_found: usize,
    errors: usize,
}

impl Counts {
    pub fn of(rows: &[Assessment]) -> Self {
        let mut c = Counts { facts: 0, high: 0, elevated: 0, low: 0, uncertain: 0,
                             too_young: 0, not_found: 0, errors: 0 };
        for r in rows {
            match &r.outcome {
                Outcome::Unmaintained { .. } | Outcome::Archived => c.facts += 1,
                Outcome::Modeled { uncertain: true, .. } => c.uncertain += 1,
                Outcome::Modeled { p, .. } if *p >= 0.7 => c.high += 1,
                Outcome::Modeled { p, .. } if *p >= 0.3 => c.elevated += 1,
                Outcome::Modeled { .. } => c.low += 1,
                Outcome::TooYoung { .. } => c.too_young += 1,
                Outcome::NotFound => c.not_found += 1,
                Outcome::Error(_) => c.errors += 1,
            }
        }
        c
    }

    pub fn brief(&self) -> String {
        let mut parts = Vec::new();
        for (n, word) in [
            (self.facts, "declared/archived"), (self.high, "high"),
            (self.elevated, "elevated"), (self.uncertain, "uncertain"),
            (self.low, "low"), (self.too_young, "too young"),
            (self.not_found, "not found"), (self.errors, "errors"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        parts.join(" · ")
    }
}

/// (symbol+word for the table, bare word for JSON)
fn verdict(o: &Outcome) -> (String, &'static str) {
    match o {
        Outcome::Unmaintained { advisory, .. } => {
            (format!("✗ unmaintained ({advisory})"), "declared_unmaintained")
        }
        Outcome::Archived => ("✗ repository archived".into(), "archived"),
        Outcome::Modeled { p, uncertain, .. } => match (*uncertain, *p) {
            (true, _) => ("? uncertain".into(), "uncertain"),
            (_, x) if x >= 0.7 => ("⚠ high risk".into(), "high"),
            (_, x) if x >= 0.3 => ("⚠ elevated".into(), "elevated"),
            _ => ("✓ low risk".into(), "low"),
        },
        Outcome::TooYoung { months } => (format!("? too young ({months:.0} mo)"), "too_young"),
        Outcome::NotFound => ("not on crates.io".into(), "not_found"),
        Outcome::Error(_) => ("lookup failed".into(), "error"),
    }
}

pub(crate) fn render(
    label: &str,
    rows: &[Assessment],
    counts: &Counts,
    art: &Artifacts,
    detail: bool,
    today: NaiveDate,
) -> (String, serde_json::Value, Sev) {
    let mut md = format!("# cargo-pulse · audit of {label}\n\n");
    md += "|**crate**|**required**|**kind**|**verdict**|**P(abandoned)**|\n|:-|:-|:-|:-|:-|\n";
    let mut jd = Vec::new();
    for r in rows {
        let (word, bare) = verdict(&r.outcome);
        let prob = match &r.outcome {
            Outcome::Modeled { p, lo, hi, .. } => {
                format!("{:.0}% ({:.0}–{:.0}%)", p * 100.0, lo * 100.0, hi * 100.0)
            }
            _ => "—".into(),
        };
        md += &format!("|{}|`{}`|{}|{}|{}|\n", r.dep.name, r.dep.req, r.dep.kind, word, prob);
        jd.push(json_entry(r, bare));
    }
    md += &format!("\n**summary** — {}\n", counts.brief());

    if detail {
        md += "\n## detail\n";
        for r in rows {
            md += &detail_block(r, today);
        }
    }

    md += &format!(
        "\n---\n*basis: model {} @ {} snapshot · direct dependencies only · \
         facts always outrank forecasts.*\n",
        art.meta.model_tag, art.meta.snapshot
    );

    let j = serde_json::json!({
        "audit": label, "snapshot": art.meta.snapshot, "deps": jd,
        "summary": {
            "declared_or_archived": counts.facts, "high": counts.high,
            "elevated": counts.elevated, "uncertain": counts.uncertain,
            "low": counts.low, "too_young": counts.too_young,
            "not_found": counts.not_found, "errors": counts.errors,
        },
    });
    let sev = if counts.facts + counts.high > 0 {
        Sev::Red
    } else if counts.elevated + counts.uncertain > 0 {
        Sev::Yellow
    } else if counts.low > 0 {
        Sev::Green
    } else {
        Sev::Plain
    };
    (md, j, sev)
}

/// The full per-package record: everything the single-crate report knows.
fn json_entry(r: &Assessment, verdict: &str) -> serde_json::Value {
    let mut e = serde_json::json!({
        "crate": r.dep.name, "req": r.dep.req,
        "kind": if r.dep.kind.is_empty() { "normal" } else { r.dep.kind },
        "verdict": verdict,
    });
    if let Some(s) = &r.stats {
        e["stats"] = serde_json::json!({
            "versions": s.versions, "last_release": s.last_release.to_string(),
            "dependents": s.dependents, "owners": s.owners, "age_days": s.age_days,
        });
        e["repo"] = s.repo.clone().into();
        e["advisory_notes"] = s.advisory_notes.into();
        if let Some((days, via)) = s.heartbeat {
            e["heartbeat"] = serde_json::json!({ "days": days, "via": via });
        }
    }
    match &r.outcome {
        Outcome::Unmaintained { advisory, summary } => {
            e["status"] = "declared_unmaintained".into();
            e["advisory"] = advisory.clone().into();
            e["advisory_summary"] = summary.clone().into();
        }
        Outcome::Archived => e["status"] = "archived".into(),
        Outcome::Modeled { p, lo, hi, pct, uncertain } => {
            e["status"] = "modeled".into();
            e["probability"] = serde_json::json!({ "point": p, "lo": lo, "hi": hi });
            e["percentile"] = (*pct).into();
            e["uncertain"] = (*uncertain).into();
        }
        Outcome::TooYoung { .. } => e["status"] = "too_young".into(),
        Outcome::NotFound => e["status"] = "not_found".into(),
        Outcome::Error(msg) => {
            e["status"] = "error".into();
            e["error"] = msg.clone().into();
        }
    }
    e
}

/// One compact block per dependency: the single-crate report's stats lines.
fn detail_block(r: &Assessment, today: NaiveDate) -> String {
    let (word, _) = verdict(&r.outcome);
    let mut b = format!("\n### {} — {}\n\n", r.dep.name, word);
    let Some(s) = &r.stats else {
        if let Outcome::Error(msg) = &r.outcome {
            b += &format!("* {msg}\n");
        }
        return b;
    };
    b += &format!(
        "* **{v}** versions · last release **{lr}** ({lrd}) · **{d}** dependents · {o} owner{pl} · age {a:.1}y\n",
        v = s.versions,
        lr = ago((today - s.last_release).num_days()),
        lrd = s.last_release,
        d = s.dependents,
        o = s.owners,
        pl = if s.owners == 1 { "" } else { "s" },
        a = s.age_days as f64 / 365.25
    );
    let notes = match s.advisory_notes {
        0 => "no other advisories".into(),
        n => format!("{n} other advisor{}", if n == 1 { "y" } else { "ies" }),
    };
    b += &format!("* repository: {} · {}\n", s.repo, notes);
    if let Some((days, via)) = s.heartbeat {
        b += &format!("* last sign of maintainer life: **{}** (via {via})\n", ago(days));
    }
    match &r.outcome {
        Outcome::Unmaintained { advisory, summary } => {
            b += &format!("* **{advisory}** (*{summary}*) — recorded fact; no forecast needed\n");
        }
        Outcome::Modeled { p, lo, hi, pct, .. } => {
            b += &format!(
                "* riskier than **{pct:.0}%** of the eligible registry · \
                 P(abandoned within ~2y) **{:.0}%** ({:.0}–{:.0}%)\n",
                p * 100.0, lo * 100.0, hi * 100.0
            );
        }
        _ => {}
    }
    b
}
