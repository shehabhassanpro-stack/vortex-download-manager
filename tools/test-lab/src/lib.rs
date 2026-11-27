//! Vortex Test Lab (`tools/test-lab` — `P0-T15` / `Vortex-Architecture-Design.md` §19.2).
//!
//! Deterministic, programmable hostile HTTP server simulating the worst servers on the
//! internet without touching the public network or wasting disk space:
//! - Deterministic seeded virtual files (`0` bytes up to `10 GiB+`) with known SHA-256
//! - Per-TCP-connection rate throttling
//! - Non-range servers (`200` instead of `206`), Weak `ETag` (`W/"..."`) `If-Range` trap
//! - Cloudflare JA4 TLS-bound `403`, single-use tokens (`410` on second request)
//! - Corrupted `Content-Range`, chunked unknown-length responses, forced `gzip` despite `identity`
//! - Mid-stream aborts, `429 Retry-After`, expiring URLs, mutating `ETags`
//! - Long/circular redirects, SSRF redirects (`127.0.0.1` / `169.254.169.254`)
//! - RFC 5987 / RFC 2047 Arabic `Content-Disposition` and malicious Path Traversal headers.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, IF_RANGE, LAST_MODIFIED, LOCATION, RANGE, RETRY_AFTER,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Fixed RFC-7231 HTTP date used for deterministic `Last-Modified` testing.
pub const DEFAULT_LAST_MODIFIED: &str = "Wed, 25 Nov 2026 12:00:00 GMT";

/// Chunk size emitted by virtual file streams (`64 KiB`).
const STREAM_CHUNK_SIZE: usize = 64 * 1024;

/// Per-resource telemetry counters exposed by `/stats` for deterministic Playwright E2E assertions.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenEndpointStats {
    /// Count of initial `200`/`206` streams served to the browser.
    pub ok_streams: u64,
    /// Count of subsequent probes rejected (`410 Gone` or `403 cf-mitigated: challenge`).
    pub rejected_probes: u64,
}

/// Snapshot returned by `GET /stats`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LabTelemetrySnapshot {
    /// Total `206 Partial Content` requests served on `/throttled/{size}`.
    pub throttled_partial_requests: u64,
    /// Total secondary-connection `403` rejections emitted by `/concurrency-capped/{max_streams}/{size}`.
    pub concurrency_capped_rejections: u64,
    /// Peak simultaneous active streams observed on `/per-conn-cap/{bps}/{size}` (`P1.7-T10`).
    pub per_conn_cap_peak_streams: u64,
    /// Total requests served on `/per-conn-cap/{bps}/{size}` (`P1.7-T10`).
    pub per_conn_cap_requests: u64,
    /// Count of simulated mid-stream resets injected on `/transient-reset-once/{size}` (`P1.7-T10`).
    pub transient_reset_drops: u64,
    /// Count of successful subsequent requests after a transient drop (`P1.7-T10`).
    pub transient_reset_recoveries: u64,
    /// Count of intentionally stalled tail streams on `/tail-straggler/{size}` (`P1.7-T10`).
    pub tail_straggler_slow_hits: u64,
    /// Count of fast hedged rescue streams on `/tail-straggler/{size}` (`P1.7-T10`).
    pub tail_straggler_fast_rescues: u64,
    /// Per-token counters for `/single-use/{token}/{size}`.
    pub single_use: HashMap<String, TokenEndpointStats>,
    /// Per-id counters for `/browser-ja4/{id}/{size}`.
    pub browser_ja4: HashMap<String, TokenEndpointStats>,
}

/// Shared mutable state tracking single-use tokens, rate-limit counters, and expiring/mutating URLs.
#[derive(Default)]
struct LabState {
    consumed_tokens: Mutex<HashSet<String>>,
    consumed_ja4_sessions: Mutex<HashSet<String>>,
    single_use_stats: Mutex<HashMap<String, TokenEndpointStats>>,
    browser_ja4_stats: Mutex<HashMap<String, TokenEndpointStats>>,
    throttled_partial_requests: AtomicU64,
    concurrency_capped_rejections: AtomicU64,
    active_capped_streams: Mutex<HashMap<String, usize>>,
    per_conn_cap_active: AtomicU64,
    per_conn_cap_peak_streams: AtomicU64,
    per_conn_cap_requests: AtomicU64,
    transient_reset_seen: Mutex<HashMap<String, u64>>,
    transient_reset_drops: AtomicU64,
    transient_reset_recoveries: AtomicU64,
    tail_straggler_seen: Mutex<HashSet<String>>,
    tail_straggler_slow_hits: AtomicU64,
    tail_straggler_fast_rescues: AtomicU64,
    rate_limit_hits: AtomicU64,
    expiring_created: Mutex<HashMap<String, Instant>>,
    mutating_versions: Mutex<HashMap<String, u64>>,
}

