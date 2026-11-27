# Changelog

All notable changes to **Vortex Download Manager** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-27

Initial public Windows x64 milestone completing Phases 0, 1, 1.5, 1.6, and 1.7.

### Added

- **Core Architecture & Observability (Phase 0)**
  - Strict 5-level Hexagonal + DDD workspace (`15` Rust crates + `4` TypeScript packages) enforced by `vortex-arch-check`.
  - Unified `VTX-*` error taxonomy (`vortex-error`), structured JSONL logging, in-memory Flight Recorder, and automatic zero-leakage URL/credential redaction (`vortex-observability`).
  - Programmable hostile HTTP test server (`tools/test-lab`) and performance budget harness (`tools/bench`).
- **Crash-Safe Core Engine, Storage & Browser Bridge (Phase 1)**
  - Sparse positional file sink (`FSCTL_SET_SPARSE`) with `VolumeSpaceManager` logical reservation and zero post-download file assembly.
  - Two-tier crash-safe checkpoint protocol (`FlushFileBuffers` before SQLite WAL commit + atomic `.vortex.meta` sidecar) verified under `100×` abrupt process termination.
  - Windows DPAPI encrypted secret vault, `HKCU` per-user Native Messaging host registration, Mark-of-the-Web (`:Zone.Identifier`) + `IAttachmentExecute` AV integration, and single-instance / graceful OS shutdown handling.
  - Thin `< 1 MiB` Native Messaging bridge (`vortex-bridge.exe`) communicating over Owner-SID Named Pipes with Protobuf v3 framing.
  - Chromium MV3 browser extension (`Brave` / `Chrome` / `Edge`) with `onDeterminingFilename` + `1,400 ms` Probe-Before-Commit seamless browser fallback.
- **Confirmation-First Capture & Desktop Workspace (Phase 1.5)**
  - Mandatory confirmation workflow for browser captures with lossless `512-byte` magic-byte verification, HTML ad-trap detection, disguised executable blocking, ad-storm burst quarantine, and `350 ms` accidental-click guard.
  - Compact standalone Tray confirmation window (`580×520`) and Edge-to-Edge Mica desktop workspace with Arabic RTL / English LTR support, OKLCH dark/light themes, virtualized downloads table, and live 512-bin segment inspector.
- **Host Resilience, Contract Integrity & 5-Layer Security (Phase 1.6)**
  - Staggered slow-start probe guard, non-fatal `403`/`429`/`503` concurrency step-down, and continuous byte-0 stream preservation for single-stream hosts (e.g., Pixeldrain).
  - Automated `tauri-specta` TypeScript contract generation (`generated.ts`) with CI zero-drift verification.
  - Five-layer security hardening (`TH-26` – `TH-33`): SSRF & anti-DNS-rebinding `VerifiedIpConnector`, header/over-delivery guards, origin-pinned DPAPI secrets, NTFS reparse-point TOCTOU defense, RTLO/weaponized file detection, IPC token-bucket rate limiting, and WebView2 navigation locking.
- **Ultra-High-Throughput Engine (Phase 1.7)**
  - Concurrent `FILE_FLAG_OVERLAPPED` positional writes with per-worker `2 MiB` slab coalescing and `32 MiB` byte-weighted semaphore backpressure (`> 99%` reduction in `WriteFile` syscalls).
  - Decoupled `512 KiB` HTTP/1.1 read buffer + native High-BDP `HTTP/2` (`ALPN: h2, http/1.1`, `16/64 MiB` windows, multi-connection H2 pool, `RST_STREAM` cleanup).
  - Multi-IP CDN round-robin striping with 60-second slow-IP quarantine and zero-waste probe stream stop offsets.
  - `SmartHostProfileCache` (decoupled from transient socket drops, 5-minute half-open recovery, instant 0 ms burst for proven hosts), `BBR / Gradient2 + AIMD` dynamic worker scaling (`1..32`), and `End-Game` hedged requests.
  - Live protocol (`HTTP/2` / `HTTP/1.1`) and concurrency mode indicators in `LiveInspectorDrawer`, plus `auto_scale_connections` and `enable_http2` settings.
  - Per-user Windows NSIS (`6.03 MiB`) and WiX MSI (`8.62 MiB`) installers requiring no administrator privileges.
