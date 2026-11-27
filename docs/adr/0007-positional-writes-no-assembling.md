# ADR-007: الكتابة الموضعية المتناثرة (FSCTL_SET_SPARSE) مع حجز المساحة المنطقي وإلغاء التجميع

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
الأسلوب التقليدي (IDM) يحفظ كل جزء في ملف مؤقت منفصل ثم يجمعها بعد الوصول إلى 100% (ضعف المساحة + دقائق انتظار). وفي المقابل، استدعاء `SetFileInformationByHandle(FileEndOfFileInfo)` وحده لملف 50GB على Windows NTFS ثم الكتابة عند إزاحة 45GB يجبر النواة على عمل **Zero-Fill متزامن** لـ 45GB ويجمد التحميل، بينما `SetFileValidData` يتطلب صلاحيات Admin المرفوضة أمنياً.

## Decision Drivers
- انتهاء فوري عند 100% بصفر وقت تجميع وبصفر تجميد Zero-Fill وبصفر صلاحيات Admin.
- استهلاك مساحة 1× فقط ومنع تجاوز المساحة الفارغة بين التحميلات المتزامنة.
- التوافق الكامل مع NTFS/ReFS ومع فلاشات exFAT/FAT32.

## Considered Options
1. **فحص دعم القرص (`FILE_SUPPORTS_SPARSE_FILES`) ← تفعيل `FSCTL_SET_SPARSE` على NTFS/ReFS قبل ضبط الحجم + `VolumeSpaceManager` لحجز المساحة المنطقي في الذاكرة + تجميع كتابات محاذى لـ `4 KiB` (1–4 MiB) + محاولة فك علم الـ Sparse (`SetSparse = FALSE`) عند 100% بشكل غير قاتل (إذا رفض NTFS نسجّل `WARN` ونكمل الـ `rename` الذري طالما `RangeSet` مكتمل 100% والهاش سليم) + وضع تمديد متتابع آمن تلقائياً على exFAT/FAT32.**
2. **`FileEndOfFileInfo` بدون Sparse** — يسبب تجميد Zero-Fill كارثياً للملفات الكبيرة على Windows.
3. **ملفات أجزاء مؤقتة + تجميع نهائي دائماً** — أبطأ، مساحة مضاعفة، استهلاك SSD مضاعف.

## Decision
اعتماد **الكتابة الموضعية المتناثرة الذكية (`FSCTL_SET_SPARSE` + `VolumeSpaceManager`)** مع البديل التلقائي لـ exFAT/FAT32، ومعززة في `Phase 1.6` بدرع **`FILE_FLAG_OPEN_REPARSE_POINT` + `GetFinalPathNameByHandleW`** ضد هجمات `Symlink/Junction TOCTOU` (`TH-29`) وفحص سقف ملفات `FAT32` (`4 GiB - 1 B`) المسبق (`UX-M02`) والتحقق من وجود `.vortex` عند الاستئناف (`UX-M03`)، ومُطوّرة في `Phase 1.7` (`ADR-021`) إلى **تجميع مستقل لكل عامل (`Per-Worker 2–4 MiB Aligned Coalescing`) + كتابة موضعية متوازية عبر `FILE_FLAG_OVERLAPPED` (IOCP)** لرفع نسبة نجاح التجميع إلى `~100%` وخفض استدعاءات `WriteFile` وفحص `WdFilter.sys` بأكثر من `99%`!

## Consequences
- **إيجابيات:** بدء فوري حتى لملف 100GB بدون تجميد وبدون Admin، صفر وقت تجميع، وحماية صارمة من امتلاء القرص.
- **سلبيات ومقايضات:** احتمال تجزئة طفيفة على أقراص HDD الميكانيكية القديمة (لا أثر لها على SSD/NVMe، وأفضل بمراحل من تجميد دقائق).
- **التحقق والفرض:** اختبار إنشاء ملف 20GB والكتابة في آخر ميجابايت أولاً على Windows والتأكد أن العملية تتم في < 50ms، + اختبار `VolumeSpaceManager` لـ 3 تحميلات متزامنة.