/// Running instance of the local Vortex Test Lab server bound to an ephemeral loopback port.
pub struct TestLabServer {
    addr: SocketAddr,
    state: Arc<LabState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl TestLabServer {
    /// Spawns a fresh Test Lab server on `127.0.0.1:0`.
    ///
    /// # Errors
    /// Returns an I/O error if binding the loopback TCP listener fails.
    pub async fn spawn() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(LabState::default());
        let app = build_router(Arc::clone(&state));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });
            let _ = server.await;
        });

        Ok(Self {
            addr,
            state,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Spawns a real cleartext HTTP/2 (h2c prior-knowledge) high-BDP server on `127.0.0.1:0`
    /// supporting `/files/{size}`, `/throttled/{size}`, and `/header-bomb` (`P1.7-T10`).
    ///
    /// # Errors
    /// Returns an I/O error if binding the loopback TCP listener fails.
    pub async fn spawn_h2() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(LabState::default());
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            break;
                        };
                        let _ = stream.set_nodelay(true);
                        tokio::spawn(async move {
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let mut builder = hyper::server::conn::http2::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            );
                            builder
                                .initial_stream_window_size(Some(16 * 1024 * 1024))
                                .initial_connection_window_size(Some(64 * 1024 * 1024))
                                .max_frame_size(Some(256 * 1024))
                                .max_concurrent_streams(Some(128));
                            let service = hyper::service::service_fn(handle_h2_request);
                            let _ = builder.serve_connection(io, service).await;
                        });
                    }
                }
            }
        });

        Ok(Self {
            addr,
            state,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Returns a live snapshot of internal server counters without making an HTTP call (`P1.7-T10`).
    #[must_use]
    pub fn telemetry(&self) -> LabTelemetrySnapshot {
        snapshot_from_state(&self.state)
    }

    /// Returns the `SocketAddr` the Test Lab is listening on.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns the base HTTP URL (e.g. `http://127.0.0.1:54321`).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Bumps the `ETag`/version of a `/mutating-etag/{id}/{size}` resource mid-download.
    #[must_use]
    pub fn mutate_resource(&self, id: &str) -> u64 {
        if let Ok(mut guard) = self.state.mutating_versions.lock() {
            let entry = guard.entry(id.to_owned()).or_insert(1);
            *entry = entry.saturating_add(1);
            *entry
        } else {
            1
        }
    }

    /// Shuts down the Test Lab background server cleanly.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for TestLabServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[inline]
const fn splitmix64_word(seed: u64, word_index: u64) -> u64 {
    let mut x = word_index.wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministically generates `len` bytes starting at `offset` for a virtual file with `seed`.
///
/// Uses word-aligned 64-bit `SplitMix64` (`8` bytes per mix, `> 8 GB/s` per core) while
/// handling arbitrary unaligned `offset` and `len` boundaries so any `Range` split produces
/// bit-for-bit identical bytes without ever bottlenecking 10 Gbps benchmarks.
#[must_use]
pub fn deterministic_bytes(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut pos = offset;
    let mut remaining = len;

    // 1. Unaligned head bytes up to the next 8-byte boundary
    let head_lane = (pos & 7) as usize;
    if head_lane != 0 && remaining > 0 {
        let word = splitmix64_word(seed, pos >> 3).to_le_bytes();
        let take = remaining.min(8 - head_lane);
        if let Some(slice) = word.get(head_lane..head_lane + take) {
            out.extend_from_slice(slice);
        }
        pos = pos.saturating_add(take as u64);
        remaining -= take;
    }

    // 2. Fast aligned 8-byte words
    while remaining >= 8 {
        let word = splitmix64_word(seed, pos >> 3);
        out.extend_from_slice(&word.to_le_bytes());
        pos = pos.saturating_add(8);
        remaining -= 8;
    }

    // 3. Trailing 1..=7 bytes
    if remaining > 0 {
        let word = splitmix64_word(seed, pos >> 3).to_le_bytes();
        if let Some(slice) = word.get(..remaining) {
            out.extend_from_slice(slice);
        }
    }

    out
}

/// Computes the exact lowercase hex SHA-256 digest of a virtual seeded file of `size` bytes.
#[must_use]
pub fn deterministic_sha256(seed: u64, size: u64) -> String {
    let mut hasher = Sha256::new();
    let mut offset = 0_u64;
    while offset < size {
        let remaining = size.saturating_sub(offset);
        let chunk_len = usize::try_from(remaining.min(STREAM_CHUNK_SIZE as u64)).unwrap_or(0);
        if chunk_len == 0 {
            break;
        }
        let chunk = deterministic_bytes(seed, offset, chunk_len);
        hasher.update(&chunk);
        offset = offset.saturating_add(chunk_len as u64);
    }
    hex::encode(hasher.finalize())
}

/// Builds the full Axum [`Router`] with all standard and hostile HTTP test endpoints.
fn build_router(state: Arc<LabState>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/stats", get(serve_stats))
        .route("/sha256/{seed}/{size}", get(serve_expected_sha256))
        .route("/files/{size}", get(serve_standard_file))
        .route("/throttled/{size}", get(serve_throttled_file))
        .route("/no-range/{size}", get(serve_no_range))
        .route("/weak-etag/{size}", get(serve_weak_etag))
        .route("/waf-ja4/{size}", get(serve_waf_ja4))
        .route("/browser-ja4/{id}/{size}", get(serve_browser_ja4))
        .route("/single-use/{token}/{size}", get(serve_single_use))
        .route("/bad-content-range/{size}", get(serve_bad_content_range))
        .route("/chunked/{size}", get(serve_chunked_unknown_len))
        .route("/forced-gzip/{size}", get(serve_forced_gzip))
        .route("/drop-after/{drop_at}/{size}", get(serve_drop_after))
        .route("/rate-limit/{size}", get(serve_rate_limited))
        .route("/expiring/{id}/{size}", get(serve_expiring))
        .route("/mutating-etag/{id}/{size}", get(serve_mutating_etag))
        .route("/redirect/chain/{hops}/{size}", get(serve_redirect_chain))
        .route("/redirect/loop", get(serve_redirect_loop))
        .route("/redirect/ssrf", get(serve_redirect_ssrf))
        .route("/redirect/ssrf-metadata", get(serve_redirect_ssrf_metadata))
        .route("/redirect/cross-origin", get(serve_redirect_cross_origin))
        .route("/filename/{mode}", get(serve_filename_mode))
        .route(
            "/concurrency-capped/{max_streams}/{size}",
            get(serve_concurrency_capped),
        )
        .route("/quota-captcha", get(serve_quota_captcha))
        .route("/clock-skew", get(serve_clock_skew))
        .route("/header-bomb", get(serve_header_bomb))
        .route("/over-deliver/{size}", get(serve_over_deliver))
        .route("/per-conn-cap/{bps}/{size}", get(serve_per_conn_cap))
        .route(
            "/transient-reset-once/{size}",
            get(serve_transient_reset_once),
        )
        .route("/tail-straggler/{size}", get(serve_tail_straggler))
        .with_state(state)
}

fn snapshot_from_state(state: &LabState) -> LabTelemetrySnapshot {
    let single_use = state.single_use_stats.lock().map_or_default(|m| m.clone());
    let browser_ja4 = state.browser_ja4_stats.lock().map_or_default(|m| m.clone());
    LabTelemetrySnapshot {
        throttled_partial_requests: state.throttled_partial_requests.load(Ordering::Relaxed),
        concurrency_capped_rejections: state.concurrency_capped_rejections.load(Ordering::Relaxed),
        per_conn_cap_peak_streams: state.per_conn_cap_peak_streams.load(Ordering::Relaxed),
        per_conn_cap_requests: state.per_conn_cap_requests.load(Ordering::Relaxed),
        transient_reset_drops: state.transient_reset_drops.load(Ordering::Relaxed),
        transient_reset_recoveries: state.transient_reset_recoveries.load(Ordering::Relaxed),
        tail_straggler_slow_hits: state.tail_straggler_slow_hits.load(Ordering::Relaxed),
        tail_straggler_fast_rescues: state.tail_straggler_fast_rescues.load(Ordering::Relaxed),
        single_use,
        browser_ja4,
    }
}

async fn serve_stats(State(state): State<Arc<LabState>>) -> Json<LabTelemetrySnapshot> {
    Json(snapshot_from_state(&state))
}

async fn serve_expected_sha256(Path((seed, size)): Path<(u64, u64)>) -> String {
    deterministic_sha256(seed, size)
}

#[derive(Default)]
struct SeedQuery {
    seed: Option<u64>,
    bps: Option<u64>,
    retry_after: Option<u64>,
    fail_first: Option<u64>,
    ttl_ms: Option<u64>,
    strict_zero_416: bool,
}

fn parse_query(raw: &HashMap<String, String>) -> SeedQuery {
    SeedQuery {
        seed: raw.get("seed").and_then(|s| s.parse().ok()),
        bps: raw.get("bps").and_then(|s| s.parse().ok()),
        retry_after: raw.get("retry_after").and_then(|s| s.parse().ok()),
        fail_first: raw.get("fail_first").and_then(|s| s.parse().ok()),
        ttl_ms: raw.get("ttl_ms").and_then(|s| s.parse().ok()),
        strict_zero_416: raw
            .get("strict_zero_416")
            .is_some_and(|v| v == "true" || v == "1"),
    }
}

/// Parses an RFC 9110 single byte range (`bytes=start-end` or `bytes=start-`).
fn parse_byte_range(header_val: &str, total_size: u64) -> Option<(u64, u64)> {
    let spec = header_val.strip_prefix("bytes=")?;
    let (start_str, end_str) = spec.split_once('-')?;
    if total_size == 0 {
        return None;
    }
    let start: u64 = start_str.parse().ok()?;
    if start >= total_size {
        return None;
    }
    let end = if end_str.is_empty() {
        total_size - 1
    } else {
        end_str.parse::<u64>().ok()?.min(total_size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn build_virtual_stream_body(seed: u64, start: u64, len: u64, bps: Option<u64>) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let stream_start = Instant::now();
        let mut written = 0_u64;
        while written < len {
            let chunk_size =
                usize::try_from((len - written).min(STREAM_CHUNK_SIZE as u64)).unwrap_or(0);
            if chunk_size == 0 {
                break;
            }
            let data = deterministic_bytes(seed, start.saturating_add(written), chunk_size);
            let next_written = written.saturating_add(chunk_size as u64);
            if let Some(rate) = bps.filter(|&r| r > 0) {
                let target_micros = next_written
                    .saturating_mul(1_000_000)
                    .checked_div(rate)
                    .unwrap_or(0);
                let target_dur = Duration::from_micros(target_micros);
                if let Some(wait_dur) = target_dur.checked_sub(stream_start.elapsed())
                    && !wait_dur.is_zero()
                {
                    tokio::time::sleep(wait_dur).await;
                }
            }
            if tx.send(Ok(Bytes::from(data))).await.is_err() {
                break;
            }
            written = next_written;
        }
    });

    Body::from_stream(ReceiverStream { rx })
}

struct ReceiverStream {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl futures_core::Stream for ReceiverStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn make_range_response(
    headers: &HeaderMap,
    size: u64,
    seed: u64,
    etag: &str,
    bps: Option<u64>,
    is_weak_etag: bool,
) -> Response {
    let if_range_val = headers.get(IF_RANGE).and_then(|v| v.to_str().ok());
    // RFC 9110 §13.1.5: Weak ETag in `If-Range` MUST never match!
    let if_range_matches = match if_range_val {
        None => true,
        Some(cond) => {
            if cond.trim_start().starts_with("W/") {
                false
            } else {
                (!is_weak_etag && cond == etag) || cond == DEFAULT_LAST_MODIFIED
            }
        }
    };

    let range_header = headers.get(RANGE).and_then(|v| v.to_str().ok());
    if if_range_matches && let Some(raw_range) = range_header {
        if let Some((start, end)) = parse_byte_range(raw_range, size) {
            let len = end - start + 1;
            let mut resp = Response::new(build_virtual_stream_body(seed, start, len, bps));
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            let h = resp.headers_mut();
            h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            h.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                h.insert(CONTENT_LENGTH, v);
            }
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
                h.insert(CONTENT_RANGE, v);
            }
            if let Ok(v) = HeaderValue::from_str(etag) {
                h.insert(ETAG, v);
            }
            h.insert(
                LAST_MODIFIED,
                HeaderValue::from_static(DEFAULT_LAST_MODIFIED),
            );
            return resp;
        }
        if size > 0 {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response();
        }
    }

    let mut resp = Response::new(build_virtual_stream_body(seed, 0, size, bps));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        h.insert(CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(ETAG, v);
    }
    h.insert(
        LAST_MODIFIED,
        HeaderValue::from_static(DEFAULT_LAST_MODIFIED),
    );
    resp
}

async fn serve_standard_file(
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    if size == 0 && opts.strict_zero_416 && headers.contains_key(RANGE) {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(CONTENT_RANGE, "bytes */0")],
        )
            .into_response();
    }
    let seed = opts.seed.unwrap_or(42);
    let etag = format!("\"vtx-seed{seed}-size{size}\"");
    make_range_response(&headers, size, seed, &etag, None, false)
}

