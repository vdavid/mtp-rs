# Dependency updates

Renovate (the Mend-hosted GitHub app) opens every dependency PR, and nobody runs `cargo update` by hand. This doc is
the playbook for triaging those PRs, plus the per-dependency decisions that `renovate.json` can't encode on its own.
Read it before merging or closing a Renovate PR.

## How Renovate is set up

- **Config**: `renovate.json`. Every package rule has a `description` with its reason, so read those first.
- **Release age**: every update waits three days after release (`minimumReleaseAge`). The `renovate/stability-days`
  check on a PR shows it passed.
- **Non-major updates**: one grouped PR ("all non-major updates"), created weekly before 6am Monday (Europe/Budapest)
  and automerged once CI is green.
- **Major updates**: one grouped PR ("all major updates"), created monthly on the 1st, never automerged. Closing it
  doesn't stick: Renovate recreates it. To drop one package from it, add a rule (`allowedVersions` to hold a version,
  or `enabled: false`) and Renovate rebuilds the branch without it on its next run.
- **Security fixes**: immediate PRs, automerged.
- **Own PR, never automerged, labeled `needs-hardware-test`**: `nusb` and `windows`. CI can't observe how they behave
  against a real device.
- **Not managed at all**: `windows-core` (moves with `windows`), the Rust toolchain in workflows (moves with the
  MSRV), and the libmtp benchmark crate (CI never builds it).
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

- **`<crate>@X requires rustc Y`, only on the `rust 1.85` jobs**: the new version raised its MSRV above ours.
  Dev-dependencies count, because the `Check (…, rust 1.85)` matrix jobs run the tests on 1.85. Either hold it with
  `allowedVersions: "<X.0.0"` and a description naming the MSRV that lifts the hold, or raise our MSRV (a deliberate
  decision, see "Raising the MSRV").
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
- **`serial_test`** (dev-dependency): held below 4. The 4.x line needs Rust 1.93.1 and brings only a `syn` 3 bump.
  Lift the hold when the MSRV reaches 1.93.1.
- **`actions/checkout`**: majors are safe here. The v7 breaking change blocks checking out fork PR code under
  `pull_request_target` and `workflow_run`, and our workflows use neither trigger. Re-check if one gets added.
- **Rust toolchain in workflows**: Renovate's `github-actions` manager reads `toolchain:` inputs as a `rust`
  dependency. Unmanaged, it bumped the `msrv` job to 1.98 (PR #28, automerged), so a job named "Verify MSRV (1.85)"
  checked 1.98 while nothing looked wrong. The rule keeps Renovate away from it.

## Raising the MSRV

A deliberate, consumer-visible change. Touch every one of these in the same commit:

- `Cargo.toml`: workspace `rust-version`.
- `.github/workflows/ci.yml`: the matrix `rust` list, and the `msrv` job's name and `toolchain`.
- `justfile`: the `msrv` recipe (its comments, the `rustup run` probe, and `cargo +1.85.0`).
- `README.md` and `crates/mtp-rs/README.md`: the MSRV badge.
- `CONTRIBUTING.md`: the MSRV section.
- `CHANGELOG.md`: a **Breaking** entry ("MSRV raised from A to B").
- `renovate.json` and this doc: lift any hold the new MSRV unblocks (`serial_test` at 1.93.1, `windows` 0.100 at
  1.95).
