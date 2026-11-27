//! Shared benchmark utilities for Vortex (`tools/bench` — `P0-T16`).

/// Sample sensitive URL used by the redaction throughput benchmark.
pub const SAMPLE_SIGNED_URL: &str = "https://user:supersecret@cdn.example.com/archive.zip?X-Amz-Signature=deadbeef&token=secret123&page=1";