async fn serve_throttled_file(
    State(state): State<Arc<LabState>>,
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let seed = opts.seed.unwrap_or(42);
    let bps = opts.bps.unwrap_or(256 * 1024);
    let etag = format!("\"vtx-throttled-{seed}-{size}\"");
    let mut resp = make_range_response(&headers, size, seed, &etag, Some(bps), false);
    if resp.status() == StatusCode::PARTIAL_CONTENT {
        state
            .throttled_partial_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    if let Some(fname) = q.get("name")
        && let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{fname}\""))
    {
        resp.headers_mut().insert(CONTENT_DISPOSITION, v);
    }
    resp
}

async fn serve_no_range(
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let seed = parse_query(&q).seed.unwrap_or(42);
    let mut resp = Response::new(build_virtual_stream_body(seed, 0, size, None));
    *resp.status_mut() = StatusCode::OK;
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        resp.headers_mut().insert(CONTENT_LENGTH, v);
    }
    resp.headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("none"));
    resp
}

async fn serve_weak_etag(
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let seed = parse_query(&q).seed.unwrap_or(42);
    let weak = format!("W/\"weak-{seed}-{size}\"");
    make_range_response(&headers, size, seed, &weak, None, true)
}

async fn serve_waf_ja4(Path(_size): Path<u64>) -> Response {
    let mut resp = (
        StatusCode::FORBIDDEN,
        "Cloudflare JA4 TLS fingerprint mismatch challenge",
    )
        .into_response();
    resp.headers_mut().insert(
        HeaderName::from_static("cf-mitigated"),
        HeaderValue::from_static("challenge"),
    );
    resp
}

