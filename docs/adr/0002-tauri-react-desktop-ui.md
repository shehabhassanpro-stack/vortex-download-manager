# ADR-002: استخدام Tauri 2 + React 19 + TypeScript لواجهة سطح المكتب

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
نريد واجهة حديثة بجمال Fluent 2 ودعم كامل للعربية (RTL) والثيمين (فاتح/داكن) وسهولة IDM، دون التضحية بخفة البرنامج أو قابلية دعم macOS وLinux لاحقاً.

## Decision Drivers
- حجم المثبّت < 15 MB وذاكرة الخلفية < 60 MB.
- تكامل مباشر وآمن مع المحرك المكتوب بـ Rust.
- دعم قوي للـ RTL وإتاحة الوصول (WCAG 2.2 AA) وسرعة تطوير الواجهة.

## Considered Options
1. **Tauri 2 + React 19 + TypeScript (Strict) + Zero-Runtime OKLCH Design Tokens + `lucide-react`** — يستخدم WebView2 المثبّت في Windows، حجم صغير (`5.82 MiB` لمثبّت NSIS و`8.32 MiB` لمثبّت MSI و`155.16 kB` مضغوط للواجهة)، تكامل Rust أصلي، `tauri-specta` (`#[specta(type = f64)]` في `specta-typescript 0.0.15`) لضمان تطابق الأنواع آلياً، ونموذج أمان Capabilities + حارس `on_navigation` (`TH-33`).
2. **Electron** — يضمّن Chromium وNode كاملين (~100 MB حجم و150+ MB ذاكرة)، يخالف هدف الخفة.
3. **WinUI 3 / Native Win32** — أصلي لـ Windows فقط، تطوير أبطأ، لا يعمل على macOS/Linux.
4. **Qt (C++)** — تعقيد ترخيص وتكامل أصعب مع Rust.

## Decision
اعتماد **Tauri 2** كغلاف مكتبي و**React 19 + TypeScript (Strict)** للواجهة مع توليد الأنواع آلياً عبر `tauri-specta`.

## Consequences
- **إيجابيات:** خفة عالية، أمان بـ CSP وCapabilities، تشارك منظومة TypeScript مع إضافة المتصفح، وقابلية النقل لـ macOS/Linux.
- **سلبيات ومقايضات:** WebView2 يستهلك 50–80 MB أثناء فتح النافذة — نُعالج ذلك بتدمير WebView عند الإغلاق للـ Tray (انظر ADR-005).
- **التحقق والفرض:** ميزانيات الأداء في CI + `tsc --noEmit` + `axe-core`.
