# Contributing to Vortex Download Manager

Thank you for your interest in contributing to **Vortex**!

Before opening a pull request, please read our governing engineering documents:
- [`docs/Engineering-Standards.md`](./docs/Engineering-Standards.md) — mandatory Rust, TypeScript, Tokio, MV3, contract-codegen, testing, and review rules
- [`docs/Vortex-Architecture-Design.md`](./docs/Vortex-Architecture-Design.md) — system architecture, 5-level crate DAG, and algorithms
- [`docs/Protocol-Spec.md`](./docs/Protocol-Spec.md) — IPC, Native Messaging, and Tauri Specta Channel C v1 contracts
- [`docs/Threat-Model.md`](./docs/Threat-Model.md) — STRIDE security controls (`TH-01`..`TH-33`)
- [`docs/Cybersecurity-Layers-And-Misuse-Prevention-Spec.md`](./docs/Cybersecurity-Layers-And-Misuse-Prevention-Spec.md) — 5-layer defense-in-depth & proactive user-misuse defenses (`UX-M01`..`UX-M12`)
- [`docs/Host-Resilience-And-Contract-Integrity-Spec.md`](./docs/Host-Resilience-And-Contract-Integrity-Spec.md) — paced slow-start concurrency, non-fatal worker step-down, and zero-drift IPC contract gate
- [`docs/Ultra-High-Throughput-Engine-Spec.md`](./docs/Ultra-High-Throughput-Engine-Spec.md) — Per-Worker `2–4 MiB` IOCP coalescing, High-BDP HTTP/2, Multi-IP CDN striping, BBR/Gradient2 auto-scaling, and End-Game hedging (`Phase 1.7`)
- [`docs/Design-System.md`](./docs/Design-System.md) & [`docs/Creative-UI-And-Brand-Overhaul-Spec.md`](./docs/Creative-UI-And-Brand-Overhaul-Spec.md) — OKLCH Calm UI, RTL CSS Logical Properties, and WCAG 2.2 AA rules
- [`docs/adr/README.md`](./docs/adr/README.md) — accepted Architecture Decision Records (`ADR-001`..`ADR-021`)

## Development Workflow

1. **Trunk-Based Development:** Keep feature branches short-lived (< 2 days) off `main`.
2. **Conventional Commits (Mandatory):**
   ```text
   <type>(<scope>): <concise imperative summary>
   ```
   Allowed types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `ci`, `chore`, `security`.
3. **Architecture & Zero-Panic Rules:**
   - Never violate the 5-level crate DAG (`Infrastructure → Application → Domain`).
   - Zero `.unwrap()`, `.expect()`, `panic!()`, or unchecked indexing in production Rust code.
   - Keep `panic = "unwind"` in `[profile.release]` to preserve Actor Bulkhead isolation.
   - Never use `any` or `@ts-ignore` in TypeScript; use CSS Logical Properties only (`ms-*`, `me-*`, `ps-*`, `pe-*`, `start-*`, `end-*`).
4. **Local Quality Gate (run before pushing):**
   ```sh
   just fmt && just check-bindings && just lint && just test && just arch-check && just audit
   ```

## Licensing & Non-Goals

- Core, Desktop, Bridge, CLI, and crates are licensed under **`GPL-3.0-or-later`** (`ADR-013`).
- The thin browser extension (`extension/`) is dual-licensed under **`MPL-2.0 OR GPL-3.0-or-later`**.
- **Strict Non-Goal (`ADR-011`):** Contributions attempting to bypass DRM (Widevine, PlayReady, FairPlay) or CAPTCHA will not be accepted.
- **Zero Telemetry (`ADR-014`):** Analytics, tracking SDKs, or silent telemetry are permanently prohibited.