async fn serve_browser_ja4(
    State(state): State<Arc<LabState>>,
    Path((id, size)): Path<(String, u64)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let bps = parse_query(&q).bps;
    let first_session = state
        .consumed_ja4_sessions
        .lock()
        .is_ok_and(|mut set| set.insert(id.clone()));

    if !first_session {
        if let Ok(mut map) = state.browser_ja4_stats.lock() {
            let entry = map.entry(id).or_default();
            entry.rejected_probes = entry.rejected_probes.saturating_add(1);
        }
        let mut resp = (
            StatusCode::FORBIDDEN,
            "Cloudflare JA4 TLS fingerprint mismatch challenge",
        )
            .into_response();
        resp.headers_mut().insert(
            HeaderName::from_static("cf-mitigated"),
            HeaderValue::from_static("challenge"),
        );
        return resp;
    }

    if let Ok(mut map) = state.browser_ja4_stats.lock() {
        let entry = map.entry(id).or_default();
        entry.ok_streams = entry.ok_streams.saturating_add(1);
    }
    let mut resp = make_range_response(&headers, size, 88, "\"ja4-session-1\"", bps, false);
    resp.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"ja4-protected.bin\""),
    );
    resp
}

async fn serve_single_use(
    State(state): State<Arc<LabState>>,
    Path((token, size)): Path<(String, u64)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let bps = parse_query(&q).bps;
    let first_use = state
        .consumed_tokens
        .lock()
        .is_ok_and(|mut set| set.insert(token.clone()));
    if !first_use {
        if let Ok(mut map) = state.single_use_stats.lock() {
            let entry = map.entry(token).or_default();
            entry.rejected_probes = entry.rejected_probes.saturating_add(1);
        }
        return (
            StatusCode::GONE,
            "Single-use download token already consumed",
        )
            .into_response();
    }
    if let Ok(mut map) = state.single_use_stats.lock() {
        let entry = map.entry(token).or_default();
        entry.ok_streams = entry.ok_streams.saturating_add(1);
    }
    let mut resp = make_range_response(&headers, size, 77, "\"single-use-1\"", bps, false);
    resp.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"single-use.bin\""),
    );
    resp
}

