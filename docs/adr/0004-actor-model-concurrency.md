# ADR-004: نموذج التزامن Actor Model مع Backpressure وBulkhead

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
إدارة عشرات التحميلات المتزامنة مع مئات الاتصالات وكتابات القرص وتحديثات الواجهة عبر أقفال مشتركة (`Arc<Mutex<...>>`) تُنتج Deadlocks وLock Contention وتضخم ذاكرة عند بطء القرص.

## Decision Drivers
- صفر Deadlocks وRace Conditions.
- استهلاك ذاكرة ثابت مهما ارتفعت سرعة الإنترنت (حتى 10 Gbps).
- عزل الأعطال (Bulkhead): سقوط تحميل واحد لا يؤثر على الباقي.

## Considered Options
1. **Actor Model فوق Tokio مع Bounded MPSC Channels + Supervision Tree** — ملكية حصرية للحالة (Single Writer)، ضغط عكسي طبيعي، وعزل كامل للأعطال.
2. **Shared-State Locking (`Arc<RwLock/Mutex>`)** — عرضة للـ Deadlocks والازدحام على الأقفال في المسار الساخن.

## Decision
اعتماد **Actor Model** (`Supervisor`, `SchedulerActor`, `DownloadActor`, `FileWriterActor`, `PersistenceActor`) مع قنوات **Bounded MPSC** وإلغاء منظم بـ `CancellationToken` ونقل Zero-Copy عبر `bytes::Bytes`.

## Consequences
- **إيجابيات:** استقرار فائق، ذاكرة مقيدة رياضياً، كود أسهل للفهم والمراجعة، وعزل تام لأي Panic داخل Actor واحد.
- **سلبيات ومقايضات:** تصميم رسائل بين الـ Actors يتطلب انضباطاً وتحديد سعات القنوات بعناية.
- **التحقق والفرض:** قاعدة `clippy::await_holding_lock = "deny"` واختبارات ضغط الذاكرة في CI.
