# 📜 سجل القرارات المعمارية — Vortex Architecture Decision Records (ADRs)

كل قرار معماري جوهري في مشروع **Vortex Download Manager** موثّق هنا وفق منهجية **MADR / Michael Nygard** المعتمدة في كبرى شركات التقنية (Google، Microsoft، Uber، Spotify).

> **قاعدة الحوكمة:** القرارات المعتمدة (`Accepted`) **ملزمة** لكل كود يُكتب في المشروع. تغيير أي قرار يتطلب إنشاء ADR جديد يحمل حالة `Supersedes ADR-XXX` ومبررات هندسية موثّقة.

---

## الفهرس الرسمي

| # | القرار | الحالة | الملف |
|---|---|---|---|
| ADR-001 | استخدام **Rust (Edition 2024)** للمحرك وكل المكونات الأصلية | ✅ Accepted | [0001-rust-core-engine.md](./0001-rust-core-engine.md) |
| ADR-002 | استخدام **Tauri 2 + React 19 + TypeScript** للواجهة بدلاً من Electron/WinUI | ✅ Accepted | [0002-tauri-react-desktop-ui.md](./0002-tauri-react-desktop-ui.md) |
| ADR-003 | معمارية **Hexagonal + Layered + DDD** مع فرض قاعدة الاعتمادية آلياً | ✅ Accepted | [0003-hexagonal-layered-ddd.md](./0003-hexagonal-layered-ddd.md) |
| ADR-004 | نموذج **Actor** مع قنوات محدودة (Backpressure) وبدون حالة مشتركة | ✅ Accepted | [0004-actor-model-concurrency.md](./0004-actor-model-concurrency.md) |
| ADR-005 | فصل **Core / UI / Bridge** في عمليات مستقلة وتدمير WebView عند الإغلاق | ✅ Accepted | [0005-process-isolation-webview-lifecycle.md](./0005-process-isolation-webview-lifecycle.md) |
| ADR-006 | **Native Messaging + Named Pipes (ACL)** ومنع فتح أي منفذ TCP محلي | ✅ Accepted | [0006-native-messaging-named-pipes.md](./0006-native-messaging-named-pipes.md) |
| ADR-007 | **الكتابة الموضعية المتناثرة (`FSCTL_SET_SPARSE`)** مع حجز المساحة المنطقي وإلغاء التجميع | ✅ Accepted | [0007-positional-writes-no-assembling.md](./0007-positional-writes-no-assembling.md) |
| ADR-008 | **SQLite WAL** ونموذج المتانة ثنائي المستوى (Checkpoint كل 20s) | ✅ Accepted | [0008-sqlite-wal-crash-safe-checkpoint.md](./0008-sqlite-wal-crash-safe-checkpoint.md) |
| ADR-009 | **التقسيم الهجين بالعقود التكيّفية (Keep-Alive) + End-Game + BBR/Gradient2** | ✅ Accepted | [0009-dynamic-segmentation-endgame-aimd.md](./0009-dynamic-segmentation-endgame-aimd.md) |
| ADR-010 | **Protobuf v3** لبروتوكول IPC الداخلي مع توافق `N / N-1 / N-2` | ✅ Accepted | [0010-protobuf-ipc-contract-first.md](./0010-protobuf-ipc-contract-first.md) |
| ADR-011 | **عدم دعم كسر حماية DRM أو تجاوز CAPTCHA** نهائياً | ✅ Accepted | [0011-no-drm-bypass.md](./0011-no-drm-bypass.md) |
| ADR-012 | **WebAssembly Component Model (`wasmtime`)** لنظام الإضافات المعزول | ✅ Accepted | [0012-wasm-plugin-sandbox.md](./0012-wasm-plugin-sandbox.md) |
| ADR-013 | ترخيص **`GPL-3.0-or-later`** مع توافق الإضافة (`MPL-2.0 OR GPL-3.0-or-later`) | ✅ Accepted | [0013-gpl3-license.md](./0013-gpl3-license.md) |
| ADR-014 | **صفر Telemetry افتراضياً** وخصوصية كاملة بالتصميم | ✅ Accepted | [0014-zero-telemetry-privacy.md](./0014-zero-telemetry-privacy.md) |
| ADR-015 | **تصنيف الأخطاء الموحّد (`VTX-*`) + Zero-Panic + React Error Boundaries** | ✅ Accepted | [0015-error-taxonomy-zero-panic.md](./0015-error-taxonomy-zero-panic.md) |
| ADR-016 | **التتبع الموزّع (`trace_id`) + JSONL + Flight Recorder + Zero-Leakage Redaction** | ✅ Accepted | [0016-observability-flight-recorder-redaction.md](./0016-observability-flight-recorder-redaction.md) |
| ADR-017 | **دورة الحياة والتعافي الذاتي** (Single-Instance، `WM_QUERYENDSESSION`، Config الذري) | ✅ Accepted | [0017-lifecycle-graceful-shutdown-self-healing.md](./0017-lifecycle-graceful-shutdown-self-healing.md) |
| ADR-018 | **تجريد الوقت (`Clock` Trait) وحقن الأعطال (Failpoints)** لحتمية الاختبارات | ✅ Accepted | [0018-deterministic-clock-failpoints.md](./0018-deterministic-clock-failpoints.md) |
| ADR-019 | **الحماية من SSRF / UNC NTLM Leak وخصوصية مراقبة الحافظة** | ✅ Accepted | [0019-ssrf-unc-clipboard-privacy.md](./0019-ssrf-unc-clipboard-privacy.md) |
| ADR-020 | **محرك YouTube والوسائط الهجين (v1.0 First-Class Media Pipeline)** | ✅ Accepted | [0020-youtube-hybrid-media-engine.md](./0020-youtube-hybrid-media-engine.md) |
| ADR-021 | **محرك السرعة القصوى (`Per-Worker IOCP` + `High-BDP HTTP/2` + `BBR/Gradient2` + `Multi-IP`)** | ✅ Accepted | [0021-ultra-high-throughput-iocp-and-adaptive-transport.md](./0021-ultra-high-throughput-iocp-and-adaptive-transport.md) |

---

## قالب إنشاء ADR جديد (`template.md`)

```markdown
# ADR-XXX: <عنوان مختصر وحاسم>
- **Status:** Proposed | Accepted | Superseded by ADR-YYY
- **Date:** YYYY-MM-DD

## Context
ما المشكلة والقيود التقنية والتشغيلية؟

## Decision Drivers
العوامل الحاسمة (الأداء، الأمان، البساطة، الصيانة...).

## Considered Options
1. الخيار أ — المميزات / العيوب
2. الخيار ب — المميزات / العيوب

## Decision
القرار المعتمد بوضوح.

## Consequences
- **إيجابيات:** ...
- **سلبيات ومقايضات:** ...
- **التحقق والفرض:** كيف يُفرض هذا القرار في الكود أو CI؟
```