async fn serve_bad_content_range(Path(size): Path<u64>) -> Response {
    let data = deterministic_bytes(42, 0, usize::try_from(size.min(1024)).unwrap_or(0));
    let mut resp = Response::new(Body::from(data));
    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    resp.headers_mut().insert(
        CONTENT_RANGE,
        HeaderValue::from_static("bytes 999999-1000000/500"),
    );
    resp
}

async fn serve_chunked_unknown_len(
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let seed = parse_query(&q).seed.unwrap_or(42);
    let mut resp = Response::new(build_virtual_stream_body(seed, 0, size, None));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp
}

async fn serve_forced_gzip(
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let seed = parse_query(&q).seed.unwrap_or(42);
    let raw = deterministic_bytes(seed, 0, usize::try_from(size.min(512 * 1024)).unwrap_or(0));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let _ = encoder.write_all(&raw);
    let compressed = encoder.finish().unwrap_or_default();

    let mut resp = Response::new(Body::from(compressed));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut()
        .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    resp
}

async fn serve_drop_after(
    Path((drop_at, size)): Path<(u64, u64)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let seed = parse_query(&q).seed.unwrap_or(42);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let to_send = usize::try_from(drop_at.min(size)).unwrap_or(0);

    tokio::spawn(async move {
        if to_send > 0 {
            let prefix = deterministic_bytes(seed, 0, to_send);
            let _ = tx.send(Ok(Bytes::from(prefix))).await;
        }
        let _ = tx
            .send(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated abrupt connection drop",
            )))
            .await;
    });

    let mut resp = Response::new(Body::from_stream(ReceiverStream { rx }));
    *resp.status_mut() = StatusCode::OK;
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        resp.headers_mut().insert(CONTENT_LENGTH, v);
    }
    resp
}

async fn serve_rate_limited(
    State(state): State<Arc<LabState>>,
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let fail_first = opts.fail_first.unwrap_or(1);
    let retry_after = opts.retry_after.unwrap_or(2);
    let hit = state.rate_limit_hits.fetch_add(1, Ordering::Relaxed);
    if hit < fail_first {
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, "Slow down").into_response();
        if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
            resp.headers_mut().insert(RETRY_AFTER, v);
        }
        return resp;
    }
    make_range_response(&headers, size, 42, "\"rl-ok\"", None, false)
}

async fn serve_expiring(
    State(state): State<Arc<LabState>>,
    Path((id, size)): Path<(String, u64)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let ttl = Duration::from_millis(parse_query(&q).ttl_ms.unwrap_or(100));
    let created = state.expiring_created.lock().map_or_else(
        |_| Instant::now(),
        |mut map| *map.entry(id).or_insert_with(Instant::now),
    );

    if created.elapsed() > ttl {
        return (StatusCode::FORBIDDEN, "Signed URL expired").into_response();
    }
    make_range_response(&headers, size, 42, "\"exp-v1\"", None, false)
}

async fn serve_mutating_etag(
    State(state): State<Arc<LabState>>,
    Path((id, size)): Path<(String, u64)>,
    headers: HeaderMap,
) -> Response {
    let ver = state
        .mutating_versions
        .lock()
        .map_or(1, |mut map| *map.entry(id).or_insert(1));
    let etag = format!("\"mut-v{ver}\"");
    make_range_response(&headers, size, 100 + ver, &etag, None, false)
}

async fn serve_redirect_chain(Path((hops, size)): Path<(u32, u64)>) -> Response {
    let next = if hops <= 1 {
        format!("/files/{size}")
    } else {
        format!("/redirect/chain/{}/{size}", hops - 1)
    };
    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(&next) {
        resp.headers_mut().insert(LOCATION, v);
    }
    resp
}

