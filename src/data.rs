//! Static artifacts (exported by src/cli/export_artifacts.py in the
//! research repo) + the tree-walker, percentile lookup, and calibration
//! math. No network code here - everything in this module is exact,
//! offline, and covered by the parity harness.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

#[derive(serde::Deserialize)]
pub struct Meta {
    pub snapshot: String,
    pub model_tag: String,
    pub features: Vec<String>,
    pub eligibility_months: u32,
    pub horizon: String,
}

// node = [feat_idx, threshold, yes, no, missing]; feat_idx -1 => leaf,
// threshold slot holds the leaf value. Gap slots (non-contiguous node
// ids) arrive as JSON null and are never visited by traversal.
type Node = [f64; 5];

#[derive(serde::Deserialize)]
pub struct Bag {
    pub bias: f64,
    pub trees: Vec<Vec<Option<Node>>>,
}

#[derive(serde::Deserialize)]
pub struct CurvePoint {
    pub index: f64,
    pub p: f64,
}

#[derive(serde::Deserialize)]
pub struct Calib {
    #[allow(dead_code)] // deserialized from the artifact for provenance; not used at serve time
    pub method: String,
    pub coef_score: f64,
    pub coef_age_years: f64,
    pub intercept: f64,
    pub iso_index_curve: Vec<CurvePoint>,
    // bootstrap band SURFACE over (age, score) - a score-only band at
    // median age is badly wrong for old crates (age drives their index)
    pub grid_scores: Vec<f64>,
    pub grid_ages: Vec<f64>,
    pub lo2d: Vec<Vec<f64>>,
    pub hi2d: Vec<Vec<f64>>,
}

pub struct Artifacts {
    pub meta: Meta,
    pub bags: Vec<Bag>,
    pub cutpoints: Vec<f64>,
    pub calib: Calib,
    pub owners: HashMap<String, chrono::NaiveDate>,
}

fn gunzip(bytes: &[u8]) -> Result<String> {
    let mut s = String::new();
    flate2::read::GzDecoder::new(bytes).read_to_string(&mut s)?;
    Ok(s)
}

/// The parity sample, compiled into the binary (used by `--parity` when
/// no external data directory is present).
pub const EMBEDDED_PARITY: &str = include_str!("../data/parity_sample.json");

impl Artifacts {
    /// Load the artifacts compiled into the binary. This makes an installed
    /// `cargo-pulse` fully self-contained, with no external data directory.
    pub fn load_embedded() -> Result<Self> {
        Self::from_parts(
            include_str!("../data/meta.json"),
            include_bytes!("../data/model_trees.json.gz"),
            include_str!("../data/percentiles.json"),
            include_str!("../data/calibration.json"),
            include_bytes!("../data/owners.csv.gz"),
        )
    }

    /// Load the artifacts from a directory (dev builds, or `CARGO_PULSE_DATA`).
    pub fn load(dir: &Path) -> Result<Self> {
        let s = |n: &str| std::fs::read_to_string(dir.join(n)).with_context(|| format!("read {n}"));
        let b = |n: &str| std::fs::read(dir.join(n)).with_context(|| format!("read {n}"));
        Self::from_parts(
            &s("meta.json")?,
            &b("model_trees.json.gz")?,
            &s("percentiles.json")?,
            &s("calibration.json")?,
            &b("owners.csv.gz")?,
        )
    }

