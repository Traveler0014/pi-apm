# CI and release packaging

GitHub Actions runs the same checks as `just verify` on every push to `main` and
every pull request, builds the five supported platforms, and publishes a GitHub
Release when a `v*` tag is pushed. Nothing here publishes to crates.io or any
package registry; the GitHub Release is the only distribution channel.

## Workflows

| File | Trigger | What it does |
|---|---|---|
| `.github/workflows/ci.yml` | push to `main`, pull request, manual | calls the shared verify workflow |
| `.github/workflows/verify.yml` | `workflow_call` | fmt, clippy `-D warnings`, Rust tests, `npm ci`, **`ui/dist` rebuild + freshness gate**, tsc + UI typecheck, vitest, both stress scripts, release-script tests |
| `.github/workflows/release.yml` | tag `v*` | gate → verify → build matrix → package → publish |
| `.github/workflows/release.yml` | push `main` touching `Cargo.toml` | gate → verify → build → **auto-tag** `v<version>` → publish |
| `.github/workflows/release.yml` | `workflow_dispatch` | gate → verify → build → upload artifacts only (no release) |

`release-gate.sh` reads `Cargo.toml` and decides the run:

| State | Result |
|---|---|
| tag `v<version>` **and** its GitHub Release both exist | whole pipeline skipped (`run=false`); `ci.yml` still verifies the commit |
| tag exists, Release missing (partial earlier run) | build + publish reuse the tag; publish is idempotent |
| neither exists (new version on `main`) | build, create the tag at `GITHUB_SHA`, publish |
| `workflow_dispatch` | always builds artifacts, never publishes |

Successful runs upload one artifact per target (`apm-<triple>`), kept for 7 days.
A tag release additionally attaches every archive plus an aggregate
`SHA256SUMS.txt` to the GitHub Release.

Publishing is **idempotent**: re-running the workflow for the same tag (or
retrying after a partial upload) replaces the assets with
`gh release upload --clobber` and keeps the release title in sync instead of
failing with "release already exists". Notes are generated once on creation and
are not rewritten on retries.

## Automatic release on a version bump

The normal flow needs no manual tag:

1. bump `version` in `Cargo.toml` and merge it to `main`;
2. `release.yml` triggers (it only watches `Cargo.toml` changes on `main`);
3. the gate finds no `v<version>` tag/release, runs verify and the five-platform
   build, creates `v<version>` at that commit with `GITHUB_TOKEN`, and publishes;
4. the release assets are the same archives and aggregate `SHA256SUMS.txt` as a
   manual tag.

A tag created with `GITHUB_TOKEN` does **not** start another workflow run, so the
auto-tag cannot recurse into the tag-push entry point. Tag pushes and
`workflow_dispatch` remain available: the first behaves like a normal release,
the second builds artifacts for inspection without publishing.

Two operational notes:

- If a run is interrupted *after* the Release was created but before all assets
  uploaded, the gate will skip the next push (tag + release exist). Repair it by
  re-running just the publish job with `--clobber`, or delete the incomplete
  Release and push again; the script is create-or-update by design.
- Deleting a published tag/release is not a rollback. Fix forward with a new
  patch version; existing installs can still download older releases by tag.

```bash
# manual dry run: Actions → release → Run workflow (no tag, no release)
gh workflow run release.yml --ref main
gh run watch
```

## Build matrix