async fn serve_redirect_loop() -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut()
        .insert(LOCATION, HeaderValue::from_static("/redirect/loop"));
    resp
}

async fn serve_redirect_ssrf() -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(
        LOCATION,
        HeaderValue::from_static("http://127.0.0.1:8080/admin"),
    );
    resp
}

async fn serve_redirect_ssrf_metadata() -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(
        LOCATION,
        HeaderValue::from_static("http://169.254.169.254/latest/meta-data/"),
    );
    resp
}

async fn serve_redirect_cross_origin(Query(q): Query<HashMap<String, String>>) -> Response {
    let target = q
        .get("target")
        .cloned()
        .unwrap_or_else(|| "https://cdn-other-origin.example.org/file.bin".to_owned());
    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(&target) {
        resp.headers_mut().insert(LOCATION, v);
    }
    resp
}

async fn serve_filename_mode(Path(mode): Path<String>) -> Response {
    let disposition = match mode.as_str() {
        "rfc5987" => {
            // "تقرير-2026.pdf" UTF-8 percent-encoded
            "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%D8%AA%D9%82%D8%B1%D9%8A%D8%B1-2026.pdf"
        }
        "rfc2047" => {
            // "ملف-عربي.zip" Base64 RFC 2047 encoded
            "attachment; filename=\"=?UTF-8?B?2YXZhNmBLdi52LHYqNmKLnppcA==?=\""
        }
        "traversal" => {
            "attachment; filename=\"../../AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup/evil.exe\""
        }
        _ => "attachment; filename=\"standard.bin\"",
    };

    let mut resp = Response::new(Body::from(deterministic_bytes(1, 0, 128)));
    *resp.status_mut() = StatusCode::OK;
    if let Ok(v) = HeaderValue::from_str(disposition) {
        resp.headers_mut().insert(CONTENT_DISPOSITION, v);
    }
    resp
}

struct ConcurrencySlotGuard {
    state: Arc<LabState>,
    key: String,
}

impl Drop for ConcurrencySlotGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.state.active_capped_streams.lock()
            && let Some(cnt) = map.get_mut(&self.key)
        {
            *cnt = cnt.saturating_sub(1);
            if *cnt == 0 {
                map.remove(&self.key);
            }
        }
    }
}

fn build_guarded_virtual_stream_body(
    seed: u64,
    start: u64,
    len: u64,
    bps: Option<u64>,
    guard: ConcurrencySlotGuard,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _slot_guard = guard;
        let stream_start = Instant::now();
        let mut written = 0_u64;
        while written < len {
            let chunk_size =
                usize::try_from((len - written).min(STREAM_CHUNK_SIZE as u64)).unwrap_or(0);
            if chunk_size == 0 {
                break;
            }
            let data = deterministic_bytes(seed, start.saturating_add(written), chunk_size);
            let next_written = written.saturating_add(chunk_size as u64);
            if let Some(rate) = bps.filter(|&r| r > 0) {
                let target_micros = next_written
                    .saturating_mul(1_000_000)
                    .checked_div(rate)
                    .unwrap_or(0);
                let target_dur = Duration::from_micros(target_micros);
                if let Some(wait_dur) = target_dur.checked_sub(stream_start.elapsed())
                    && !wait_dur.is_zero()
                {
                    tokio::time::sleep(wait_dur).await;
                }
            }
            if tx.send(Ok(Bytes::from(data))).await.is_err() {
                break;
            }
            written = next_written;
        }
    });

    Body::from_stream(ReceiverStream { rx })
}

/// Simulates strict concurrency-capped hosts like `pixeldrain.com` (`P1.6-T11`):
/// allows at most `max_streams` simultaneous active streams and rejects excess connections with `403`.
async fn serve_concurrency_capped(
    State(state): State<Arc<LabState>>,
    Path((max_streams, size)): Path<(usize, u64)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let seed = opts.seed.unwrap_or(42);
    let bps = opts.bps.or(Some(2 * 1024 * 1024));
    let bucket_key = q
        .get("id")
        .cloned()
        .unwrap_or_else(|| format!("cap-{max_streams}-{size}"));
    let cap = max_streams.max(1);

    let acquired = state.active_capped_streams.lock().is_ok_and(|mut map| {
        let cur = map.entry(bucket_key.clone()).or_insert(0);
        if *cur >= cap {
            false
        } else {
            *cur += 1;
            true
        }
    });

    if !acquired {
        state
            .concurrency_capped_rejections
            .fetch_add(1, Ordering::Relaxed);
        let mut resp = (
            StatusCode::FORBIDDEN,
            r#"{"success":false,"value":"too_many_connections","message":"Too many simultaneous connections"}"#,
        )
            .into_response();
        resp.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        return resp;
    }

    let guard = ConcurrencySlotGuard {
        state: Arc::clone(&state),
        key: bucket_key,
    };
    let etag = format!("\"vtx-cap-{seed}-{size}\"");
    let range_header = headers.get(RANGE).and_then(|v| v.to_str().ok());
    let (status, start, len) = if let Some(raw_range) = range_header
        && let Some((s, e)) = parse_byte_range(raw_range, size)
    {
        (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
    } else {
        (StatusCode::OK, 0, size)
    };

    let mut resp = Response::new(build_guarded_virtual_stream_body(
        seed, start, len, bps, guard,
    ));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert(CONTENT_LENGTH, v);
    }
    if status == StatusCode::PARTIAL_CONTENT
        && len > 0
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
    {
        h.insert(CONTENT_RANGE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(ETAG, v);
    }
    h.insert(
        LAST_MODIFIED,
        HeaderValue::from_static(DEFAULT_LAST_MODIFIED),
    );
    if let Some(fname) = q.get("name")
        && let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{fname}\""))
    {
        h.insert(CONTENT_DISPOSITION, v);
    }
    resp
}

