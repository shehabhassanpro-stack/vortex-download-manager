# Security Policy

## Supported Versions

Vortex Download Manager provides security updates for the latest `Stable` release channel and active `Beta` releases.

## Reporting a Vulnerability

**Please do NOT report security vulnerabilities through public GitHub issues.**

Instead, please use **GitHub Private Vulnerability Reporting** on this repository (Security → Advisories → Report a vulnerability).

Include as much of the following as possible:
- Affected component (`vortex-net`, `vortex-io`, `vortex-ipc`, `vortex-bridge`, `extension`, `desktop`, etc.)
- Vortex version, OS version, and browser version (if applicable)
- Reproduction steps or proof-of-concept (please scrub any real cookies, tokens, or credentials)
- Expected vs. actual security impact

## Response SLAs (per Threat-Model.md §5)

| Milestone | Target SLA |
|---|---|
| Initial acknowledgement | **≤ 48 hours** |
| Severity triage (CVSS) & remediation plan | **≤ 5 days** |
| Signed security update for Critical vulnerabilities | **≤ 7 days** |

## Scope & Hard Non-Goals

- Reference our full Microsoft SDL STRIDE matrix in [`docs/Threat-Model.md`](./docs/Threat-Model.md) (`TH-01` through `TH-33`) as well as our Phase 0–1 Defense-in-Depth architecture in [`docs/Cybersecurity-Layers-And-Misuse-Prevention-Spec.md`](./docs/Cybersecurity-Layers-And-Misuse-Prevention-Spec.md) and anti-deception capture controls in [`docs/Smart-Capture-And-Anti-Abuse-Spec.md`](./docs/Smart-Capture-And-Anti-Abuse-Spec.md).
- **DRM & CAPTCHA Policy (`ADR-011`):** Vortex explicitly and permanently prohibits DRM circumvention or CAPTCHA bypasses. Pull requests or reports requesting DRM bypass functionality will be rejected.
- **Privacy Guarantee (`ADR-014`):** Zero telemetry by default. Any unauthorized outbound network call or secret leakage into logs/diagnostics is treated as a high-severity security bug.
