# ADR-018: تجريد الوقت (Clock Trait) وحقن الأعطال (Failpoints)

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
اختبار Exponential Backoff، والجدولة الزمنية، وEWMA، ومحدد السرعة، وانقطاع الكهرباء بين `FlushFileBuffers` و`SQLite COMMIT` باستخدام وقت حقيقي (`sleep`) يؤدي لاختبارات بطيئة وهشّة (Flaky) وغير قادرة على إصابة اللحظات الحرجة بدقة.

## Decision Drivers
- اختبارات حتمية 100% (Zero Flakiness) وسريعة (< 10 ثوانٍ لمئات الاختبارات).
- قدرة مؤكدة على اختبار أكثر حالات الفشل ندرة وخطورة.

## Considered Options
1. **سمة `Clock` Trait (`SystemClock` في الإنتاج / `FakeClock` في الاختبارات) + نقاط Failpoints مشروطة بميزة `#[cfg(feature = "failpoints")]` (صفر تكلفة في الإنتاج).**
2. **استخدام `tokio::time::sleep` واختبارات عشوائية فقط** — بطيئة، متقطعة الفشل، وتفوت حالات السباق الدقيقة.

## Decision
اعتماد `Clock` Trait في `vortex-domain` و`vortex-engine` و`vortex-application`، ونقاط **Failpoints** في `vortex-io` و`vortex-storage` و`vortex-engine`.

## Consequences
- **إيجابيات:** تغطية اختبارية دقيقة جداً، وCI سريع وموثوق، وبرهان فعلي على Crash-Safety.
- **سلبيات ومقايضات:** تمرير `Arc<dyn Clock>` للخدمات الزمنية — نمط بسيط ونظيف تماماً مع Dependency Injection.
- **التحقق والفرض:** حظر `std::thread::sleep` في اختبارات الوحدة + اختبارات Failpoints الإلزامية في CI.
