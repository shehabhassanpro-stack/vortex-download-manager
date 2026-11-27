# ADR-010: Protobuf v3 لبروتوكول IPC الداخلي بنهج Contract-First

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
الاتصال بين `vortex-bridge` و`vortex-cli` و`Vortex Core` عبر Named Pipe يحتاج عقداً صارماً وسريعاً يضمن عدم كسر التوافق عندما تتحدث إضافة المتصفح من المتجر قبل تحديث التطبيق المكتبي أو العكس.

## Decision Drivers
- أداء عالٍ وتغليف ثنائي مدمج وآمن.
- قواعد تطور وتوافق خلفي مثبتة عالمياً (`N / N-1 / N-2`).
- توليد تلقائي للأنواع من مصدر واحد (`proto/vortex_ipc.proto`).

## Considered Options
1. **Protobuf v3 (`prost`) على Named Pipe + JSON مطابق على Native Messaging (قيد المتصفح) + Capability Negotiation.**
2. **JSON خام على Named Pipe** — أسهل للفحص اليدوي، لكنه أضعف في فرض قواعد التطور والتوافق الثنائي وأثقل في التحليل.
3. **bincode / postcard** — أسرع، لكنه يفتقر للتوافق المرن بين الإصدارات المختلفة.

## Decision
اعتماد **Protobuf v3** مع Length-Delimited Framing على Named Pipe (بحد أقصى 4 MiB) وفق [Protocol-Spec.md](../Protocol-Spec.md)، مع حفظ Golden Wire Tests للتحقق من التوافق الخلفي في CI.

## Consequences
- **إيجابيات:** عقد وحيد، توافق مضمون بين الإصدارات، ومناعة عالية ضد انحراف المخطط.
- **سلبيات ومقايضات:** يتطلب وجود `protoc` في بيئة التطوير (تم تثبيته بالفعل).
- **التحقق والفرض:** اختبارات Golden Binary في `crates/vortex-ipc/tests/golden/`.
