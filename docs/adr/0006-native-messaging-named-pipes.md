# ADR-006: Native Messaging + Named Pipes ومنع أي منفذ TCP محلي

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
كثير من مديري التحميل يفتحون خادم HTTP/WebSocket محلي على `127.0.0.1:PORT` للتواصل مع إضافة المتصفح، مما يفتح ثغرات CSRF / DNS Rebinding / اختطاف محلي من أي صفحة ويب أو مستخدم آخر على الجهاز.

## Decision Drivers
- تقليص سطح الهجوم إلى الصفر على الشبكة المحلية.
- عدم إثارة جدار الحماية (Windows Firewall popups).
- التوافق الرسمي طويل الأمد مع Manifest V3.

## Considered Options
1. **Browser Native Messaging (`Extension ↔ Bridge`) + OS Named Pipe / UDS مع ACL للمستخدم الحالي (`Bridge/CLI ↔ Core`)** — المعيار الرسمي والأكثر أماناً، بدون أي منفذ شبكة.
2. **Localhost HTTP/WebSocket Server** — أسهل في الإعداد، لكنه يفتح سطح هجوم شبكي ويتطلب إدارة Tokens معقدة ويثير جدار الحماية أحياناً.

## Decision
اعتماد **Native Messaging + Named Pipes (Owner-SID ACL + `PIPE_REJECT_REMOTE_CLIENTS` + سقف `16` عميلاً متزامناً ومحدد معدل `TokenBucket` بـ `100 msg/s` وفق `TH-32`)**، ومنع فتح أي منفذ TCP/UDP محلي في التطبيق.

## Consequences
- **إيجابيات:** أمان عالٍ (مفصّل في `Threat-Model.md` TH-05)، لا نوافذ جدار حماية، وتقييد صارم بمعرّف الإضافة في `allowed_origins`.
- **سلبيات ومقايضات:** يتطلب تسجيل Native Host Manifest في `HKCU` أثناء التثبيت (بلا صلاحيات Admin) مع زر "إصلاح الربط" في شاشة التشخيص.
- **التحقق والفرض:** اختبار أمني يتحقق من عدم وجود أي Listening Port ومن صحة SDDL على الـ Pipe.
