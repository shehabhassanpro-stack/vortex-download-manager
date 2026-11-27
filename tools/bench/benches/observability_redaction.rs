//! Criterion benchmark for hot-path URL redaction and error formatting (`P0-T16`).

#![allow(missing_docs)]

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use vortex_bench::SAMPLE_SIGNED_URL;
use vortex_observability::{RedactedUrl, redact_sensitive_text};

const CLEAN_JSONL_LINE: &str = r#"{"timestamp":"2026-11-25T22:00:00Z","level":"INFO","target":"vortex_engine::scheduler","fields":{"message":"Segment completed","bytes":262144}}"#;
const TAINTED_JSONL_LINE: &str = r#"{"timestamp":"2026-11-25T22:00:00Z","level":"WARN","target":"vortex_net","fields":{"message":"Redirect failed for https://cdn.example.com/file.bin?token=secret123","authorization":"Bearer supersecret"}}"#;

fn bench_redacted_url(c: &mut Criterion) {
    let mut group = c.benchmark_group("observability");
    group.bench_function("redacted_url_display", |b| {
        b.iter(|| {
            let redacted = RedactedUrl::parse(black_box(SAMPLE_SIGNED_URL));
            black_box(redacted.to_string())
        });
    });
    group.bench_function("redact_clean_jsonl_fast_path", |b| {
        b.iter(|| black_box(redact_sensitive_text(black_box(CLEAN_JSONL_LINE))));
    });
    group.bench_function("redact_tainted_jsonl_ast", |b| {
        b.iter(|| black_box(redact_sensitive_text(black_box(TAINTED_JSONL_LINE))));
    });
    group.finish();
}

criterion_group!(benches, bench_redacted_url);
criterion_main!(benches);