| Runner | Target | Strategy |
|---|---|---|
| `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | native |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | native arm64 hosted runner |
| `macos-15-intel` | `x86_64-apple-darwin` | native |
| `macos-15-intel` | `aarch64-apple-darwin` | cross-compile on x64: `rustup target add` only |
| `windows-2022` | `x86_64-pc-windows-msvc` | native MSVC |

Cross-compilation notes:

- **macOS runs on an Intel runner.** This account (a personal, public-repo
  owner) has no arm64 macOS runner: `macos-15`, `macos-26` and `macos-latest`
  all resolve to arm64 here, and arm64 jobs fail with
  `The configuration 'macos-14-arm64-us-default' is not supported`. The matrix
  therefore uses `macos-15-intel`, the official x64 (Intel) macOS 15 image
  published in `actions/runner-images` (supported until 2027-08):
  `x86_64-apple-darwin` is built natively and executed for the smoke test, while
  `aarch64-apple-darwin` is cross-compiled on the same runner. Apple's SDK is
  universal, so `rustup target add aarch64-apple-darwin` plus
  `cargo build --target` is enough — no Docker, Zig or extra SDK. The arm64
  binary cannot run on the Intel host, so its smoke flag is `false`; verify it
  on an Apple Silicon machine (or through `scripts/verify-package.sh`).
- **Label history:** `macos-13` (the previous Intel image) was retired, which
  left both macOS jobs queued forever — a job whose label matches no runner is
  never picked up and does not consume its `timeout-minutes`
  (see APM-43/APM-47). The migration is a two-line label swap; targets, smoke
  flags and publish logic are unchanged.
- **Label fallback:** if `macos-15-intel` is retired, move to the next
  `macos-*-intel` x64 label (`macos-26-intel` is the current generation) and
  verify it in the official `actions/runner-images` image table. Never use
  `macos-15`/`macos-26`/`macos-latest` here: they resolve to arm64 and fail with
  `The configuration 'macos-14-arm64-us-default' is not supported`.
  `actionlint` keeps a static runner-label list that can lag behind new labels
  and then reports `unknown label` for a valid image; the official image table
  takes precedence, so do not "fix" that warning by downgrading to an older or
  retired label. `macos-15-intel` entered that list in `actionlint` v1.7.12
  (checked in `rule_runner_label.go`), so `just lint-workflows` needs at least
  that version — the binary used for this migration was v1.7.7 and reports one
  `label "macos-15-intel" is unknown` warning while still accepting the retired
  `macos-13`. CI does not run `actionlint` (`.github/workflows/verify.yml` runs
  fmt/clippy/test/vitest/stress only), so the warning is local-tooling noise.
- **Linux arm64** uses GitHub's arm64 hosted runner. If that label disappears,
  replace that matrix entry with a `cross` build on `ubuntu-24.04`
  (`cargo install cross`, `cross build --target aarch64-unknown-linux-gnu`);
  `cross` needs Docker, which is present on Linux runners.
- **Windows** is built natively with MSVC. SQLite is compiled from the bundled
  `rusqlite` amalgamation, so no system SQLite or vcpkg is required. Archives
  are `.zip`; the packager converts the Git Bash `/d/a/...` path with `cygpath -w`
  before handing it to native tools (7-Zip, bsdtar, Python). Fallback chain:
  `7z` → default 7-Zip install → `bsdtar` → Windows `tar -a` → `python3`/`python`
  zipfile → MSYS `zip`; if none exists the step fails with an explicit message.
  Checksums fall back from `sha256sum`/`shasum` to Windows `certutil`, still
  emitting the standard `<hash>  <file>` format.
  `scripts/test-package-windows.sh` covers this branch on Linux with fake
  `cygpath`/`7z`: the archive path must be Windows-style, the six files and the
  checksums must be unchanged, and a missing toolchain must fail clearly.
- Rust **edition 2024** requires a recent stable toolchain. The version is
  **pinned**: `rust-toolchain.toml` at the repository root selects `1.100.0`
  with `rustfmt` and `clippy`, and both workflows pass `toolchain: 1.100.0`.
  Bump both places together; `cargo`/`rustup` then use the same compiler
  locally, which avoids "green locally, red in CI" clippy drift.

## Packages

Each archive is named `apm-<version>-<target>.(tar.gz|zip)` and contains exactly:

```
apm-<version>-<target>/
  apm            (apm.exe on Windows)
  apm-mcp
  apm-serve
  README.md
  LICENSE
  SHA256SUMS      # covers the five files above
```

No `.git`, `target/`, `.apm/`, database, token file, log or test fixture is
packaged; `scripts/package-release.sh` copies only named files, and
`scripts/verify-package.sh` rejects unexpected top-level entries.

## Supply-chain policy

- Every third-party action is pinned to a **commit SHA** with the human-readable
  tag in a comment (`actions/checkout`, `actions/setup-node`,
  `actions/upload-artifact`, `actions/download-artifact`,
  `dtolnay/rust-toolchain`). Update a pin only with a reviewed diff; resolve the
  new SHA with `gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`.
- `permissions` is `contents: read` at workflow level. Only the release job
  elevates to `contents: write`, and it receives nothing else (no packages, no
  issues, no id-token).
- Publishing uses `gh release create` with `GITHUB_TOKEN`; no third-party release
  action sees the token.
- Artifacts are uploaded only from `dist/`; logs and build directories are never
  attached. Release publishing refuses a tag whose name does not match
  `Cargo.toml`'s version.

## Local packaging and verification

```bash
just build                      # release binaries for this machine
just release-package            # dist/apm-<version>-<host-target>.tar.gz
just release-check dist/apm-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