    fn from_parts(
        meta: &str,
        trees_gz: &[u8],
        percentiles: &str,
        calib: &str,
        owners_gz: &[u8],
    ) -> Result<Self> {
        let meta: Meta = serde_json::from_str(meta)?;
        let bags: Vec<Bag> = serde_json::from_str(&gunzip(trees_gz)?)?;

        #[derive(serde::Deserialize)]
        struct Pct {
            cutpoints: Vec<f64>,
        }
        let pct: Pct = serde_json::from_str(percentiles)?;
        let calib: Calib = serde_json::from_str(calib)?;

        let mut owners = HashMap::new();
        let mut rdr = csv::Reader::from_reader(flate2::read::GzDecoder::new(owners_gz));
        for rec in rdr.records() {
            let rec = rec?;
            if let (Some(name), Some(d)) = (rec.get(0), rec.get(1)) {
                if let Ok(date) = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                    owners.insert(name.to_string(), date);
                }
            }
        }
        Ok(Artifacts { meta, bags, cutpoints: pct.cutpoints, calib, owners })
    }

    /// Mean log-hazard over the bagged ensemble. `x` is the feature
    /// vector in meta.features order; NaN encodes missing.
    pub fn score(&self, x: &[f64]) -> f64 {
        let mut total = 0.0;
        for bag in &self.bags {
            let mut s = bag.bias;
            for tree in &bag.trees {
                s += eval_tree(tree, x);
            }
            total += s;
        }
        total / self.bags.len() as f64
    }

    /// Share of the undeclared registry this score outranks, in percent.
    pub fn percentile(&self, score: f64) -> f64 {
        let k = self.cutpoints.partition_point(|c| *c <= score);
        (k as f64 / (self.cutpoints.len() - 1) as f64 * 100.0).clamp(0.0, 100.0)
    }

    /// C4 calibration: P(abandoned within ~2y) at this crate's actual
    /// age, plus a [lo, hi] band read from the 2D bootstrap surface at
    /// the nearest (age, score) gridpoint.
    pub fn probability(&self, score: f64, age_years: f64) -> (f64, f64, f64) {
        let c = &self.calib;
        let idx = c.coef_score * score + c.coef_age_years * age_years + c.intercept;
        let p = interp_curve(&c.iso_index_curve, idx);
        let nearest = |grid: &[f64], v: f64| {
            grid.iter()
                .enumerate()
                .min_by(|a, b| (a.1 - v).abs().total_cmp(&(b.1 - v).abs()))
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        let ai = nearest(&c.grid_ages, age_years);
        let si = nearest(&c.grid_scores, score);
        let (lo, hi) = (c.lo2d[ai][si], c.hi2d[ai][si]);
        (p, lo.min(p), hi.max(p))
    }
}

fn eval_tree(nodes: &[Option<Node>], x: &[f64]) -> f64 {
    let mut i = 0usize;
    loop {
        let n = nodes[i].expect("traversal reached a gap node");
        if n[0] < 0.0 {
            return n[1];
        }
        let v = x[n[0] as usize];
        // f32 comparison replicates the booster exactly: hist thresholds
        // often sit ON shared feature values (log1p of integer counts),
        // and v == t must route right, which f64 comparison gets wrong
        i = if v.is_nan() {
            n[4] as usize
        } else if (v as f32) < (n[1] as f32) {
            n[2] as usize
        } else {
            n[3] as usize
        };
    }
}

fn interp_curve(curve: &[CurvePoint], idx: f64) -> f64 {
    let first = curve.first().expect("empty iso curve");
    let last = curve.last().unwrap();
    if idx <= first.index {
        return first.p;
    }
    if idx >= last.index {
        return last.p;
    }
    let j = curve.partition_point(|c| c.index <= idx);
    let (a, b) = (&curve[j - 1], &curve[j]);
    if (b.index - a.index).abs() < 1e-12 {
        return a.p;
    }
    a.p + (b.p - a.p) * (idx - a.index) / (b.index - a.index)
}

/// Train/serve harness: re-score the 60 dump-computed feature vectors
/// and compare against the Python ensemble's scores.
pub fn parity_check(parity_json: &str, art: &Artifacts) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        features: Vec<Option<f64>>,
        score: f64,
    }
    let rows: Vec<Row> = serde_json::from_str(parity_json)?;
    let mut worst = (0.0f64, String::new());
    for r in &rows {
        let x: Vec<f64> = r.features.iter().map(|v| v.unwrap_or(f64::NAN)).collect();
        let d = (art.score(&x) - r.score).abs();
        if d > worst.0 {
            worst = (d, r.name.clone());
        }
    }
    println!(
        "parity: {} crates, max |rust - python| = {:.2e} (worst: {})",
        rows.len(),
        worst.0,
        worst.1
    );
    if worst.0 > 1e-3 {
        anyhow::bail!("PARITY FAIL - tree walker diverges from the Python ensemble");
    }
    println!("parity PASS (tolerance 1e-3)");
    Ok(())
}
