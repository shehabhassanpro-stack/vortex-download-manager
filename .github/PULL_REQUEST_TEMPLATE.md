## Summary

<!-- Describe what this PR changes and why. Reference task IDs (e.g. P0-T09) or ADRs. -->

## Related Tasks / ADRs
- Task ID: `P_-T__`
- ADRs: `ADR-___`

## Engineering Review Checklist (Engineering-Standards.md §5.2)

- [ ] **Architecture & Contracts:** Code resides in the right layer (`Infrastructure → Application → Domain`), respects the 5-level DAG (`just arch-check`), and keeps `tauri-specta` `generated.ts` in zero-drift sync with Rust DTOs.
- [ ] **Correctness & Edge Cases:** Handled 0-byte files, > 4 GiB files (incl. FAT32 guard), concurrency-capped hosts (`403`/`429` step-down), Arabic/Unicode filenames, network drops, disk full, and mid-operation cancellation.
- [ ] **Errors & Observability:** All errors use stable `VTX-*` codes (`vortex-error`), carry `trace_id`, serialize `ApiError` in canonical `camelCase`, zero `.unwrap()`/`.expect()`/`panic!()`, and secrets are wrapped/redacted (`Zero-Leakage` test green).
- [ ] **Concurrency & Performance:** Bounded byte-weighted backpressure, Per-Worker `2–4 MiB` aligned coalescing (zero shared-buffer thrashing or per-`16 KiB` `spawn_blocking`/Mutex), decoupled HTTP header cap vs `256–512 KiB` body read buffer, `Clock` trait used for testable time.
- [ ] **Security (`TH-01`..`TH-33`):** All untrusted inputs validated (Path Traversal, NTFS Symlink/Reparse-Point TOCTOU, RTLO/Magic-Byte spoofing, UNC, Socket-Level SSRF/DNS-Rebinding, WebView2 Navigation Lock, Argument Injection without shell); any `unsafe` block has `// SAFETY:` justification.
- [ ] **UI / RTL / A11y (if applicable):** CSS Logical Properties only, `tabular-nums` for dynamic metrics, `i18next` ICU keys, WCAG 2.2 AA (`axe-core` clean).
- [ ] **Tests & Docs:** Unit / `proptest` / Fault-Injection tests cover success & error paths; `Tasks.md` and `rustdoc`/JSDoc updated; `just check && just lint && just test && just audit` green.
