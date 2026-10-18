# cargo-pulse

**Abandonment risk for crates.io packages - explicit signals first, a calibrated two-year probability second.**

`cargo-pulse` tells you whether a Rust crate is at risk of becoming abandoned.
A recorded fact always outranks a forecast: if a crate is already declared
unmaintained (RustSec) or its repository is archived, the tool reports that and
stops. Only for a live, undeclared, sufficiently old crate does it run a model -
a bagged survival (Cox) model over registry and repository signals - and report a
**calibrated probability of abandonment within ~2 years**, with an uncertainty band.

```
$ cargo pulse serde
✓ crates.io    316 versions · 114570 dependents
✓ RustSec/OSV  no advisories
✓ repository   github: live
✓ scoring      100 bags · percentile 0

✓ LOW RISK - roughly 0–3% chance of abandonment within ~2 years (point 1%)
```

## Install

```sh
cargo install cargo-pulse
cargo pulse <crate>          # e.g. cargo pulse serde
```

The model and its data are compiled into the binary, so no extra files or setup
are needed.

## Usage

```sh
cargo pulse <crate>             # terminal report (default)
cargo pulse <crate> --markdown  # raw markdown (paste-ready)
cargo pulse <crate> --json      # machine-readable (CI gates)
cargo pulse audit [path]        # risk table for a project's direct dependencies
cargo pulse audit --detail      # + one full stats block per dependency
cargo pulse audit --out r.md    # also write the report to a file (.md or .json)
cargo pulse --parity            # self-check: Rust scorer vs the reference model
```

**GitHub token.** Live repository checks use the GitHub API. Without a
`GITHUB_TOKEN` the tool makes the same calls anonymously (60 requests/h, about
5 per live crate); a token raises this to 5,000/h. If the rate limit is hit,
cargo-pulse **stops with an error** asking for a token instead of forecasting
without the repository activity signals. A token needs no
scopes: create one at https://github.com/settings/tokens, then
`export GITHUB_TOKEN=<token>`.

**Caching.** GitHub responses are cached on disk (`$XDG_CACHE_HOME/cargo-pulse`
or `~/.cache/cargo-pulse`) with their ETags; repeat lookups revalidate with
conditional requests.

## Audit a project

`cargo pulse audit` reads the project's metadata (via `cargo metadata`), runs
every direct dependency through the same pipeline. Dev and build dependencies are included and flagged; `--json` emits
the full structured list for CI.

```
$ cd my-project && cargo pulse audit
✓ project      my-project · 17 direct dependencies
✓ auditing     17 dependencies · 1 declared/archived · 2 elevated · 14 low

│   crate    │required│kind│       verdict        │P(abandoned)│
├────────────┼────────┼────┼──────────────────────┼────────────┤
│lazy_static │^1      │    │✗ unmaintained (RUST…)│—           │
│old-parser  │^0.3    │    │⚠ elevated            │38% (24–55%)│
│…           │        │    │                      │            │

summary — 1 declared/archived · 2 elevated · 14 low
```


## How it works

1. **crates.io** - does the crate exist? current stats.
2. **RustSec / OSV** - declared unmaintained? → report and stop.
3. **Forge** - repository archived or deleted? → report and stop.
4. **Age gate** - at least 18 months old? otherwise "too young to assess".
5. **Forecast** - assemble features, score with the bagged Cox ensemble, and
   report a registry percentile and a gold-calibrated two-year probability with a
   bootstrap band.


## Build from source

```sh
git clone https://github.com/RiyaMathew-11/cargo-pulse
cd cargo-pulse
cargo build --release
./target/release/cargo-pulse serde
```

With Nix (flakes), no Rust toolchain required:

```sh
nix run github:RiyaMathew-11/cargo-pulse -- serde      # run without installing
nix profile install github:RiyaMathew-11/cargo-pulse   # or install it
nix develop                                            # dev shell (cargo, rustc, clippy)
```

## License

Licensed under the GNU Affero General Public License v3.0 or later
([AGPL-3.0-or-later](LICENSE)).