/// Simulates Pixeldrain bandwidth quota / CAPTCHA rejection (`VTX-RES-006` — `P1.6-T11`).
async fn serve_quota_captcha() -> Response {
    let mut resp = (
        StatusCode::FORBIDDEN,
        r#"{"success":false,"value":"file_rate_limited_captcha_required","message":"Quota reached; complete CAPTCHA"}"#,
    )
        .into_response();
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Simulates S3/CDN `403 RequestTimeTooSkewed` with a severely drifted `Date` header (`VTX-AUTH-002` — `P1.6-T11`).
async fn serve_clock_skew() -> Response {
    let mut resp = (
        StatusCode::FORBIDDEN,
        "<Error><Code>RequestTimeTooSkewed</Code><Message>Clock drift exceeds 5 minutes</Message></Error>",
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::DATE,
        HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
    );
    resp
}

/// Emits 150 response headers exceeding `MAX_RESPONSE_HEADERS = 100` (`TH-27` — `P1.6-T11`).
async fn serve_header_bomb() -> Response {
    let mut resp = (StatusCode::OK, "header-bomb").into_response();
    let h = resp.headers_mut();
    for i in 0..150_u32 {
        if let Ok(name) = HeaderName::from_bytes(format!("x-vtx-bomb-{i}").as_bytes()) {
            h.insert(name, HeaderValue::from_static("1"));
        }
    }
    resp
}

/// Declares a `206 Partial Content` range of `size` bytes via `Content-Range` (without `Content-Length`)
/// while intentionally streaming `size * 3` bytes to trigger `TH-27` over-delivery abort (`VTX-NET-005`).
async fn serve_over_deliver(Path(size): Path<u64>) -> Response {
    let declared = size.max(1024);
    let actual_stream = declared.saturating_mul(3);
    let mut resp = Response::new(build_virtual_stream_body(99, 0, actual_stream, None));
    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    let h = resp.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "bytes 0-{}/{}",
        declared - 1,
        declared.saturating_mul(2)
    )) {
        h.insert(CONTENT_RANGE, v);
    }
    resp
}

struct PerConnCapGuard {
    state: Arc<LabState>,
}

impl Drop for PerConnCapGuard {
    fn drop(&mut self) {
        self.state
            .per_conn_cap_active
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn build_atomic_guarded_stream(
    seed: u64,
    start: u64,
    len: u64,
    bps: Option<u64>,
    guard: PerConnCapGuard,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _guard = guard;
        let stream_start = Instant::now();
        let mut written = 0_u64;
        while written < len {
            let chunk_size =
                usize::try_from((len - written).min(STREAM_CHUNK_SIZE as u64)).unwrap_or(0);
            if chunk_size == 0 {
                break;
            }
            let data = deterministic_bytes(seed, start.saturating_add(written), chunk_size);
            let next_written = written.saturating_add(chunk_size as u64);
            if let Some(rate) = bps.filter(|&r| r > 0) {
                let target_micros = next_written
                    .saturating_mul(1_000_000)
                    .checked_div(rate)
                    .unwrap_or(0);
                let target_dur = Duration::from_micros(target_micros);
                if let Some(wait_dur) = target_dur.checked_sub(stream_start.elapsed())
                    && !wait_dur.is_zero()
                {
                    tokio::time::sleep(wait_dur).await;
                }
            }
            if tx.send(Ok(Bytes::from(data))).await.is_err() {
                break;
            }
            written = next_written;
        }
    });
    Body::from_stream(ReceiverStream { rx })
}

