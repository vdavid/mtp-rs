# Dependency updates

Renovate (the Mend-hosted GitHub app) opens every dependency PR, and nobody runs `cargo update` by hand. This doc is
the playbook for triaging those PRs, plus the per-dependency decisions that `renovate.json` can't encode on its own.
Read it before merging or closing a Renovate PR, or before touching `rust-version`.

## MSRV policy

- **Promise**: we support the Rust releases from the last 12 months. `rust-version` in the root `Cargo.toml` is the
  oldest stable released within that window (or older, if nothing needs newer).
- **Raising it** is a minor version bump (0.x: the `x`), with a **Breaking** changelog line. Raise it when a dependency
  or a language feature needs it; don't chase the window for its own sake.
- **Scope**: what consumers compile, so `mtp-rs` (every feature) and `mtp-rs-cli`, on each OS. Tests, examples,
  dev-dependencies, and the `mtp-bench` benchmark run on stable only.
- ❌ **Never cap a dependency in `Cargo.toml` (or hold it in `renovate.json`) for the MSRV.** A cap forces every
  consumer's graph onto the old version, even consumers on a new Rust. Consumers on the MSRV get a buildable version
  from Cargo's MSRV-aware resolver instead (`resolver = "3"`, the edition 2024 default, or
  `resolver.incompatible-rust-versions = "fallback"`).

## How CI checks the MSRV

The `msrv` job (and `just msrv`) runs `cargo update` with `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback` on the
MSRV toolchain before `cargo check`, so it builds the graph a consumer on the MSRV would resolve, never the committed
`Cargo.lock`. `just msrv` puts the committed `Cargo.lock` back afterward and builds into `target/msrv`.

So the committed `Cargo.lock` is free to hold versions that need a newer Rust: it serves our stable-only tests and the
CLI binary we ship. A red `msrv` job means a consumer on the MSRV really can't build us: a dependency raised its MSRV
in a version our `Cargo.toml` floor requires, or it published no version our MSRV can build. Raise our MSRV, or (rarely)
lower our floor.

## Why Renovate can't keep `Cargo.lock` MSRV-compatible

Verified on 2026-10-27 against Renovate `main` (`lib/modules/manager/cargo/artifacts.ts`) and by running Renovate
44.181.0 locally (`--platform=local --dry-run=lookup`) with cargo 1.99.1:

- **In-range updates force the version.** With `rangeStrategy: "update-lockfile"` (cargo's default under `auto`),
  Renovate runs `cargo update --package <crate>@<locked> --precise <new>`, then `cargo update --workspace`. `--precise`
  ignores the MSRV-aware resolver: in a `rust-version = "1.87"` crate it took `sha2` 0.11.0 → 0.11.1 ("requires Rust
  1.88"), and the `--workspace` pass kept it. That's how PR #48 (and smb2's #25) went red on the MSRV jobs while
  touching only `Cargo.lock`.
- **`constraintsFiltering: "strict"` with `constraints.rust` doesn't fix it.** The `crate` datasource does attach each
  release's `rust_version`, but the comparison is unreliable: `"1.87"` let `serial_test` 4.0.1 (`rust_version`
  "1.93.1") through, a range like `"<=1.88.0"` filtered out every release (compatible ones too), and
  `semver-coerced` constraint versioning let 4.0.1 through again. Renovate doesn't read `rust-version` from
  `Cargo.toml` either (renovatebot/renovate#26314, open).
- **`lockFileMaintenance` would respect the MSRV** (it runs a plain `cargo update`), but Renovate can't apply
  `minimumReleaseAge` to it, so it would drop our three-day window. Not enabled.

Hence the split above: Renovate keeps `Cargo.lock` current with `--precise` (and the three-day window), and the MSRV
job checks the consumer's view independently.

## How Renovate is set up

- **Config**: `renovate.json`. Every package rule has a `description` with its reason, so read those first.
- **Release age**: every update waits three days after release (`minimumReleaseAge`). The `renovate/stability-days`
  check on a PR shows it passed.
- **Cargo range strategy**: `update-lockfile`, pinned by a rule. In-range updates move only `Cargo.lock`; a `Cargo.toml`
  floor is a promise to consumers and moves only on purpose (a fix we need, like `nusb`'s, or a major bump).
- **Non-major updates**: one grouped PR ("all non-major updates"), created weekly before 6am Monday (Europe/Budapest)
  and automerged once CI is green.
- **Major updates**: one grouped PR ("all major updates"), created monthly on the 1st, never automerged. Closing it
  doesn't stick: Renovate recreates it. To drop one package from it, add a rule (`allowedVersions` to hold a version,
  or `enabled: false`) and Renovate rebuilds the branch without it on its next run.
- **Security fixes**: immediate PRs, automerged.
- **Own PR, never automerged, labeled `needs-hardware-test`**: `nusb` and `windows`. CI can't observe how they behave
  against a real device.
- **Not managed at all**: `windows-core` (moves with `windows`), the Rust toolchain in workflows (moves with the
  MSRV), the libmtp benchmark crate (CI never builds it), and `dist`'s generated `mtp-rs-cli-release.yml` (its
  actions and runners move with `cargo-dist-version` and `dist generate`, and a newer Ubuntu runner would raise the
  glibc the Linux CLI binaries need). The hand-written `publish-homebrew-formula.yml` stays managed.
- **0.x "minor" bumps are breaking, but Renovate files them as minor.** Cargo treats `0.62` → `0.100` as incompatible,
  yet Renovate puts it in the automerge group, and CI is then the only gate. Any 0.x crate whose behavior CI can't
  observe needs its own no-automerge rule, like `nusb` and `windows` have.
- **Dependency Dashboard**: issue #19 lists open PRs, held and pending updates, and every detected dependency. It also
  has a checkbox that triggers a Renovate run.
- **Merging by hand**: squash-merge (`gh pr merge <n> --squash --delete-branch`), which matches what automerge
  produces (`Update … (#28)`).

## Triage a Renovate PR

1. Run `gh pr view <n>` (the body carries the release notes) and `gh pr checks <n>`.
2. For each red job, run `gh run view <run-id> --job <job-id> --log-failed` and look for the `error` lines.
3. Match the failure against the signatures below.
4. If it's green, still check the per-dependency notes: green CI doesn't cover device behavior.

## Failure signatures

- **`<crate>@X requires rustc Y`, only on the `Verify MSRV` jobs**: a consumer on our MSRV can't build us anymore (see
  "How CI checks the MSRV"). Raise our MSRV if Y is inside the 12-month window (see "Raising the MSRV"); otherwise
  look for why our floor needs that version. Never add an `allowedVersions` hold or a `Cargo.toml` cap for it.
