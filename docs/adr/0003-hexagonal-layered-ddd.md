# ADR-003: معمارية Hexagonal + Layered + DDD مع فرض الاعتماديات آلياً

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
الاعتماد على Layered Architecture تقليدية فقط يؤدي لتسرب تفاصيل HTTP وSQLite ونظام الملفات إلى منطق الأعمال، مما يصعّب الاختبار وإضافة بروتوكولات جديدة (FTP, HTTP/3, HLS, BitTorrent).

## Decision Drivers
- قابلية اختبار 100% لمنطق التحميل بدون شبكة أو قرص.
- إضافة بروتوكولات ومنصات جديدة دون تعديل المحرك (Open/Closed Principle).
- وضوح الحدود ومنع التدهور المعماري مع نمو المشروع.

## Considered Options
1. **Hexagonal (Ports & Adapters) + Layered + DDD** — `Domain` في المركز بصفر اعتماديات I/O، `Application` يعرّف Traits (Ports)، و`Infrastructure` ينفّذها من الخارج للداخل.
2. **Classic 3-Tier Layered** — أبسط في البداية لكنه يربط منطق الأعمال بالبنية التحتية ويُضعف القابلية للاختبار.
3. **Microservices** — تعقيد توزيعي وشبكي غير مبرر لتطبيق سطح مكتب محلي.

## Decision
اعتماد **Hexagonal + Layered + DDD** مقسمة إلى Crates مستقلة في Monorepo مع Composition Root وحيد.

## Consequences
- **إيجابيات:** عزل كامل، اختبارات فائقة السرعة، سهولة استبدال أي Adapter (مثل `io_uring` أو `HTTP/3`).
- **سلبيات ومقايضات:** قدر أكبر من الـ Traits والتجريد المبدئي — مبرر تماماً بطول عمر المشروع وحجمه.
- **التحقق والفرض:** أداة `tools/arch-check` تعمل في كل PR وتُفشل البناء عند أي اعتمادية معاكسة للـ DAG.