/// Simulates a CDN/host that caps throughput per TCP connection (`bps` bytes/sec) with no concurrency
/// cap — ideal for verifying `BBR/Gradient2` dynamic auto-scaling (`P1.7-T10`).
async fn serve_per_conn_cap(
    State(state): State<Arc<LabState>>,
    Path((bps, size)): Path<(u64, u64)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let seed = opts.seed.unwrap_or(42);
    state.per_conn_cap_requests.fetch_add(1, Ordering::Relaxed);
    let now_active = state.per_conn_cap_active.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = state
        .per_conn_cap_peak_streams
        .fetch_max(now_active, Ordering::Relaxed);
    let guard = PerConnCapGuard {
        state: Arc::clone(&state),
    };

    let etag = format!("\"vtx-perconn-{seed}-{size}\"");
    let range_header = headers.get(RANGE).and_then(|v| v.to_str().ok());
    let (status, start, len) = if let Some(raw_range) = range_header
        && let Some((s, e)) = parse_byte_range(raw_range, size)
    {
        (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
    } else {
        (StatusCode::OK, 0, size)
    };

    let mut resp = Response::new(build_atomic_guarded_stream(
        seed,
        start,
        len,
        Some(bps.max(64 * 1024)),
        guard,
    ));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert(CONTENT_LENGTH, v);
    }
    if status == StatusCode::PARTIAL_CONTENT
        && len > 0
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
    {
        h.insert(CONTENT_RANGE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(ETAG, v);
    }
    h.insert(
        LAST_MODIFIED,
        HeaderValue::from_static(DEFAULT_LAST_MODIFIED),
    );
    resp
}

/// Simulates a single transient mid-stream TCP reset on the first secondary `Range` request (`hit == 1`),
/// while all other requests succeed cleanly — verifies `NetConnectionLost` NEVER poisons
/// `SmartHostProfileCache` (`P1.7-T10`).
async fn serve_transient_reset_once(
    State(state): State<Arc<LabState>>,
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let seed = opts.seed.unwrap_or(55);
    let bps = opts.bps;
    let bucket = q.get("id").cloned().unwrap_or_else(|| format!("tr-{size}"));

    let is_secondary_range = headers
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|r| !r.starts_with("bytes=0-"));

    let should_drop = if is_secondary_range {
        state.transient_reset_seen.lock().is_ok_and(|mut map| {
            let cnt = map.entry(bucket).or_insert(0);
            *cnt += 1;
            *cnt == 1
        })
    } else {
        false
    };

    if should_drop
        && let Some(raw_range) = headers.get(RANGE).and_then(|v| v.to_str().ok())
        && let Some((start, end)) = parse_byte_range(raw_range, size)
    {
        state.transient_reset_drops.fetch_add(1, Ordering::Relaxed);
        let len = end - start + 1;
        let to_send = usize::try_from(len.min(64 * 1024)).unwrap_or(0);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        tokio::spawn(async move {
            if to_send > 0 {
                let prefix = deterministic_bytes(seed, start, to_send);
                let _ = tx.send(Ok(Bytes::from(prefix))).await;
            }
            let _ = tx
                .send(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated transient secondary reset",
                )))
                .await;
        });
        let mut resp = Response::new(Body::from_stream(ReceiverStream { rx }));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        let h = resp.headers_mut();
        h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
            h.insert(CONTENT_LENGTH, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
            h.insert(CONTENT_RANGE, v);
        }
        h.insert(ETAG, HeaderValue::from_static("\"vtx-tr-v1\""));
        return resp;
    }

    if is_secondary_range {
        state
            .transient_reset_recoveries
            .fetch_add(1, Ordering::Relaxed);
    }
    make_range_response(&headers, size, seed, "\"vtx-tr-v1\"", bps, false)
}

/// Simulates an `End-Game` slow tail straggler: the FIRST range request touching the final `25%` of
/// the file is throttled to `16 KiB/s`, while any subsequent hedged request for the same tail streams
/// at full unthrottled speed (`P1.7-T10`).
async fn serve_tail_straggler(
    State(state): State<Arc<LabState>>,
    Path(size): Path<u64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let opts = parse_query(&q);
    let seed = opts.seed.unwrap_or(88);
    let bucket = q
        .get("id")
        .cloned()
        .unwrap_or_else(|| format!("tail-{size}"));
    let parsed_range = headers
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| parse_byte_range(raw, size));

    let is_tail_segment = parsed_range.is_some_and(|(s, e)| {
        let len = e.saturating_sub(s).saturating_add(1);
        s > 0 && e + 1 == size && len <= 4 * 1024 * 1024
    });

    let bps = if is_tail_segment {
        let first_tail = state
            .tail_straggler_seen
            .lock()
            .is_ok_and(|mut set| set.insert(bucket));
        if first_tail {
            state
                .tail_straggler_slow_hits
                .fetch_add(1, Ordering::Relaxed);
            Some(16 * 1024)
        } else {
            state
                .tail_straggler_fast_rescues
                .fetch_add(1, Ordering::Relaxed);
            None
        }
    } else {
        opts.bps
    };

    let etag = format!("\"vtx-tail-{seed}-{size}\"");
    make_range_response(&headers, size, seed, &etag, bps, false)
}

async fn handle_h2_request(
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<Response, std::convert::Infallible> {
    let path = req.uri().path();
    let query_map: HashMap<String, String> = req.uri().query().map_or_default(|q| {
        q.split('&')
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                Some((k.to_owned(), v.to_owned()))
            })
            .collect()
    });
    let opts = parse_query(&query_map);
    let seed = opts.seed.unwrap_or(42);

    if path == "/header-bomb" {
        return Ok(serve_header_bomb().await);
    }
    if let Some(size_str) = path.strip_prefix("/files/")
        && let Ok(size) = size_str.parse::<u64>()
    {
        let etag = format!("\"vtx-h2-{seed}-{size}\"");
        return Ok(make_range_response(
            req.headers(),
            size,
            seed,
            &etag,
            opts.bps,
            false,
        ));
    }
    Ok((StatusCode::NOT_FOUND, "not found").into_response())
}
