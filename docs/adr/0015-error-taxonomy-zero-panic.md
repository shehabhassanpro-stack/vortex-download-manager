# ADR-015: تصنيف الأخطاء الموحّد (VTX-*) + Zero-Panic + React Error Boundaries

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
معالجة الأخطاء العشوائية (نصوص حرة، `.unwrap()`، ابتلاع أخطاء، أو تعطل شاشة React بيضاء) هي السبب الأول لتدني موثوقية التطبيقات المكتبية وصعوبة دعم المستخدمين.

## Decision Drivers
- Crash-free sessions ≥ 99.9%.
- رسائل واضحة ومترجمة وقابلة للتصرف للمستخدم + تفاصيل تقنية دقيقة للمطور.
- عزل الأعطال بحيث لا يُسقط خطأ في تحميل واحد أو لوحة واجهة التطبيق بالكامل.

## Considered Options
1. **وحدة `vortex-error` بأكواد ثابتة `VTX-*` + `ApiError` عبر الحدود + فرض Clippy Zero-Panic (`deny(unwrap_used, expect_used, panic, indexing_slicing)`) + عزل Actors + React Error Boundaries + Fallback آمن في الإضافة.**
2. **استخدام `anyhow::Error` كسلسلة نصية في كل مكان** — يفقد البنية والتصنيف والترجمة الآلية والإجراءات المقترحة.

## Decision
اعتماد معمارية الأخطاء الصارمة الموصوفة في §23 من [Vortex-Architecture-Design.md](../Vortex-Architecture-Design.md) و[Engineering-Standards.md](../Engineering-Standards.md).

## Consequences
- **إيجابيات:** موثوقية بمستوى المؤسسات، تشخيص فوري، تجربة مستخدم سلسة حتى عند الأعطال.
- **سلبيات ومقايضات:** يتطلب انضباطاً إضافياً في معالجة كل `Result`/`Option` — يفرضه المترجم تلقائياً بدون مجهود ذهني لاحق.
- **التحقق والفرض:** `[workspace.lints.clippy]` في CI + اختبارات مسارات الخطأ لكل كود `VTX-*` + **توحيد تسلسل `ApiError` بصيغة `camelCase` وبوابة توليد `tauri-specta` (`#[specta(type = f64)]` في `specta-typescript 0.0.15`) المفحوصة ضد الانحراف في CI (`Phase 1.6`)** لمنع أي اختلاف بين عقود Rust وTypeScript.