- **Trait bounds not satisfied, with two versions of one crate in the paths** (for example
  `IPortableDeviceEventCallback: Interface`, with both `windows-core-0.62.2` and `windows-core-0.100.0` in the notes):
  Renovate bumped one crate of a family that must move together. See "Crates that move together".
- **Only the Windows jobs fail**: the `windows` family, or code under `cfg(windows)`.

## Crates that move together

- **`windows` and `windows-core`**: pinned to the same exact version (`=`) in `crates/mtp-rs/Cargo.toml`.
  `windows-core` is a direct dependency only so the `#[implement]` macro's generated `::windows_core` paths resolve
  (the WPD event callback in `mtp::backend::wpd::events`). Renovate ignores `windows-core`, so when a `windows` PR
  arrives, bump `windows-core` to the same version in that PR.

## Per-dependency notes

- **`nusb`**: the whole USB layer. Green CI proves only the mock and virtual transports. Before merging, run the
  integration suite against a real device (AGENTS.md "Testing", and `docs/debugging.md` for device setup).
- **`windows` and `windows-core`**: the WPD backend. CI on `windows-latest` compiles and runs the conformance harness
  but has no device, so verify against a phone on Windows before merging.
  - **The 0.100 line**: windows-rs published `windows-core` 0.100.0 on 2026-09-03 but held back `windows` 0.100 while
    they test new metadata ([announcement](https://github.com/microsoft/windows-rs/issues/4867)). It still wasn't on
    crates.io on 2026-09-13. Until it is, stay on `=0.62.2`.
  - **Moving to 0.100 is a migration**: Win32 bindings are now generated from the Windows SDK headers, so Cargo
    features, imports, signatures (raw `HRESULT`s, pointer/count parameters), enum and ownership types, and module
    grouping (by header name) can all change. Its MSRV is 1.95, above ours.
  - **Worth weighing at that point**: windows-rs recommends that libraries skip the `windows` umbrella crate and
    generate only the APIs they use with `windows-bindgen`. That means fewer dependencies and less version churn for
    downstream consumers such as Cmdr.
- **`actions/checkout`**: majors are safe here. The v7 breaking change blocks checking out fork PR code under
  `pull_request_target` and `workflow_run`, and our workflows use neither trigger. Re-check if one gets added.
- **Rust toolchain in workflows**: Renovate's `github-actions` manager reads `toolchain:` inputs as a `rust`
  dependency. Unmanaged, it bumped the `msrv` job to 1.98 (PR #28, automerged), so a job named "Verify MSRV (1.85)"
  checked 1.98 while nothing looked wrong. The rule keeps Renovate away from it.

## Raising the MSRV

A deliberate, consumer-visible change. Touch every one of these in the same commit:

- `Cargo.toml`: workspace `rust-version`.
- `.github/workflows/ci.yml`: the `msrv` job's name and `toolchain`.
- `justfile`: the `msrv` recipe (its comments, the `rustup run` probe, and `cargo +1.xx.0`) and the header lines.
- `README.md` and `crates/mtp-rs/README.md`: the MSRV badge and the MSRV paragraph.
- `CONTRIBUTING.md`: the MSRV section and the "Submitting changes" step.
- `CHANGELOG.md`: a **Breaking** entry ("The MSRV is now Rust B (was A)"), and a minor version bump at release.
- This doc: the `windows` 0.100 note, if the new MSRV reaches 1.95.
