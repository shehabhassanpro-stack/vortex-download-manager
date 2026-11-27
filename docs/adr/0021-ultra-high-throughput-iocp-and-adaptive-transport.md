# ADR-021: محرك السرعة القصوى — التجميع المستقل لكل عامل بـ Overlapped IOCP، النقل الهجين HTTP/2 عالي الـ BDP، والتوسع التكيّفي الذكي

- **Status:** Accepted (Implemented & Verified ✅ — Phase 1.7)
- **Date:** 2026-11-27
- **Extends:** ADR-007, ADR-009

## Context

كشف الاختبار الميداني بعد `Phase 1.6` أن الحماية ضد الخوادم المقيدة للتوازي (`pixeldrain.com`) عملت بنجاح `100%`، إلا أن سرعة التحميل على المواقع السريعة الأخرى تراجعت بسبب تفاعل **7 اختناقات** في المسار الساخن (`Hot Path`):
1. تطبيق `hyper::http1::Builder::max_buf_size(64 KiB)` لتطبيق `TH-27` خنق مخزن قراءة جسم البيانات (Body) نفسه وقطّع القراءات إلى شرائح `8–16 KiB`.
2. إدراج `ErrorCode::NetConnectionLost` داخل شرط خفض التوازي الدائم (`Non-Fatal Step-Down`) أدى لتسميم `HostCapabilityCache` وتجميد مواقع سريعة عند اتصال واحد لمدة `30` دقيقة بسبب أي تذبذب شبكة عابر.
3. مشاركة مخزن تجميع واحد (`Single AlignedStagingBuffer`) في `SparseFileSink` عبر `8` عمال يكتبون في إزاحات متباعدة تسبب في تدمير التجميع (`~0% Hit Rate`) وإطلاق `~65,000` استدعاء `spawn_blocking` + `WriteFile` + فحص `WdFilter.sys` لكل `1 GB`، مع قناة كتابة بسعة `~1 MiB` فقط.
4. قفل `shared_map` وتكرار تحديث `BTreeMap` على كل `16 KiB`، والإبقاء على `ALPN: http/1.1` فقط، والاتصال بـ Edge IP واحد فقط، وإهدار بايتات `Range: bytes=0-` بعد تقسيم نطاق `Worker #1`.

## Decision Drivers

- الوصول إلى **`98–100%` من السقف الفيزيائي الحقيقي** للخط والخادم (مستوى سرعة `Steam` و`IDM` وأعلى) على كافة المواقع السريعة والبطيئة والمقيدة.
- خفض استدعاءات القرص وفحص مضاد الفيروسات الفوري (`WdFilter.sys`) بنسبة `> 99%` مع استقرار استهلاك المعالج (`< 5%` عند `1 Gbps`) والذاكرة (`< 150 MB` لـ 10 تحميلات).
- الحفاظ التام على **`100%` من متانة `ADR-008` (Crash-Safe Checkpoint)** و**`100%` من الطبقات الأمنية `TH-26` → `TH-33`** بـ `0%` ضريبة أداء في المسار الساخن، مع عدم كسر حماية الخوادم المقيدة (`Pixeldrain`).

## Considered Options

1. **الحل المتكامل المعتمد (`Per-Worker 2–4 MiB Aligned Coalescing + Concurrent Overlapped IOCP + Decoupled 512 KiB Read Buffer + High-BDP HTTP/2 + BBR/Gradient2 Auto-Scaler + Multi-IP Striping + Lock-Free Hot Path + End-Game Hedging`):**
   - كل عامل يجمّع `2–4 MiB` محاذاة لـ `4 KiB` محلياً ثم يرسلها لكتابة موضعية متوازية عبر `FILE_FLAG_OVERLAPPED` مع Semaphore موزون بالبايتات (`32–64 MiB`).
   - فصل فحص الترويسات (`≤ 64 KiB` / `≤ 100` ترويسة) عن مخزن قراءة البيانات (`256–512 KiB`)، وتفعيل `ALPN: ["h2", "http/1.1"]` مع نوافذ HTTP/2 ضخمة (`16 MiB` للتيار / `64 MiB` للاتصال) واتصالات H2 متعددة.
   - قصر خفض التوازي الصارم على `403`/`429`/`503` فقط (دون `NetConnectionLost`)، وترقية الكاش إلى `SmartHostProfileCache` (`hard_cap` مع `Half-Open Probe` + `proven_optimal_workers`)، وتفعيل توسع ديناميكي `BBR/Gradient2` (`1..32` اتصالاً عبر `WSAIoctl(SIO_TCP_INFO)`), وتوزيع Round-Robin على كل Edge IPs، و`End-Game Hedged Requests` لآخر `2%`.
2. **زيادة عدد الاتصالات الثابت عشوائياً إلى 32 مع الإبقاء على نفس مسار القرص و`hyper`:**
   - سيزيد تنافس الأقفال وتدمير التجميع في القرص ويسبّب Bufferbloat وحظر WAF على المواقع الحساسة.

## Decision

اعتماد **الخيار (1)** بالكامل وتنفيذه في **`Phase 1.7`** وفق المواصفة المرجعية [Ultra-High-Throughput-Engine-Spec.md](../Ultra-High-Throughput-Engine-Spec.md) كمرحلة تسريع جوهرية قبل `Phase 2`.

## Consequences

- **إيجابيات:** قفزة هائلة في سرعة التحميل واستقرارها على جميع أنواع المواقع والخطوط السريعة (100 Mbps – 10 Gbps)، تخفيض `> 99%` في استدعاءات نواة Windows للقرص، قضاء نهائي على تباطؤ `99%`، وحماية كاملة للخوادم المقيدة (`Pixeldrain`) ولطبقات الأمان `TH-26..TH-33`.
- **سلبيات ومقايضات:** ارتفاع طفيف ومضبوط في ذاكرة التحميل النشط (`32–64 MiB` عبر Byte-Weighted Semaphore لكل تحميل نشط — يبقى أقل بكثير من ميزانية `< 150 MB` ولا يمس ذاكرة الخمول `< 45 MB` لأن كل الشرائح تُعاد للـ Pool وتُفرَّغ فور التوقف).
- **التحقق والفرض:** اختبارات آلية في `tools/test-lab` و`tools/bench` تثبت: (أ) متوسط حجم كتابة `≥ 1.5 MiB` و`≤ 64` استدعاء لـ `64 MiB` بـ 8 عمال، (ب) عدم تسمم الكاش عند `NetConnectionLost` عابر، (ج) توسع `BBR/Gradient2` على `/per-conn-cap`، (د) عمل `End-Game Hedged Requests` على `/tail-straggler`، (هـ) عمل `HTTP/2` عالي الـ BDP مع رفض `Header Bomb` (`TH-27`)، (و) عدم تراجع اختبار `Pixeldrain` 1-stream وجميع اختبارات الأمان.