Both publishing branches are covered offline by `just test-release-scripts`
(also run in CI): a fake `gh` asserts `release create` for a fresh tag,
`release upload --clobber` + title sync for an existing one, that an upload
failure propagates, and that a missing `SHA256SUMS.txt` is refused before any
GitHub call.

`release-check` verifies:

1. exactly one top-level directory and the six required files;
2. no `.git`, `target`, `.apm`, `*.db`, token or log files;
3. `SHA256SUMS` matches every file;
4. when the package targets the host: `--version` for `apm`, `apm-mcp` and
   `apm-serve`, then a real CLI round-trip (`init → create → get → dashboard`)
   and `apm ui --help`.

Use `--skip-run` to check only structure/checksums (for a foreign package).

## Download, install, upgrade, rollback

```bash
# 1. Pick a release; download the archive and the aggregate checksum list
gh release download v0.1.0 \
  --pattern 'apm-0.1.0-x86_64-unknown-linux-gnu.tar.gz' \
  --pattern 'SHA256SUMS.txt'

# 2. Verify just this archive against the aggregate list (the list covers all
#    platforms, so check one line instead of `sha256sum -c` on the whole file)
grep ' apm-0.1.0-x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS.txt | sha256sum -c -
# macOS has no sha256sum: use `shasum -a 256 -c -` instead.

tar -xzf apm-0.1.0-x86_64-unknown-linux-gnu.tar.gz
cd apm-0.1.0-x86_64-unknown-linux-gnu && sha256sum -c SHA256SUMS   # per-package list

# 2. Install all three binaries (macOS/Linux)
install -m 0755 apm apm-mcp apm-serve ~/.local/bin/
apm --version && apm-mcp --version && apm-serve --version

# Windows (PowerShell)
# Expand-Archive apm-0.1.0-x86_64-pc-windows-msvc.zip -DestinationPath .
# Copy-Item .\apm-0.1.0-x86_64-pc-windows-msvc\*.exe $env:USERPROFILE\.local\bin
```

Upgrade = install the newer release over the same paths; the SQLite ledger
(`.apm/apm.db`) is forward-compatible and unchanged by binary replacement.

Rollback = reinstall the previous release archive (the same three binaries).
Keep `SHA256SUMS.txt` from each release; do not "roll back" by editing the
database. If a release is known-bad, publish a new patch tag rather than deleting
the old tag — existing installs keep working and can reinstall the previous
archive by tag.

## First remote verification (no tag yet)

1. Push the branch and wait for the `ci` workflow (verify job) to pass.
2. Run `gh workflow run release.yml --ref <branch>`; confirm all five matrix jobs
   are green and five `apm-<triple>` artifacts exist.
3. Download one artifact and run `just release-check <archive>` (or
   `--skip-run` for a foreign platform). For a foreign platform, run the
   downloaded binaries on that platform once.
4. Only then create the tag:

   ```bash
   git tag -a v0.1.0 -m "apm 0.1.0"
   git push origin v0.1.0        # this publishes the release
   gh release view v0.1.0
   ```

If anything fails after the tag, fix forward with a new tag; do not move or
delete a published tag.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `tag vX does not match Cargo.toml version` | bump `Cargo.toml` before tagging |
| `missing target/.../apm (build with --target … first)` | package step ran before a successful build, or wrong `--bin-dir` |
| `python3: command not found` in stress | the verify job runs on Ubuntu, which ships python3 |
| `ubuntu-24.04-arm` label unavailable | switch that matrix row to a `cross` build (see above) |
| release job skipped | expected on `workflow_dispatch`; only `refs/tags/v*` publishes |
| `gh: HTTP 403` when publishing | the release job needs `permissions: contents: write` |
| re-running a tag workflow | safe: existing assets are replaced with `--clobber`, title is synced, notes stay as generated |
