# ADR-012: WebAssembly Component Model (wasmtime) لنظام الإضافات

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
نريد تمكين المجتمع من توسيع Vortex (دعم مواقع، معالجات ما بعد التحميل، إشعارات مخصصة) دون تعريض جهاز المستخدم لثغرات تنفيذ كود عشوائي (RCE) كما يحدث مع إضافات DLL أو سكربتات Python غير المعزولة.

## Decision Drivers
- عزل أمني حقيقي (Sandboxing) وصلاحيات صريحة (Capability-based).
- تعدد لغات التطوير للمجتمع (Rust, JS, Go, Python).
- حماية الأداء (تقييد CPU والذاكرة والوقت لكل Plugin).

## Considered Options
1. **WASM Component Model عبر `wasmtime` + واجهات WIT + صلاحيات Capability-based + Fuel/Memory Limits.**
2. **مكتبات ديناميكية (DLL / SO)** — خطيرة جداً أمنياً، غير متوافقة بين الأنظمة، وعطلها يسقط التطبيق كله.
3. **Lua / QuickJS مضمّن** — أضعف عزلاً أو محصور بلغة واحدة.

## Decision
اعتماد **WebAssembly Component Model (`wasmtime`)** كمنصة وحيدة للإضافات الداخلية، مع استثناء أدوات الوسائط (`yt-dlp` و`FFmpeg` المصغّر) التي تُدار كعمليات جانبية منفصلة عند الطلب (**Managed On-Demand Sidecars**) بتوثيق مزدوج (`Ed25519` + `SHA-256`) وفق [ADR-020](./0020-youtube-hybrid-media-engine.md).

## Consequences
- **إيجابيات:** أمان على مستوى المتصفح، استقرار تام، ومجتمع واسع متعدد اللغات.
- **سلبيات ومقايضات:** زيادة بسيطة في حجم الـ Binary (~3–5 MB) — تُعالج بإدراجه في Phase 3 بعد نضج المحرك.
- **التحقق والفرض:** اختبار Plugin خبيث يثبت حجب الوصول غير المصرح به في CI (Phase 3).
