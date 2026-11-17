# CI and release packaging

GitHub Actions runs the same checks as `just verify` on every push to `main` and
every pull request, builds the five supported platforms, and publishes a GitHub
Release when a `v*` tag is pushed. Nothing here publishes to crates.io or any
package registry; the GitHub Release is the only distribution channel.

## Workflows

| File | Trigger | What it does |
|---|---|---|
| `.github/workflows/ci.yml` | push to `main`, pull request, manual | calls the shared verify workflow |
| `.github/workflows/verify.yml` | `workflow_call` | fmt, clippy `-D warnings`, Rust tests, `npm ci`, tsc + UI typecheck, vitest, both stress scripts |
| `.github/workflows/release.yml` | tag `v*` | verify → build matrix → package → publish |
| `.github/workflows/release.yml` | `workflow_dispatch` | verify → build matrix → upload artifacts only (no release) |

Successful runs upload one artifact per target (`apm-<triple>`), kept for 7 days.
A tag release additionally attaches every archive plus an aggregate
`SHA256SUMS.txt` to the GitHub Release.

Publishing is **idempotent**: re-running the workflow for the same tag (or
retrying after a partial upload) replaces the assets with
`gh release upload --clobber` and keeps the release title in sync instead of
failing with "release already exists". Notes are generated once on creation and
are not rewritten on retries.

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
| `macos-14` | `aarch64-apple-darwin` | native |
| `macos-14` | `x86_64-apple-darwin` | cross-compile on arm64: `rustup target add` only |
| `windows-2022` | `x86_64-pc-windows-msvc` | native MSVC |

Cross-compilation notes:

- **macOS x86_64** is the only cross build. `dtolnay/rust-toolchain` installs the
  `x86_64-apple-darwin` standard library, and Apple's SDK is universal, so the
  same `macos-14` runner links both architectures. No Docker, Zig or SDK
  download is involved. The x86_64 binary is not executed in CI (no Rosetta
  guarantee); it is smoke-tested by `scripts/verify-package.sh` on an Intel Mac.
- **Linux arm64** uses GitHub's arm64 hosted runner. If that label disappears,
  replace that matrix entry with a `cross` build on `ubuntu-24.04`
  (`cargo install cross`, `cross build --target aarch64-unknown-linux-gnu`);
  `cross` needs Docker, which is present on Linux runners.
- **Windows** is built natively with MSVC. SQLite is compiled from the bundled
  `rusqlite` amalgamation, so no system SQLite or vcpkg is required. Archives
  are `.zip` built with 7-Zip (preinstalled on the runner).
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
