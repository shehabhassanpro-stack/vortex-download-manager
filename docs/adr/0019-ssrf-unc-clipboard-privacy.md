# ADR-019: الحماية من SSRF / UNC NTLM Leak وخصوصية مراقبة الحافظة

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
توجد فئتان من الثغرات الشائعة جداً والمُهملة في مديري التحميل على Windows:
1. استغلال الروابط أو التحويلات لإجبار Windows على إرسال NTLM Hash لمسار UNC (`\\attacker\share`) أو استهداف خدمات محلية (`127.0.0.1` / `169.254.169.254`) مع تسريب كوكيز عبر الأصول.
2. قراءة مراقب الحافظة لكلمات المرور المنسوخة من مديري كلمات المرور (1Password، Bitwarden، KeePass).

## Decision Drivers
- حماية استباقية بمستوى Microsoft SDL من اليوم الأول.
- احترام كامل لخصوصية المستخدم وأدواته الأمنية.

## Considered Options
1. **Allowlist صارم للبروتوكولات + فحص IP بعد DNS لمنع SSRF Redirect + تجريد `Cookie`/`Authorization` عند Cross-Origin Redirect + احترام صيغ استثناء مديري كلمات المرور في Windows Clipboard (`ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory`).**
2. **التعامل مع الروابط والحافظة بشكل ساذج** — يعرض المستخدم لثغرات خطيرة وتقارير CVE محرجة.

## Decision
اعتماد الضوابط الكاملة الموثقة في [Threat-Model.md](../Threat-Model.md) (`TH-03`, `TH-04`, `TH-09`, وامتدادها في `Phase 1.6` عبر `TH-26` و`TH-28`: فحص العناوين المحلولة داخل `VerifiedIpConnector` قبل كل `TcpStream::connect` لمنع `DNS Rebinding` والاستهداف المباشر، وربط الكوكيز في DPAPI بالأصل المسجّل `Origin-Pinned Secret Vault`) كمتطلبات إلزامية في `vortex-net` و`vortex-storage` و`vortex-ipc` و`vortex-platform`.

## Consequences
- **إيجابيات:** حصانة أمنية متفوقة على أغلب مديري التحميل المنافسين، وخصوصية حقيقية.
- **سلبيات ومقايضات:** قد يظهر للمستخدم سؤال تأكيد إذا قام موقع عام بتحويله لخادم محلي (سلوك مرغوب وأمني).
- **التحقق والفرض:** اختبارات أمنية آلية في `tools/test-lab` (TH-03, TH-04, TH-09).
