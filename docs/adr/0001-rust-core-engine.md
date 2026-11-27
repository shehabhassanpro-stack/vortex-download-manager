# ADR-001: استخدام Rust (Edition 2024) للمحرك والمكونات الأصلية

- **Status:** Accepted
- **Date:** 2026-11-25

## Context
محرك Vortex يتعامل مع مدخلات شبكية غير موثوقة، اتصالات متوازية بسرعات تصل إلى 10 Gbps، وعمليات قرص منخفضة المستوى، مع ميزانية ذاكرة صارمة (< 60 MB في الخلفية) وهدف دعم Windows ثم macOS وLinux.

## Decision Drivers
- أمان الذاكرة (منع Buffer Overflows وUse-After-Free وData Races).
- أداء أصلي (Native) بدون توقفات Garbage Collector.
- استهلاك منخفض جداً للذاكرة وحجم تنفيذي صغير.
- منظومة Async وشبكات ناضجة (Tokio, hyper, rustls, quinn).

## Considered Options
1. **Rust (Edition 2024) + Tokio** — أداء C++، أمان ذاكرة وقت الترجمة، بدون GC، متعدد المنصات، تكامل ممتاز مع Tauri.
2. **C++23** — أداء مماثل، لكن ~70% من الثغرات الحرجة ناتجة عن أخطاء الذاكرة، وإدارة الاعتماديات وبناء متعدد المنصات أعقد.
3. **Go** — سريع التطوير، لكن الـ GC يستهلك ذاكرة أعلى ويضيف تأخيراً في المسار الساخن، والتكامل مع Win32/WebView أقل سلاسة.
4. **C# / .NET** — ممتاز على Windows فقط، لكن الـ Runtime أكبر حجماً ومرتبط بـ GC.

## Decision
اعتماد **Rust (Edition 2024)** كلغة وحيدة للمحرك (`crates/*`)، والوسيط (`apps/bridge`)، وسطر الأوامر (`apps/cli`)، والغلاف المكتبي (`apps/desktop/src-tauri`).

## Consequences
- **إيجابيات:** صفر ثغرات ذاكرية في الكود الآمن، استهلاك RAM منخفض (~15–25 MB للـ Core)، حزم تنفيذية مستقلة بدون Runtime خارجي.
- **سلبيات ومقايضات:** أزمنة بناء (Compile times) أطول نسبياً — نُخففها بتقسيم الـ Workspace لـ Crates صغيرة واستخدام `sccache` و`cargo-nextest`.
- **التحقق والفرض:** مثبت في `rust-toolchain.toml` و`Cargo.toml` مع `cargo clippy -D warnings` في CI.
