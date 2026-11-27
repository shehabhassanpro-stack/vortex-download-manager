# ADR-009: التقسيم الهجين بالعقود التكيّفية (Keep-Alive) + End-Game + BBR/Gradient2 Concurrency

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
1. في HTTP/1.1، تقليص استجابة مفتوحة (`Range: bytes=start-`) يجبر العميل على إغلاق مقبس TCP وخسارة **Keep-Alive**.
2. التقسيم الثابت يسبب Tail Latency ("التعليق عند 99%").
3. زيادة عدد الاتصالات بلا وعي بزمن الاستجابة تسبب **Bufferbloat** (ارتفاع الـ Ping بشدة) أو حظر `429`.

## Decision Drivers
- استغلال ≥ 95% من سعة الخط مع الحفاظ 100% على HTTP/1.1 Keep-Alive وTLS Session Resumption.
- القضاء التام على تعليق آخر 1%، وعدم رفع الـ Ping أثناء التصفح والألعاب.

## Considered Options
1. **الخوارزمية المركّبة الهجينة:**
   - **Probe مفتوح (`bytes=0-`)** لاتصال #1، بينما تطلب اتصالات HTTP/1.1 الإضافية **عقوداً محددة الطول تكيّفية ($C \in [1, 32]\text{ MiB}$، مستهدفة $\tau \approx 3\text{ s}$)** فتحافظ على Keep-Alive وتتيح **Work Stealing** مجانياً عند حدود القطع؛ وفي HTTP/2-3 نلغي الذيل بـ `RST_STREAM` / `STOP_SENDING`.
   - **End-Game Hedged Requests** لآخر ≤ 2% عند بطء اتصال عن 30% من الوسيط.
   - **متحكم توازي مستوحى من Google BBR وNetflix Gradient2 + AIMD:** يوازن بين السرعة $\hat{V}$ وتدرّج التأخير $g = \text{RTT}_{\min}/\text{RTT}_{\text{ewma}}$ لمنع Bufferbloat، مع قياس الـ RTT مباشرة من نواة النظام بالميكروثانية عبر **`WSAIoctl(SIO_TCP_INFO)`** (`TCP_INFO_v1` ثم رجوع لـ `v0` + `Windowed Min-Filter`) للاتصالات المباشرة، و`quinn::Connection::rtt()` لـ HTTP/3، و`Windowed Min-Filter` لـ `HTTP/2 PING`/Header RTT عند العمل خلف بروكسي.
2. **تنصيف مفتوح ساذج + عدد اتصالات ثابت** — يهدر مصافحات TCP+TLS ويسبب Bufferbloat أو حظر الخادم.

## Decision
اعتماد **التقسيم الهجين بالعقود التكيّفية (`2–64 MiB`) + Work Stealing + End-Game + BBR/Gradient2 Concurrency**، مع تعزيز جدول الإقلاع في `Phase 1.6` بالخفض السلس غير القاتل للتوازي (`Non-Fatal Concurrency Step-Down`) لحماية الخوادم المقيدة مثل `Pixeldrain` ([Host-Resilience-And-Contract-Integrity-Spec.md](../Host-Resilience-And-Contract-Integrity-Spec.md))، واستكماله في `Phase 1.7` وفق [ADR-021](./0021-ultra-high-throughput-iocp-and-adaptive-transport.md) و[Ultra-High-Throughput-Engine-Spec.md](../Ultra-High-Throughput-Engine-Spec.md) عبر: ① قصر خفض التوازي الدائم على `403`/`429`/`503` فقط دون `NetConnectionLost` العابر، ② ترقية الكاش إلى `SmartHostProfileCache` (`Half-Open Probe` + `proven_optimal_workers`)، ③ تفاوض `ALPN: ["h2", "http/1.1"]` بنوافذ H2 ضخمة (`16/64 MiB`) + `Multi-IP CDN Striping`، ④ إطلاق `BBR/Gradient2 Auto-Scaler (1..32)` و`End-Game Hedged Requests` و`Lock-Free AtomicU64 Cursors`.

## Consequences
- **إيجابيات:** سرعة قصوى، صفر مصافحات TLS مهدرة، ping منخفض ومستقر، وتفوق علمي وهندسي واضح على IDM وaria2.
- **سلبيات ومقايضات:** يتطلب تتبع $\text{RTT}_{\min}$ وEWMA بدقة (مغطى بوحدة `Dual-Horizon Continuous EWMA`).
- **التحقق والفرض:** اختبارات `proptest` + Benchmarks على `tools/test-lab` للتحقق من إعادة استخدام اتصالات Keep-Alive وعدم تضخم RTT.
