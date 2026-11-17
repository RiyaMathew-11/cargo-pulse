# cargo-pulse

**Abandonment risk for crates.io packages: recorded facts first, a calibrated two-year probability second.**

`cargo-pulse` shows the abandonment risk of a Rust crate.
A recorded fact has priority over a forecast.
If RustSec declares the crate unmaintained, or its repository is archived, the tool reports this and stops.
For a live crate that is old enough, a bagged survival (Cox) model scores registry and repository signals.
The result is a **calibrated probability of abandonment in approximately two years**, with an uncertainty band.

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

The binary contains the model and its data. No other files are necessary.

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

**GitHub token.** The repository check uses the GitHub API.
Without a `GITHUB_TOKEN`, GitHub permits 60 requests each hour. One live crate uses approximately 5.
With a token, the limit is 5,000 each hour. The token needs no scopes.
At the limit, the tool stops with an error. It does not forecast without repository signals.
Create a token at https://github.com/settings/tokens, then run `export GITHUB_TOKEN=<token>`.

**Cache.** GitHub responses are stored with their ETags in `$XDG_CACHE_HOME/cargo-pulse` or `~/.cache/cargo-pulse`.
A repeat lookup sends a conditional request.

## Audit a project

`cargo pulse audit` reads the project metadata with `cargo metadata` and runs each direct dependency through the same stages.
Dev and build dependencies are included and flagged. The `--json` flag gives the full list for CI.
crates.io lookups run at one request each second, as the [crates.io data access policy](https://crates.io/data-access) requires.
OSV and GitHub lookups run on three threads.
An audit takes approximately 3 s for each dependency, almost all network wait.

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

1. **crates.io.** Check that the crate exists and read its stats.
2. **RustSec / OSV.** If declared unmaintained, report and stop.
3. **Forge.** If the repository is archived or deleted, report and stop.
4. **Age gate.** If the crate is less than 18 months old, report "too young to assess" and stop.
5. **Forecast.** Score the features with the bagged Cox ensemble.
   Report a registry percentile and a calibrated two-year probability with a bootstrap band.

## Build from source

```sh
git clone https://github.com/RiyaMathew-11/cargo-pulse
cd cargo-pulse
cargo build --release
./target/release/cargo-pulse serde
```

With Nix flakes, no Rust toolchain is necessary:

```sh
nix run github:RiyaMathew-11/cargo-pulse -- serde      # run without installing
nix profile install github:RiyaMathew-11/cargo-pulse   # or install it
nix develop                                            # dev shell (cargo, rustc, clippy)
```

## License

GNU Affero General Public License v3.0 or later ([AGPL-3.0-or-later](LICENSE)).
