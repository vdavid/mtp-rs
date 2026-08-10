# mtp-rs Development Commands
# ============================
#
# This is a Cargo workspace with two published crates:
#   - mtp-rs       (the library)        crates/mtp-rs/
#   - mtp-rs-cli   (the CLI binary)     crates/mtp-rs-cli/
# Plus a non-published benchmark crate at benchmarks/mtp-rs-vs-libmtp/.
#
# All checks below run across the whole workspace unless noted otherwise.
#
# Available commands (run `just --list` for details):
#
#   Individual checks:
#     fmt          - Format code with cargo fmt
#     fmt-check    - Check formatting (CI mode)
#     clippy       - Run clippy with -D warnings
#     test         - Run tests (default features)
#     test-all     - Run tests with all features
#     doc          - Build documentation
#     msrv         - Check MSRV (1.85) compatibility
#     audit        - Security audit (requires cargo-audit)
#     deny         - License/dependency check (requires cargo-deny)
#     udeps        - Find unused dependencies (requires nightly + cargo-udeps)
#
#   Composite commands:
#     check        - Run fast checks: fmt-check, clippy, test, doc (default)
#     check-all    - Run all checks including audit and deny
#     fix          - Auto-fix formatting and clippy warnings
#
#   Release:
#     release-dry  - cargo publish --dry-run for both published crates
#
#   Utility commands:
#     clean         - Remove build artifacts
#     install-tools - Install required development tools
#
# MSRV: 1.85

set shell := ["bash", "-uc"]

# Default recipe - run fast checks
default: check

# ==============================================================================
# Individual Checks
# ==============================================================================

# Format code with cargo fmt
fmt:
    @echo "[*] Formatting..."
    @cargo fmt --all
    @echo "[+] Formatted"

# Check formatting without modifying files (for CI)
fmt-check:
    @echo "[*] Checking formatting..."
    @cargo fmt --all --check
    @echo "[+] Formatting OK"

# Run clippy with strict warnings.
# Scoped to `default-members` (lib + CLI) via workspace Cargo.toml. The
# benchmark crate `mtp-bench` depends on libmtp via pkg-config and is
# tested manually with `cargo clippy -p mtp-bench` when libmtp is installed.
clippy:
    @echo "[*] Running clippy..."
    @cargo clippy --all-targets --all-features --quiet -- -D warnings
    @echo "[+] Clippy passed"

# Run tests
#
# The `fs_watcher_*` tests run in their own pass, not in the main parallel pool.
# They wait on real OS filesystem-event delivery, and inside a ~400-test parallel
# run a loaded machine starves them past their poll budget: they fail as a group
# while passing every time on their own. That reads like a watcher bug and isn't
# one, so it costs an afternoon every time someone rediscovers it. Keep them
# separate rather than inflating their timeouts, which only makes the suite
# slower before it still fails.
test:
    @echo "[*] Running tests..."
    @cargo test --quiet -- --skip fs_watcher
    @echo "[*] Running filesystem-watcher tests (own pass, serialized)..."
    @cargo test --quiet fs_watcher -- --test-threads=1
    @echo "[+] Tests passed"

# Run tests with all features enabled (same fs_watcher split as `test`)
test-all:
    @echo "[*] Running tests with all features..."
    @cargo test --all-features --quiet -- --skip fs_watcher
    @echo "[*] Running filesystem-watcher tests (own pass, serialized)..."
    @cargo test --all-features --quiet fs_watcher -- --test-threads=1
    @echo "[+] All feature tests passed"

# Build documentation
doc:
    @echo "[*] Building docs..."
    @cargo doc --no-deps --quiet
    @echo "[+] Docs built"

# Check MSRV compatibility for published crates (lib + CLI). Skips the
# `mtp-bench` benchmark crate, which depends on libmtp-rs and comfy-table
# that require newer Rust. MSRV is a contract for downstream consumers of
# the published crates, not for internal benchmarks.
msrv:
    @echo "[*] Checking MSRV (1.85) compatibility..."
    @if ! rustup run 1.85.0 rustc --version &> /dev/null; then \
        echo "[!] Rust 1.85 not found. Install with: rustup toolchain install 1.85.0"; \
        exit 1; \
    fi
    @RUSTFLAGS="-D warnings" cargo +1.85.0 check --all-features --quiet
    @echo "[+] MSRV check passed"

