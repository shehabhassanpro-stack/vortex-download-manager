# 📐 Vortex Download Manager — المعايير الهندسية ودستور الكود (Engineering Standards)

> **الإصدار:** `1.2 (Synchronized with Phase 1.6 Verified & Phase 1.7 Ultra-High-Throughput Standards)` — **النطاق:** جميع الـ Crates والتطبيقات والإضافات في مستودع Vortex.
> **المرجعية:** مستوحاة من [Google Engineering Practices](https://google.github.io/eng-practices/)، ومبادئ Microsoft SDL، ومعايير مجتمع Rust الحديث (Edition 2024).
> **قاعدة ملزمة:** أي كود يُكتب في هذا المشروع — سواء بواسطة مطوّر بشري أو وكيل ذكاء اصطناعي — **يجب** أن يلتزم حرفياً بكل قاعدة في هذه الوثيقة.

---

## 1. القواعد المعمارية الصارمة (Architecture Invariants)

1. **قاعدة الاعتمادية (Dependency Rule — Hexagonal / Clean):**
   - `vortex-domain` ← **صفر اعتماديات خارجية على I/O** (ممنوع `tokio`, `hyper`, `rusqlite`, `std::fs`, `std::net`). يعتمد فقط على `std` و`vortex-error` و`serde` (اختيارياً) وهياكل بيانات نقية.
   - `vortex-application` ← يعتمد فقط على `vortex-domain` و`vortex-error` و`vortex-observability`. يعرّف الـ **Ports (Traits)** ولا يعرف تفاصيل أي Adapter.
   - `vortex-net` / `vortex-storage` / `vortex-io` / `vortex-platform` / `vortex-ipc` (ولاحقاً `vortex-proto-ftp` / `vortex-media` / `vortex-plugin-host`) ← تنفّذ الـ Ports؛ ممنوع احتواؤها على منطق أعمال (Business Logic).
   - **Composition Root واحد فقط:** الربط بين الـ Ports والـ Adapters يتم حصرياً في `apps/desktop/src-tauri/src/bootstrap.rs` (يُستدعى من `main.rs`، وكذلك في `apps/cli` و`apps/bridge`).
   - يتم فرض هذه القواعد تلقائياً في CI عبر `tools/arch-check`.

2. **الحدود المعمارية في الواجهة (Feature-Sliced Design):**
   - `shared/` لا يستورد من `features/` أو `app/`.
   - كل `feature/` معزول ولا يستورد إلا من `shared/` أو الواجهة العامة (`index.ts`) لـ `feature` آخر عند الضرورة القصوى.
   - ممنوع استدعاء `invoke(...)` الخام في مكونات React — كل تواصل مع Rust يمر عبر الدوال المولّدة آلياً في `src/bindings/` (`tauri-specta`).

---

## 2. معايير كود Rust (Edition 2024)

### 2.1 قواعد الـ Lints على مستوى الـ Workspace (إلزامية في `Cargo.toml`)

```toml
[workspace.lints.rust]
unsafe_code = "warn"
missing_docs = "warn"
rust_2024_compatibility = { level = "deny", priority = -1 }

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "deny"
todo = "deny"
unimplemented = "deny"
dbg_macro = "deny"
print_stdout = "deny"
print_stderr = "deny"
await_holding_lock = "deny"
large_futures = "warn"
```

> **الاستثناء الوحيد:** داخل وحدات الاختبار (`#[cfg(test)]`) يُسمح بـ `unwrap`/`expect`/`panic` في الـ Assertions فقط.

### 2.2 سياسة `unsafe`
- ممنوع استخدام `unsafe` في `vortex-domain` و`vortex-application` و`vortex-engine` و`vortex-ipc` و`vortex-media` و`vortex-proto-ftp`.
- يُسمح به فقط عند الضرورة القصوى في `vortex-platform` و`vortex-io` (مثلاً استدعاء Win32 APIs عبر `windows` crate)، وبشرط:
  1. تغليفه في أصغر دالة آمنة ممكنة.
  2. وجود تعليق `// SAFETY: <تفسير دقيق لماذا تتحقق كل الشروط المسبقة>`.
  3. مراجعة خاصة (Security Review) واختبار مباشر لها.

### 2.3 قواعد التزامن والأداء في Tokio (Async Discipline)

| ✅ افعل (DO) | ❌ لا تفعل (DON'T) |
|---|---|
| استخدم **Actors + Bounded MPSC Channels** (لا حالة مشتركة قابلة للتعديل). | لا تستخدم `Arc<Mutex<T>>` لحالة التحميل المشتركة. |
| أرسل العمليات الثقيلة (`BLAKE3`/`SHA-256`، `rusqlite`، `FlushFileBuffers`، فك الضغط) إلى `spawn_blocking` أو Rayon. | لا تنفذ أي عملية متزامنة تستغرق > 100µs على خيوط Tokio Worker. |
| استخدم `tokio_util::sync::CancellationToken` لكل Actor وWorker لضمان إلغاء منظم. | لا تترك `tokio::spawn` يتيمة (Detached) بدون إشراف `Supervisor` أو `JoinSet`. |
| تأكد من **Cancel-Safety** عند استخدام `tokio::select!` (خاصة مع القراءة من الشبكة والقنوات). | لا تحتفظ بأي `std::sync::MutexGuard` عبر نقطة `.await` (ممنوع بـ Clippy). |
| مرّر `bytes::Bytes` من الشبكة إلى كاتب القرص بدون نسخ (Zero-Copy) من Slab Pool محاذى لـ `4 KiB`. | لا تنسخ الـ Buffers إلى `Vec<u8>` جديد في المسار الساخن. |
| استخدم **Per-Worker `2–4 MiB` 4KiB-Aligned Coalescing Slabs** + **Byte-Weighted Backpressure (`32–64 MiB`)** + كتابة موضعية متوازية (`FILE_FLAG_OVERLAPPED` IOCP) و**Lock-Free `AtomicU64` cursors** في المسار الساخن (`ADR-021`). | **ممنوع** مشاركة مخزن تجميع واحد بين عدة عمال متوازين (تدمير التجميع)، أو استدعاء `spawn_blocking` / قفل `Mutex` عام لكل قطعة `16 KiB`! |
| افصل حد ترويسات HTTP (`≤ 64 KiB` / `≤ 100` ترويسة — `TH-27`) عن مخزن قراءة جسم البيانات (`256–512 KiB`)، واضبط نوافذ HTTP/2 العالية للـ BDP (`16 MiB` للتيار / `64 MiB` للاتصال + `adaptive_window`). | **ممنوع** استخدام `http1::Builder::max_buf_size(64 * 1024)` لخنق قراءة الـ Body أو ترك نافذة HTTP/2 عند الافتراضي `64 KiB`! |
| استخدم **`mimalloc`** كمخصّص ذاكرة عام، وحافظ على **`panic = "unwind"`** في `[profile.release]` لضمان عمل عزل الـ Actors (`JoinError::is_panic`). | لا تضع `panic = "abort"` في الـ Core لأنه يبطل عزل الـ Bulkhead ويسقط كل التحميلات معاً! |
| على Windows NTFS/ReFS: فعّل **`FSCTL_SET_SPARSE`** قبل `FileEndOfFileInfo`، وافتح ملفات `.vortex` مع التحقق من عدم كونها روابط رمزية/نقاط وصل (**`FILE_FLAG_OPEN_REPARSE_POINT`** + `GetFinalPathNameByHandleW` — `TH-29`)، واجعل إلغاء العلم `SetSparse = FALSE` عند 100% **غير قاتل (`WARN` فقط)**، ونفّذ `FlushFileBuffers` وفق دورة 20s (وليس مع كل قطعة). | لا تستدعِ `FileEndOfFileInfo` لملف ضخم بدون Sparse (فخ Zero-Fill)، ولا تستخدم `MOVEFILE_REPLACE_EXISTING` بدون موافقة صريحة، ولا تضبط `SO_RCVBUF` يدوياً على Windows (يعطّل TCP Auto-Tuning). |
| قِس الـ RTT للـ BBR/Gradient2 من النواة عبر **`WSAIoctl(SIO_TCP_INFO)`** (`v1` ثم رجوع لـ `v0`) للاتصال المباشر، ومن `quinn::Connection::rtt()` لـ HTTP/3، ومن `Windowed Min-Filter` عند وجود بروكسي. | لا تعتمد على HTTP TTFB الخام كمقياس لـ Bufferbloat الشبكة (لأنه يتلوث بزمن معالجة الخادم). |
| عند تشغيل عمليات جانبية (`yt-dlp`, `ffmpeg`): استدعِ `Command::new(bin)` مباشرة، تحقّق من SHA-256 للـ Binary، ومرّر `--no-exec --no-config-locations --` قبل أي رابط! | **ممنوع منعاً باتاً** تمرير روابط أو أسماء ملفات عبر `cmd.exe /c` أو `powershell` (منع Command/Argument Injection). |
| استخدم `Clock` Trait لكل `sleep`/`now` في الـ Domain والـ Backoff والجدولة. | لا تستدعي `Instant::now()` أو `tokio::time::sleep()` مباشرة داخل منطق قابل للاختبار الزمني. |

### 2.4 معالجة الأخطاء (Error Handling Playbook)

- **في الـ Crates الداخلية (`vortex-*`):** أخطاء مكتوبة الأنواع بـ `thiserror` مع دالة تحويل إلى `VortexError` تحمل `ErrorCode` (`VTX-*`) و`Recoverability`.
- **في الـ Binaries (`apps/*`) فقط:** يُسمح بـ `anyhow` في `main` أثناء الإقلاع قبل تهيئة الحدود.
- **ممنوع ابتلاع الأخطاء بصمت:** كتابة `let _ = ...` على `Result` ممنوعة إلا مع تعليق يشرح السبب (مثلاً إرسال إشعار على قناة أُغلقت بالفعل أثناء الإيقاف).

```rust
// ❌ خطأ: panic محتمل + لا كود + لا سياق + يسرب الرابط الكامل
let size: u64 = header.to_str().unwrap().parse().unwrap();
tracing::error!("failed downloading {}", req.url);

// ✅ صحيح: بدون panic + كود ثابت + رابط منظف + سياق منظم
let size = header
    .to_str()
    .map_err(|e| NetError::MalformedHeader { header: "content-length", source: e })?
    .parse::<u64>()
    .map_err(|_| NetError::InvalidContentLength)?;

tracing::warn!(
    error_code = %ErrorCode::NetMalformedResponse,
    url = %RedactedUrl::from(&req.url),
    "server returned invalid Content-Length; falling back to unknown-size stream"
);
```

### 2.5 السجلات والأسرار (Observability & Redaction Playbook)

- استخدم `#[tracing::instrument(skip(self, cookies, auth, bytes), fields(download_id = %id))]` على العمليات الرئيسية.
- **قاعدة الأسرار:** كل Cookie أو Header مصادقة أو كلمة مرور بروكسي تُخزّن في `secrecy::SecretString` (تُصفَّر تلقائياً بـ `zeroize`). ممنوع طباعتها حتى على مستوى `TRACE`.
- **قاعدة المسار الساخن (Hot-Path Hygiene):** ممنوع إصدار `INFO`/`DEBUG` داخل حلقة قراءة الحزم (`chunk read`) أو كتابة القرص. استخدم عدادات `AtomicU64` وسجّل ملخصاً دورياً فقط.
- **الالتزام بمستويات السجل:**
  - `error!`: فشل يؤثر على المستخدم أو حالة داخلية غير متوقعة (يُشغّل تفريغ Flight Recorder للحرج منها).
  - `warn!`: فشل عابر قابل للاستعادة (`429`, timeout, `ERROR_SHARING_VIOLATION`, انخفاض المساحة).
  - `info!`: معالم دورة الحياة (بدء، إضافة، اكتمال، إيقاف، تغيير إعدادات).
  - `debug!`: قرارات المحرك (تقسيم جزء، AIMD، Circuit Breaker).
  - `trace!`: إطارات بروتوكولية منظفة من الأسرار.

---

## 3. معايير TypeScript وReact 19 والإضافة (Frontend & Extension)

### 3.1 قواعد TypeScript الصارمة
- `strict: true`, `noUncheckedIndexedAccess: true`, `exactOptionalPropertyTypes: true`, `noImplicitOverride: true`.
- **ممنوع `any`** (استخدم `unknown` مع التحقق بـ Type Guards أو `zod` عند حدود الإدخال).
- **ممنوع `@ts-ignore`** (عند الضرورة القصوى استخدم `@ts-expect-error` مع سبب موثق).
- **بوابة تكامل عقود `tauri-specta` إلزامية 100%:**
  - كل أنواع الـ IPC بين React وRust تُولَّد آلياً في `apps/desktop/ui/src/bindings/generated.ts` عبر `tauri-specta` (`specta-typescript 0.0.15` مع تعليقات `#[cfg_attr(feature = "specta", specta(type = f64))]` لحقول `u64`/`i64`).
  - **ممنوع منعاً باتاً** ابتلاع خطأ التصدير بـ `let _ = builder.export(...)` أو كتابة أنواع بديلة يدوياً تخالف هياكل Rust؛ يجب أن يفشل CI فوراً (`git diff --exit-code`) عند أي انحراف في `generated.ts`، مع وجود مُطبّع دفاعي (`normalizeApiError`) عند حدود `client.ts` يضمن عدم انهيار الواجهة أبداً.

### 3.2 قواعد React والأداء
- **Error Boundaries إلزامية:** `RootErrorBoundary` على التطبيق كله + `FeatureErrorBoundary` حول كل لوحة رئيسية، وكل خطأ ملتقط يُرسل فوراً للـ Core عبر `log_frontend_event`.
- **Zustand Selectors ذرية:** ممنوع اشتراك المكوّن في الـ Store كاملاً؛ استخدم Selectors محددة أو `useShallow` لمنع إعادة الرسم عند تحديثات 10Hz.
- **القوائم > 50 عنصراً:** إلزامي استخدام `TanStack Virtual` (لا رسم DOM لآلاف الصفوف).
- **الرسوم اللحظية (شريط الأجزاء + منحنى السرعة):** تُرسم على `<canvas>` مع `requestAnimationFrame` و`devicePixelRatio` — ممنوع تحديث مئات عناصر DOM في الثانية.
- **الأرقام المتغيرة (السرعة، النسبة، المتبقي):** إلزامي `tabular-nums` لمنع اهتزاز الأعمدة بصرياً.

### 3.3 قواعد الـ RTL والتدويل وإتاحة الوصول (i18n & A11y)
- **CSS Logical Properties إلزامية 100% (في ملفات CSS القياسية وDesign Tokens):**
  - ✅ استخدم: `margin-inline-start`, `margin-inline-end`, `padding-inline-start`, `padding-inline-end`, `inset-inline-start`, `inset-inline-end`, `border-inline-start`, `border-start-start-radius`, `text-align: start`.
  - ❌ ممنوع: `margin-left`, `margin-right`, `padding-left`, `padding-right`, `left`, `right`, `text-align: left`, `text-align: right` (يُفرض بقاعدة فحص آلي وESLint).
- **عناصر تبقى LTR دائماً حتى في الواجهة العربية:** الروابط (URLs)، مسارات الملفات، أكواد الأخطاء (`VTX-*`)، الهاشات (`SHA-256`)، و`trace_id` — تُغلّف بـ `dir="ltr" className="unicode-bidi-isolate"`.
- **لا نصوص ثابتة (Hardcoded Strings):** كل نص مرئي يمر عبر `i18next` مع ICU MessageFormat لصيغ الجمع العربية الست.
- **WCAG 2.2 AA:** كل زر أيقوني يحمل `aria-label`، التركيز واضح بـ `focus-visible`، والتنقل الكامل متاح بلوحة المفاتيح.

### 3.4 قواعد إضافة المتصفح (MV3 Hygiene)
- لا تعتمد على متغيرات الذاكرة في Service Worker للبقاء بعد الخمول — احفظ الحالة في `chrome.storage.session`.
- اربط الالتقاط الأساسي بـ **`chrome.downloads.onDeterminingFilename`** (وليس `onCreated` وحده) لضمان توفر `finalUrl` و`filename` و`fileSize` بعد انتهاء التحويلات.
- ادمج دائماً الكوكيز العادية مع **الكوكيز المقسّمة (CHIPS)** عبر تمرير `partitionKey: { topLevelSite }` للتبويب الأصلي.
- **القاعدة الذهبية (Live-Stream Hold بدون `pause()`):** في Chromium لا تستدعِ `downloads.pause()` أثناء الـ Probe (لأنه يغلق مقبس TCP ويحرق التوكن أحادي الاستخدام)؛ بل أرجع `true` من `onDeterminingFilename` وأخّر `suggest()` داخل `SafeSuggestGuard` (`≤ 2.0s`). عند `accepted = true` نفّذ `cancel + erase`، وعند أي Timeout أو `fallback_to_browser = true` (أو اكتمال ≥ 80% من ملف صغير) استدعِ `suggest()` ليكمل المتصفح على نفس الاتصال المفتوح! (وعلى Firefox MV3 استخدم `webRequest.onHeadersReceived` الحاجب بنفس المنطق).
- كل عنصر يُحقن في صفحة الويب (مثل كبسولة YouTube/الفيديو العائمة) يجب عزله تماماً داخل **Closed Shadow DOM** مع التحقق من `event.isTrusted` وإخفائه عند ملء الشاشة أو السكون.

---

## 4. معايير الاختبار (Testing Standards)

1. **نمط الاختبار:** **Arrange – Act – Assert (AAA)** بأسماء تصف السلوك: `resume_fails_with_resource_changed_when_etag_differs`.
2. **حتمية 100% (Zero Flakiness):** ممنوع `thread::sleep` أو الاعتماد على الإنترنت الحقيقي في اختبارات Unit/Integration. استخدم `FakeClock` و`tools/test-lab` المحلي.
3. **الخصائص الرياضية (`proptest`):** إلزامية لـ `RangeSet`، `SegmentationPolicy`، `FilenameSanitizer`، `RedactedUrl`، و`DomainTrie`.
4. **اختبار مسارات الخطأ وحقن الأعطال:** كل كود خطأ في `VTX-*` يجب أن يكون له اختبار واحد على الأقل يثبت إنتاجه ومعالجته.
5. **العتبات الإلزامية:**
   - تغطية `vortex-domain`, `vortex-engine`, `vortex-error` ≥ **90%**.
   - تغطية باقي الـ Crates ≥ **75%**.
   - اختبار Zero-Leakage للأسرار يمر في كل PR.

---

## 5. Git، الفروع، ومراجعة الكود (Google Code Review Rubric)

### 5.1 Trunk-Based Development + Conventional Commits
- فرع `main` محمي وقابل للإصدار دائماً؛ فروع العمل قصيرة العمر (< 2 يوم).
- صيغة الـ Commit الإلزامية:
  `<type>(<scope>): <concise imperative summary>`
  الأنواع: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `ci`, `chore`, `security`.
  مثال: `feat(engine): add work-stealing dynamic segmentation`

### 5.2 قائمة مراجعة الكود (Code Review Checklist)
لا يُدمج أي PR قبل التحقق من:
- [ ] **التصميم والمعمارية:** هل الكود في الطبقة الصحيحة؟ هل يحترم Ports & Adapters؟
- [ ] **الصحة والحالات الحدية:** ماذا يحدث عند حجم 0، ملف > 4GB، اسم عربي، انقطاع الشبكة، امتلاء القرص، أو الإلغاء في منتصف العملية؟
- [ ] **الأخطاء والسجلات:** هل الأخطاء مصنّفة بـ `VTX-*`؟ هل الأسرار محجوبة؟ هل `trace_id` ممرر؟
- [ ] **التزامن والأداء:** هل القنوات محدودة (Bounded)؟ هل يوجد Blocking داخل Async؟ هل يوجد تخصيص ذاكرة غير ضروري في المسار الساخن؟
- [ ] **الأمان:** هل كل مدخل خارجي مُتحقق منه؟ هل يوجد خطر Path Traversal / SSRF / UNC / XSS؟
- [ ] **الاختبارات والتوثيق:** هل تغطي الاختبارات السلوك الجديد ومسارات الفشل؟ هل تم تحديث `Tasks.md` و`rustdoc`؟

---

## 6. سياسة الترخيص والاعتماديات (Licensing Policy)

- **ترخيص Vortex الرسمي:** **`GPL-3.0-or-later`** (GNU General Public License v3.0 or later) — يضمن حقوق المؤلف الأصلية وحماية براءات الاختراع، ويفرض قانونياً أن يبقى Vortex وأي نسخة معدّلة أو مشتقة منه **مجانية ومفتوحة المصدر للأبد**.
  - **ملاحظة التوزيع على macOS / Safari (Phase 4):** يُوزَّع تطبيق macOS مباشرةً كحزمة موقّعة وموثّقة من Apple (**Notarized `.dmg` + Homebrew Cask**) خارج متجر Mac App Store (متوافق 100% مع `GPL-3.0-or-later`). كما يُرخّص مجلد الإضافة الرقيق `extension/` بـ `MPL-2.0 OR GPL-3.0-or-later` لإبقاء خيار متجر Safari مفتوحاً مستقبلاً دون أي مساس بحماية المحرك والتطبيق بـ `GPL-3.0-or-later`.
  - **أدوات الوسائط المُدارة عند الطلب (`FFmpeg` / `yt-dlp`):** تُنزَّل **On-Demand فقط** (خارج المثبّت الأساسي للحفاظ على `< 15 MB`) مع تحقق مزدوج (**توقيع Ed25519 + بصمة SHA-256**)، ويجب أن تكون نسخة FFmpeg مبنية حصراً بـ `--enable-gpl --enable-version3` و**بدون أي `--enable-nonfree`** (حظر قاطع لـ `libfdk-aac` وما شابهها) مع نشر وصفة البناء في `tools/ffmpeg-min-build/`.
- **الاعتماديات المسموحة (مفروضة بـ `cargo-deny`):**
  - ✅ مسموح: `MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `MPL-2.0`, `Zlib`, `BSL-1.0`, `Unicode-3.0`, `CC0-1.0`, `GPL-3.0-or-later`, `LGPL-3.0-or-later`.
  - ❌ ممنوع تلقائياً: `GPL-2.0-only` (غير متوافق مع Apache-2.0)، `AGPL` (إلا بمراجعة خاصة)، التراخيص التجارية المغلقة أو غير المحددة، و`SSPL`/`BSL` المقيدة.
- **مراجعة أي اعتمادية جديدة:** لا تُضاف Crate أو حزمة npm جديدة قبل تبرير الحاجة وفحص صيانتها وحجمها وأمانها (`cargo-audit`).
