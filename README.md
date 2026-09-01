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
cargo pulse <crate>             # coloured terminal report (default)
cargo pulse <crate> --markdown  # raw markdown (paste-ready)
cargo pulse <crate> --json      # machine-readable (CI gates)
cargo pulse --parity            # self-check: Rust scorer vs the reference model
```

An optional `GITHUB_TOKEN` raises the GitHub API rate limit used for live
repository checks.

## How it works

1. **crates.io** - does the crate exist? current stats.
2. **RustSec / OSV** - declared unmaintained? → report and stop.
3. **Forge** - repository archived or deleted? → report and stop.
4. **Age gate** - at least 18 months old? otherwise "too young to assess".
5. **Forecast** - assemble features, score with the bagged Cox ensemble, and
   report a registry percentile and a gold-calibrated two-year probability with a
   bootstrap band.

The probability is calibrated against a human/LLM-judged gold standard. The model
ties a simple "months-since-last-release" rule on the general population and beats
it on independent signals, while adding a calibrated probability a bare rule
cannot provide. Full method, evaluation, and caveats are in the accompanying MSc
dissertation.

## Build from source

```sh
git clone https://github.com/RiyaMathew-11/cargo-pulse
cd cargo-pulse
cargo build --release
./target/release/cargo-pulse serde
```

## License

Licensed under the GNU Affero General Public License v3.0 or later
([AGPL-3.0-or-later](LICENSE)).