# Run security audit (requires cargo-audit)
audit:
    @echo "[*] Running security audit..."
    @if ! command -v cargo-audit &> /dev/null; then \
        echo "[!] cargo-audit not found. Install with: just install-tools"; \
        exit 1; \
    fi
    @cargo audit --deny warnings --ignore RUSTSEC-2024-0388 --ignore RUSTSEC-2026-0097
    @echo "[+] Security audit passed"

# Run cargo-deny checks (requires cargo-deny)
deny:
    @echo "[*] Running cargo-deny..."
    @if ! command -v cargo-deny &> /dev/null; then \
        echo "[!] cargo-deny not found. Install with: just install-tools"; \
        exit 1; \
    fi
    @cargo deny --log-level error check
    @echo "[+] Cargo deny passed"

# Find unused dependencies (requires nightly + cargo-udeps)
udeps:
    @echo "[*] Checking for unused dependencies..."
    @if ! command -v cargo-udeps &> /dev/null; then \
        echo "[!] cargo-udeps not found. Install with: just install-tools"; \
        exit 1; \
    fi
    @if ! rustup run nightly rustc --version &> /dev/null; then \
        echo "[!] Nightly toolchain not found. Install with: rustup install nightly"; \
        exit 1; \
    fi
    cargo +nightly udeps --all-features --all-targets
    @echo "[+] No unused dependencies found"

# ==============================================================================
# Composite Commands
# ==============================================================================

# Run fast checks: fmt-check, clippy, test, doc
check: fmt-check clippy test doc
    @echo ""
    @echo "[+] All fast checks passed!"

# Run all checks including slow ones: check + msrv + audit + deny
check-all: check msrv audit deny
    @echo ""
    @echo "[+] All checks passed!"

# Auto-fix formatting and clippy warnings
fix: fmt
    @echo "[*] Running clippy --fix..."
    @cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged --quiet -- -D warnings
    @echo "[+] Fixed"

# ==============================================================================
# Release
# ==============================================================================

# Pre-publish validation for both published crates.
#
# Library: full `cargo publish --dry-run`. Builds the .crate file, verifies
# packaging, and compiles the packaged source. This is the real check.
#
# CLI: `cargo package --list` only, because the CLI depends on the lib via
# `version = "X.Y.Z", path = "../mtp-rs"`. `publish --dry-run` resolves that
# version against crates.io and fails when the new lib version isn't there
# yet. After the lib is actually published, run `cargo publish --dry-run
# -p mtp-rs-cli` separately (or `just release-dry-cli` below) to fully
# verify the CLI before publishing it.
release-dry:
    @echo "[*] Dry-run publishing mtp-rs (library)..."
    @cargo publish --dry-run -p mtp-rs
    @echo ""
    @echo "[*] Listing files mtp-rs-cli would publish (full dry-run needs the lib on crates.io first)..."
    @cargo package -p mtp-rs-cli --list --allow-dirty
    @echo ""
    @echo "[+] Lib dry-run passed and CLI file list looks sane. See docs/releasing.md for the publish flow."

# Full `cargo publish --dry-run` for the CLI. Only works AFTER the lib's
# new version is on crates.io.
release-dry-cli:
    @echo "[*] Dry-run publishing mtp-rs-cli (binary)..."
    @cargo publish --dry-run -p mtp-rs-cli
    @echo "[+] CLI dry-run passed."

# ==============================================================================
# Utility Commands
# ==============================================================================

# Remove build artifacts
clean:
    @echo "[*] Cleaning build artifacts..."
    cargo clean
    @echo "[+] Clean complete"

# Install required development tools
install-tools:
    @echo "[*] Installing development tools..."
    @echo ""
    @echo "Installing cargo-audit..."
    cargo install cargo-audit
    @echo ""
    @echo "Installing cargo-deny..."
    cargo install cargo-deny
    @echo ""
    @echo "Installing cargo-udeps (requires nightly)..."
    rustup install nightly
    cargo install cargo-udeps
    @echo ""
    @echo "[+] All tools installed"
