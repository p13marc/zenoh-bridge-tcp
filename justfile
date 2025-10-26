# Justfile for zenoh-bridge-tcp
# Run `just --list` to see all available commands

# Default recipe (runs when you just type `just`)
default:
    @just --list

# Build the project
build:
    cargo build

# Build the project in release mode
build-release:
    cargo build --release

# Run the project
run *ARGS:
    cargo run -- {{ARGS}}

# Run tests
test:
    cargo test

# Run tests with output
test-verbose:
    cargo test -- --nocapture

# Run tests including ignored ones
test-all:
    cargo test -- --include-ignored

# Check the project (fast compile check)
check:
    cargo check

# Check all targets and features
check-all:
    cargo check --all-targets --all-features

# Format code
fmt:
    cargo fmt

# Check formatting without applying changes
fmt-check:
    cargo fmt -- --check

# Run clippy linter
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run clippy with pedantic lints
clippy-pedantic:
    cargo clippy --all-targets --all-features -- -W clippy::pedantic

# Run clippy and automatically fix issues
clippy-fix:
    cargo clippy --fix --allow-dirty --allow-staged

# Security audit - check for known vulnerabilities
audit:
    cargo audit

# Check for security advisories and update audit database
audit-update:
    cargo audit --update

# Run cargo-deny to check dependencies for licenses, security, etc.
deny:
    cargo deny check

# Check licenses only
deny-licenses:
    cargo deny check licenses

# Check advisories only
deny-advisories:
    cargo deny check advisories

# Check bans only
deny-bans:
    cargo deny check bans

# Check sources only
deny-sources:
    cargo deny check sources

# Find unused dependencies with cargo-machete
machete:
    cargo machete

# Find unused dependencies with cargo-udeps (requires nightly)
udeps:
    cargo +nightly udeps --all-targets

# Run code coverage with tarpaulin
coverage:
    cargo tarpaulin --out Html --output-dir coverage

# Run code coverage and show in terminal
coverage-terminal:
    cargo tarpaulin --out Stdout

# Run code coverage with detailed report
coverage-verbose:
    cargo tarpaulin --out Html --output-dir coverage --verbose

# Run code coverage and upload to codecov (CI)
coverage-ci:
    cargo tarpaulin --out Xml

# Run all quality checks
quality: fmt-check clippy test audit deny machete
    @echo "✅ All quality checks passed!"

# Run all quality checks (nightly version with udeps)
quality-nightly: fmt-check clippy test audit deny machete udeps
    @echo "✅ All quality checks passed (including nightly tools)!"

# Fix all auto-fixable issues
fix:
    cargo fmt
    cargo clippy --fix --allow-dirty --allow-staged
    @echo "✅ Auto-fixes applied!"

# Clean build artifacts
clean:
    cargo clean

# Clean and rebuild everything
rebuild: clean build

# Install required tools
install-tools:
    @echo "Installing quality tools..."
    cargo install cargo-audit
    cargo install cargo-deny
    cargo install cargo-machete
    cargo install cargo-tarpaulin
    @echo "Note: cargo-udeps requires nightly toolchain"
    @echo "Run: rustup toolchain install nightly"
    @echo "✅ Tools installed!"

# Update dependencies
update:
    cargo update

# Show outdated dependencies
outdated:
    cargo outdated

# Generate documentation
doc:
    cargo doc --no-deps

# Generate and open documentation
doc-open:
    cargo doc --no-deps --open

# Run benchmarks (if any)
bench:
    cargo bench

# Run example
example NAME:
    cargo run --example {{NAME}}

# Watch and run tests on file changes (requires cargo-watch)
watch:
    cargo watch -x test

# Watch and run specific command on file changes
watch-cmd CMD:
    cargo watch -x "{{CMD}}"

# Full CI pipeline simulation
ci: fmt-check clippy test doc
    @echo "✅ CI checks passed!"

# Pre-commit checks (fast version)
pre-commit: fmt clippy test
    @echo "✅ Ready to commit!"

# Release preparation checks
pre-release: quality coverage doc
    @echo "✅ Ready for release!"
