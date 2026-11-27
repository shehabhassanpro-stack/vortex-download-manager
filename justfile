# Vortex Download Manager — Unified Task Runner (Phase 0 + Phase 1 MVP)
# Requires: just (https://github.com/casey/just)

set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# List available recipes
default:
    @just --list

# Fast type & compile check across Rust workspace (all features) and TypeScript packages
check:
    cargo check --workspace --all-targets --all-features
    pnpm typecheck

# Format all Rust and TypeScript/JSON/CSS files
fmt:
    cargo fmt --all
    pnpm format

# Verify formatting without modifying files (CI gate)
fmt-check:
    cargo fmt --all -- --check
    pnpm format:check

# Run strict Rust Clippy (-D warnings, all features) and ESLint (strict TS + RTL + FSD)
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    pnpm lint

# Run all Rust tests (all features via cargo-nextest) and TypeScript tests (Vitest + axe-core + 10k 60FPS)
test:
    cargo nextest run --workspace --all-features
    pnpm test

# Run Rust test coverage with mandatory Phase 1 thresholds (>= 90% on vortex-error, vortex-domain, vortex-engine)
test-cov:
    cargo llvm-cov clean --workspace
    cargo llvm-cov nextest -p vortex-error --fail-under-lines 90
    cargo llvm-cov nextest -p vortex-domain --fail-under-lines 90
    cargo llvm-cov nextest -p vortex-engine --fail-under-lines 90
    cargo llvm-cov nextest -p vortex-error -p vortex-observability -p vortex-domain -p vortex-application -p vortex-engine -p vortex-ipc --fail-under-lines 75 --lcov --output-path lcov.info

# Run automated MVP performance, memory (< 60 MB), Cold Start (< 300ms), 20GB Sparse (< 50ms) & Test Lab (>= 95%) budgets
check-budgets:
    cargo nextest run -p vortex-bench --test mvp_budgets

# Run automated Playwright E2E suite against real Brave + vortex-bridge.exe + vortex-desktop.exe
e2e-brave:
    pnpm build:ui
    pnpm build:ext
    cargo build --release -p vortex-desktop -p vortex-test-lab
    cargo build --release -p vortex-bridge
    pnpm test:e2e

# Build per-user Windows NSIS & MSI installers (no Admin) and verify < 15 MB size gate
build-installer:
    powershell -NoLogo -ExecutionPolicy Bypass -File scripts/verify-installers.ps1

# Security & license audit (cargo-deny, cargo-audit, pnpm audit)
audit:
    cargo deny check
    cargo audit
    pnpm audit --audit-level=high

# Verify 5-level Crate DAG, target deps, workspace lints & unsafe policy (tools/arch-check)
arch-check:
    cargo run -p vortex-arch-check --locked

# Verify protobuf codegen & run Golden Wire binary (.bin) and JSON compatibility tests
proto-gen:
    cargo nextest run -p vortex-ipc --all-features

# Regenerate tauri-specta TypeScript bindings and verify zero contract drift (`P1.6-T13`)
check-bindings:
    cargo test -p vortex-desktop export_specta_typescript_bindings_to_ui
    git diff --exit-code apps/desktop/ui/src/bindings/generated.ts

# Launch desktop UI preview / Tauri dev shell
dev-desktop:
    pnpm --filter @vortex/desktop-ui dev

# Launch browser extension dev server targeting Brave (Chromium MV3)
dev-ext-brave:
    pnpm --filter @vortex/extension dev:brave

# Run full Phase 1 + Phase 1.6 quality & exit gate locally
ci: fmt-check check check-bindings lint test arch-check audit
