//! `HttpByteSource` implementing `probe_and_stream` (`16 MiB` RAM buffer for `0ms` effective start,
//! WAF JA4 / single-use token auto-fallback), strict RFC 9110 `If-Range`, SSRF + Cross-Origin
//! credential stripping, and Keep-Alive `open_range` (`P1-T14`..`P1-T17`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, COOKIE, DATE, HOST, IF_RANGE, LOCATION, RANGE, REFERER, USER_AGENT,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use tokio::sync::mpsc;
use url::Url;
use vortex_application::{
    AppResult, ByteSource, ByteStream, FallbackReason, ProbeOutcome, ResourceInfo, ResourceRequest,
    TransportMetrics,
};
use vortex_domain::{ByteRange, Clock, FilenameSanitizer, IfRangeDirective};
use vortex_error::{ApiError, ErrorCode, Recoverability, TraceId, VortexError};
use vortex_observability::{ExposeSecret, RedactedUrl};

use crate::connector::{ActiveHttpConn, MAX_HTTP1_HEADER_BUF_BYTES, OriginKey, TransportPool};
use crate::headers::{
    ParsedContentRange, extract_resume_validators, is_same_origin, parse_content_range,
    parse_disposition_raw_and_sanitized, parse_http_date_epoch_secs, parse_retry_after_ms,
};
use crate::metrics::SocketMetricsProbe;
use crate::resilience::{
    DEFAULT_CIRCUIT_FAILURE_THRESHOLD, DEFAULT_CIRCUIT_OPEN_MS, HostCircuitBreaker,
    WatchdogByteStream,
};

/// Maximum HTTP redirect hops allowed before aborting (`§11.3` & `TH-14`).
pub const MAX_REDIRECT_HOPS: usize = 10;

/// Maximum HTTP response headers accepted per response (`TH-27` Header-Bomb defense).
pub const MAX_RESPONSE_HEADERS: usize = 100;

/// Maximum sanitized error-body bytes sniffed on `403`/`429`/`503` for accurate classification (`P1.6-T03`).
const MAX_ERROR_BODY_SNIFF_BYTES: usize = 1024;

/// Maximum time spent sniffing a non-2xx error body (`500ms`).
const ERROR_BODY_SNIFF_TIMEOUT: Duration = Duration::from_millis(500);

/// Clock skew delta threshold (`5 minutes`) flagging `VTX-AUTH-002` on `403` responses (`UX-M07`).
const CLOCK_SKEW_THRESHOLD_SECS: i64 = 300;

/// Bounded `16 MiB` probe stream RAM buffer (`256 * 64 KiB` slots) for `0ms` effective start (`§8.2`).
const PROBE_CHANNEL_SLOTS: usize = 256;

/// Configuration options for [`HttpByteSource`].
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct HttpClientConfig {
    /// TCP + TLS handshake timeout (`default 10s`).
    pub connect_timeout: Duration,
    /// Stall watchdog per-chunk idle timeout (`default 30s` — `§30.2`).
    pub idle_read_timeout: Duration,
    /// Consecutive transient failures before opening host circuit (`default 5`).
    pub circuit_failure_threshold: u32,
    /// Circuit open cooldown (`default 15s`).
    pub circuit_open_cooldown_ms: u64,
    /// Allow direct dials to private/loopback networks (`false` by default — `TH-26` SSRF defense!).
    pub allow_private_network: bool,
    /// Enable High-BDP `HTTP/2` (`ALPN: ["h2", "http/1.1"]`, `16/64 MiB` windows — `P1.7-T04`).
    pub enable_http2: bool,
    /// Test-only cleartext HTTP/2 prior knowledge for local `tools/test-lab` H2 tests (always `false` in production!).
    pub http2_prior_knowledge_for_test: bool,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            idle_read_timeout: Duration::from_secs(30),
            circuit_failure_threshold: DEFAULT_CIRCUIT_FAILURE_THRESHOLD,
            circuit_open_cooldown_ms: DEFAULT_CIRCUIT_OPEN_MS,
            allow_private_network: false,
            enable_http2: true,
            http2_prior_knowledge_for_test: false,
        }
    }
}

impl HttpClientConfig {
    /// Configuration for local `tools/test-lab` (`127.0.0.1`) integration tests; cross-origin
    /// SSRF redirects and public-to-private DNS rebinding remain strictly blocked (`TH-04` / `TH-26`)!
    #[must_use]
    pub fn for_local_testing() -> Self {
        Self {
            allow_private_network: true,
            ..Self::default()
        }
    }

    /// Configuration for local `tools/test-lab` cleartext HTTP/2 (`h2c`) integration tests (`P1.7-T04`).
    #[must_use]
    pub fn for_local_h2_testing() -> Self {
        Self {
            allow_private_network: true,
            enable_http2: true,
            http2_prior_knowledge_for_test: true,
            ..Self::default()
        }
    }
}

/// Production HTTP/1.1 & High-BDP HTTP/2 `ByteSource` adapter (`crates/vortex-net`).
#[derive(Debug, Clone)]
pub struct HttpByteSource {
    pool: TransportPool,
    breaker: HostCircuitBreaker,
    cfg: HttpClientConfig,
    clock: Arc<dyn Clock>,
}

impl HttpByteSource {
    /// Constructs a new [`HttpByteSource`].
    ///
    /// # Errors
    /// Returns `VTX-NET-003` if OS TLS verifier initialization fails.
    pub fn new(clock: Arc<dyn Clock>, cfg: HttpClientConfig) -> AppResult<Self> {
        let pool = TransportPool::new_with_options(
            cfg.connect_timeout,
            cfg.enable_http2,
            cfg.http2_prior_knowledge_for_test,
        )?;
        let breaker = HostCircuitBreaker::new(
            Arc::clone(&clock),
            cfg.circuit_failure_threshold,
            cfg.circuit_open_cooldown_ms,
        );
        Ok(Self {
            pool,
            breaker,
            cfg,
            clock,
        })
    }

    /// Exposes the underlying [`TransportPool`] (e.g., to inspect Keep-Alive reuse metrics).
    #[must_use]
    pub const fn transport_pool(&self) -> &TransportPool {
        &self.pool
    }

    /// Exposes the per-host [`HostCircuitBreaker`].
    #[must_use]
    pub const fn circuit_breaker(&self) -> &HostCircuitBreaker {
        &self.breaker
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_with_redirects(
        &self,
        req: &ResourceRequest,
        range_header: &str,
        if_range_header: Option<&str>,
    ) -> AppResult<(
        Url,
        StatusCode,
        HeaderMap,
        Incoming,
        ActiveHttpConn,
        OriginKey,
    )> {
        let initial_url = FilenameSanitizer::validate_download_url(&req.url)?;
        let mut current_url = initial_url.clone();
        let mut send_credentials = true;

        for hop in 0..=MAX_REDIRECT_HOPS {
            if hop == MAX_REDIRECT_HOPS {
                return Err(VortexError::coded(
                    ErrorCode::NetMalformedResponse,
                    req.trace_id,
                    format!("Exceeded maximum HTTP redirect limit ({MAX_REDIRECT_HOPS})"),
                ));
            }

            let origin = origin_key_from_url(&current_url, req.trace_id)?;
            self.breaker.check_permitted(&origin.host, req.trace_id)?;

            // Enforce socket-level SSRF non-global IP check on EVERY dial when `allow_private_network == false`
            // (`TH-26`) AND unconditionally on Cross-Origin Redirects (`TH-04`)!
            let crossed_origin = !is_same_origin(&initial_url, &current_url);
            let enforce_ssrf_public =
                (crossed_origin || !self.cfg.allow_private_network) && !req.allow_private_redirect;

            let mut conn = match self
                .pool
                .checkout_or_connect(&origin, enforce_ssrf_public, req.trace_id)
                .await
            {
                Ok(c) => c,
                Err(err) => {
                    if err.as_api_error().recoverability.is_retryable() {
                        let _ = self
                            .breaker
                            .record_transient_failure(&origin.host, req.trace_id);
                    }
                    return Err(err);
                }
            };

            let is_http2 = matches!(conn, ActiveHttpConn::Http2(_));
            let http_req = build_http_request(
                &current_url,
                &origin,
                req,
                send_credentials,
                range_header,
                if_range_header,
                is_http2,
                req.trace_id,
            )?;

            let resp = match tokio::time::timeout(
                self.cfg.idle_read_timeout,
                conn.send_request(http_req),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(hyper_err)) => {
                    self.pool.release_aborted(&origin, conn);
                    if hyper_err.is_parse()
                        || hyper_err.is_parse_too_large()
                        || hyper_err.is_parse_status()
                    {
                        return Err(VortexError::coded(
                            ErrorCode::NetMalformedResponse,
                            req.trace_id,
                            format!(
                                "HTTP response violated parser or header count/size bounds (TH-27): {hyper_err}"
                            ),
                        ));
                    }
                    let _ = self
                        .breaker
                        .record_transient_failure(&origin.host, req.trace_id);
                    return Err(VortexError::coded(
                        ErrorCode::NetConnectionLost,
                        req.trace_id,
                        format!("HTTP request failed: {hyper_err}"),
                    ));
                }
                Err(_) => {
                    self.pool.release_aborted(&origin, conn);
                    let _ = self
                        .breaker
                        .record_transient_failure(&origin.host, req.trace_id);
                    return Err(VortexError::coded(
                        ErrorCode::NetConnectionLost,
                        req.trace_id,
                        "Timed out awaiting HTTP response headers",
                    ));
                }
            };

            let (parts, body) = resp.into_parts();
            // Decoupled `TH-27` Header-Bomb Guard (`<= 100` headers AND `<= 64 KiB` total header bytes)
            // while keeping the `512 KiB` body read buffer completely unthrottled (`P1.7-T03`)!
            let header_bytes = total_header_bytes(&parts.headers);
            if parts.headers.len() > MAX_RESPONSE_HEADERS
                || header_bytes > MAX_HTTP1_HEADER_BUF_BYTES
            {
                self.pool.release_aborted(&origin, conn);
                return Err(VortexError::coded(
                    ErrorCode::NetMalformedResponse,
                    req.trace_id,
                    format!(
                        "HTTP response exceeded header bounds (count={} / max={}, bytes={header_bytes} / max={MAX_HTTP1_HEADER_BUF_BYTES}) (TH-27)",
                        parts.headers.len(),
                        MAX_RESPONSE_HEADERS
                    ),
                ));
            }
            if parts.status.is_redirection() {
                let Some(loc_raw) = parts.headers.get(LOCATION).and_then(|v| v.to_str().ok())
                else {
                    return Err(VortexError::coded(
                        ErrorCode::NetMalformedResponse,
                        req.trace_id,
                        format!("Redirect {} missing Location header", parts.status),
                    ));
                };

                let next_url = current_url.join(loc_raw.trim()).map_err(|e| {
                    VortexError::coded(
                        ErrorCode::SecPolicyViolation,
                        req.trace_id,
                        format!("Invalid redirect Location URL: {e}"),
                    )
                })?;
                // Validate redirect target scheme (`http`/`https` only — blocks `file://` & UNC `TH-03`!)
                let _ = FilenameSanitizer::validate_download_url(next_url.as_str())?;

                // Strip `Cookie` and `Authorization` immediately on ANY Cross-Origin Redirect (`TH-04`)!
                if !is_same_origin(&current_url, &next_url) {
                    if send_credentials {
                        tracing::debug!(
                            trace_id = %req.trace_id,
                            from = %RedactedUrl::parse(current_url.as_str()),
                            to = %RedactedUrl::parse(next_url.as_str()),
                            "Cross-origin redirect detected; stripping Cookie and Authorization headers"
                        );
                    }
                    send_credentials = false;
                }

                // Drain redirect body so the Keep-Alive socket can be cleanly returned to pool
                let mut body_to_drain = body;
                if tokio::time::timeout(Duration::from_millis(250), async {
                    while let Some(Ok(_)) = body_to_drain.frame().await {}
                })
                .await
                .is_ok()
                {
                    self.pool.checkin(origin, conn);
                }

                current_url = next_url;
                continue;
            }

            if parts.status.is_server_error() || parts.status == StatusCode::TOO_MANY_REQUESTS {
                let _ = self
                    .breaker
                    .record_transient_failure(&origin.host, req.trace_id);
            } else {
                self.breaker.record_success(&origin.host);
            }

            return Ok((current_url, parts.status, parts.headers, body, conn, origin));
        }

        Err(VortexError::coded(
            ErrorCode::NetMalformedResponse,
            req.trace_id,
            "Redirect loop exhausted",
        ))
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl ByteSource for HttpByteSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        let (final_url, status, headers, body, conn, origin) =
            self.execute_with_redirects(req, "bytes=0-", None).await?;
        let now_ms = self.clock.now_unix_ms();

        // 1. WAF TLS JA3/JA4 / Single-Use Token Auto-Fallback (`§8.2` & `P1-T15`)!
        if req.is_browser_handoff
            && let Some(fallback) = classify_handoff_fallback(status, &headers)
        {
            return Ok(fallback);
        }

        // 2. Zero-byte resource returning `416 Range Not Satisfiable` + `Content-Range: bytes */0`
        if status == StatusCode::RANGE_NOT_SATISFIABLE
            && let Some(cr_raw) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok())
            && matches!(
                parse_content_range(cr_raw, req.trace_id),
                Ok(ParsedContentRange::Unsatisfied { total: 0 })
            )
        {
            let metrics = conn.metrics_probe().sample(now_ms);
            self.pool.checkin(origin, conn);
            let (raw_suggested_filename, suggested_filename) =
                parse_disposition_raw_and_sanitized(&headers);
            let info = ResourceInfo {
                final_url: final_url.to_string(),
                total_size: Some(0),
                supports_range: true,
                mime_type: clean_mime(&headers),
                suggested_filename,
                raw_suggested_filename,
                validators: extract_resume_validators(&headers, Some(0)),
                server_checksum: None,
                transport_metrics: metrics,
            };
            return Ok(ProbeOutcome::Transferable {
                info: Box::new(info),
                initial_stream: Box::new(EmptyByteStream { metrics }),
            });
        }

        if !status.is_success() {
            let snippet = read_bounded_error_snippet(body).await;
            return Err(map_http_status_error(
                status,
                &headers,
                &snippet,
                now_ms,
                req.trace_id,
            ));
        }

        // Reject forced `Content-Encoding` (e.g. `gzip` despite `Accept-Encoding: identity` — `§11.3`)
        ensure_identity_encoding(&headers, req.trace_id)?;

        let (supports_range, total_size) = if status == StatusCode::PARTIAL_CONTENT {
            let Some(cr_val) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
                return Err(VortexError::coded(
                    ErrorCode::NetMalformedResponse,
                    req.trace_id,
                    "206 Partial Content response missing Content-Range header",
                ));
            };
            match parse_content_range(cr_val, req.trace_id)? {
                ParsedContentRange::ByteInterval {
                    start: 0, total, ..
                } => (true, total),
                other => {
                    return Err(VortexError::coded(
                        ErrorCode::NetMalformedResponse,
                        req.trace_id,
                        format!("Probe requested bytes=0-, but server returned {other:?}"),
                    ));
                }
            }
        } else {
            let len = headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            (false, len)
        };

        let metrics_probe = conn.metrics_probe().clone();
        let initial_metrics = metrics_probe.sample(now_ms);
        let validators = extract_resume_validators(&headers, total_size);
        let (raw_suggested_filename, suggested_filename) =
            parse_disposition_raw_and_sanitized(&headers);
        let mime_type = clean_mime(&headers);

        // Pump live probe stream into a bounded `16 MiB` RAM channel (`0ms` wasted start latency!)
        let buffered_stream = spawn_probe_ram_pump(
            body,
            conn,
            origin,
            self.pool.clone(),
            Arc::clone(&self.clock),
            total_size,
            self.cfg.idle_read_timeout,
            req.trace_id,
        );

        let info = ResourceInfo {
            final_url: final_url.to_string(),
            total_size,
            supports_range,
            mime_type,
            suggested_filename,
            raw_suggested_filename,
            validators,
            server_checksum: None,
            transport_metrics: initial_metrics,
        };

        Ok(ProbeOutcome::Transferable {
            info: Box::new(info),
            initial_stream: Box::new(buffered_stream),
        })
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        let range_hdr = range.to_http_range_header();
        let directive = req.validators.select_if_range();
        let if_range_str = match &directive {
            IfRangeDirective::SendHeader(val) => Some(val.as_str()),
            IfRangeDirective::ManualValidate206 => None,
        };

        let (_, status, headers, body, conn, origin) = self
            .execute_with_redirects(req, &range_hdr, if_range_str)
            .await?;
        let now_ms = self.clock.now_unix_ms();

        if status == StatusCode::OK {
            // Server ignored `Range` (or `If-Range` detected mutated Strong ETag and fell back to `200 OK`!)
            if matches!(directive, IfRangeDirective::SendHeader(_)) {
                let fresh = extract_resume_validators(&headers, None);
                req.validators.validate_on_resume(&fresh)?;
            }
            return Err(VortexError::coded(
                ErrorCode::ResRangeUnsupported,
                req.trace_id,
                format!("Server returned 200 OK instead of 206 Partial Content for {range_hdr}"),
            ));
        }

        if !status.is_success() {
            let snippet = read_bounded_error_snippet(body).await;
            return Err(map_http_status_error(
                status,
                &headers,
                &snippet,
                now_ms,
                req.trace_id,
            ));
        }

        ensure_identity_encoding(&headers, req.trace_id)?;

        // Verify `Content-Range` matches requested `[range.start, range.last_inclusive]` (`TH-10`!)
        let Some(cr_raw) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
            return Err(VortexError::coded(
                ErrorCode::NetMalformedResponse,
                req.trace_id,
                "206 Partial Content missing Content-Range header",
            ));
        };
        let total_from_cr = match parse_content_range(cr_raw, req.trace_id)? {
            ParsedContentRange::ByteInterval {
                start,
                end_inclusive,
                total,
            } => {
                if start != range.start() || end_inclusive != range.last_inclusive() {
                    return Err(VortexError::coded(
                        ErrorCode::NetMalformedResponse,
                        req.trace_id,
                        format!(
                            "Content-Range mismatch: requested bytes={}-{}, got bytes={start}-{end_inclusive}",
                            range.start(),
                            range.last_inclusive()
                        ),
                    ));
                }
                total
            }
            ParsedContentRange::Unsatisfied { total } => {
                return Err(VortexError::coded(
                    ErrorCode::NetMalformedResponse,
                    req.trace_id,
                    format!("Unsatisfied Content-Range bytes */{total} on 206"),
                ));
            }
        };

        // Programmatic `206` validator check when Weak `ETag` / Weak `Last-Modified` prevented `If-Range` (`§8.6.3`)!
        let fresh_validators = extract_resume_validators(&headers, total_from_cr);
        req.validators.validate_on_resume(&fresh_validators)?;

        let initial_metrics = conn.metrics_probe().sample(now_ms);
        let live_stream = HyperBodyStream {
            body: Some(body),
            conn: Some((origin, conn, self.pool.clone())),
            expected_remaining: Some(range.len()),
            clock: Arc::clone(&self.clock),
            trace_id: req.trace_id,
        };
        let guarded =
            WatchdogByteStream::new(live_stream, self.cfg.idle_read_timeout, req.trace_id);
        Ok((Box::new(guarded), initial_metrics))
    }
}

struct EmptyByteStream {
    metrics: TransportMetrics,
}

#[async_trait]
impl ByteStream for EmptyByteStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        Ok(None)
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.metrics
    }
}

struct HyperBodyStream {
    body: Option<Incoming>,
    conn: Option<(OriginKey, ActiveHttpConn, TransportPool)>,
    expected_remaining: Option<u64>,
    clock: Arc<dyn Clock>,
    trace_id: TraceId,
}

impl Drop for HyperBodyStream {
    fn drop(&mut self) {
        self.body = None;
        if let Some((origin, conn, pool)) = self.conn.take() {
            pool.release_aborted(&origin, conn);
        }
    }
}

#[async_trait]
impl ByteStream for HyperBodyStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        let Some(body) = self.body.as_mut() else {
            return Ok(None);
        };
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data()
                        && !data.is_empty()
                    {
                        if let Some(rem) = &mut self.expected_remaining {
                            let len = data.len() as u64;
                            if len > *rem {
                                // Strict byte-ceiling enforcement (`TH-27` Over-Delivery Guard)!
                                self.body = None;
                                if let Some((origin, conn, pool)) = self.conn.take() {
                                    pool.release_aborted(&origin, conn);
                                }
                                return Err(VortexError::coded(
                                    ErrorCode::NetMalformedResponse,
                                    self.trace_id,
                                    format!(
                                        "Server over-delivered {len} bytes exceeding expected remaining {rem} bytes (TH-27)"
                                    ),
                                ));
                            }
                            *rem -= len;
                        }
                        return Ok(Some(data));
                    }
                }
                Some(Err(err)) => {
                    self.body = None;
                    if let Some((origin, conn, pool)) = self.conn.take() {
                        pool.release_aborted(&origin, conn);
                    }
                    return Err(VortexError::coded(
                        ErrorCode::NetConnectionLost,
                        self.trace_id,
                        format!("Mid-stream HTTP read error: {err}"),
                    ));
                }
                None => {
                    self.body = None;
                    if let Some(rem) = self.expected_remaining
                        && rem > 0
                    {
                        if let Some((origin, conn, pool)) = self.conn.take() {
                            pool.release_aborted(&origin, conn);
                        }
                        return Err(VortexError::coded(
                            ErrorCode::NetConnectionLost,
                            self.trace_id,
                            format!("Premature EOF: {rem} bytes missing from HTTP body"),
                        ));
                    }
                    // Clean EOF: return warm Keep-Alive/H2 socket back to `TransportPool` (`100%` reuse!)
                    if let Some((origin, conn, pool)) = self.conn.take() {
                        pool.checkin(origin, conn);
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn transport_metrics(&self) -> TransportMetrics {
        if let Some((_, conn, _)) = &self.conn {
            conn.metrics_probe().sample(self.clock.now_unix_ms())
        } else {
            TransportMetrics::default()
        }
    }
}

struct BufferedProbeStream {
    rx: mpsc::Receiver<AppResult<Bytes>>,
    metrics_probe: SocketMetricsProbe,
    clock: Arc<dyn Clock>,
    stop_at_offset: Arc<AtomicU64>,
}

#[async_trait]
impl ByteStream for BufferedProbeStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        match self.rx.recv().await {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.metrics_probe.sample(self.clock.now_unix_ms())
    }

    fn set_stop_offset(&self, stop_offset: u64) {
        self.stop_at_offset.fetch_min(stop_offset, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_probe_ram_pump(
    body: Incoming,
    conn: ActiveHttpConn,
    origin: OriginKey,
    pool: TransportPool,
    clock: Arc<dyn Clock>,
    expected_total_size: Option<u64>,
    idle_timeout: Duration,
    trace_id: TraceId,
) -> BufferedProbeStream {
    let (tx, rx) = mpsc::channel::<AppResult<Bytes>>(PROBE_CHANNEL_SLOTS);
    let metrics_probe = conn.metrics_probe().clone();
    let pump_clock = Arc::clone(&clock);
    let stop_at_offset = Arc::new(AtomicU64::new(u64::MAX));
    let pump_stop = Arc::clone(&stop_at_offset);

    tokio::spawn(async move {
        let raw_stream = HyperBodyStream {
            body: Some(body),
            conn: Some((origin, conn, pool)),
            expected_remaining: expected_total_size,
            clock: pump_clock,
            trace_id,
        };
        let mut watchdog = WatchdogByteStream::new(raw_stream, idle_timeout, trace_id);
        let mut pumped_bytes = 0_u64;
        loop {
            let stop_limit = pump_stop.load(Ordering::Acquire);
            if pumped_bytes >= stop_limit {
                // Zero-waste stop (`P1.7-T03`): stop pumping past Worker #1's stolen split boundary!
                break;
            }
            match watchdog.next_chunk().await {
                Ok(Some(mut chunk)) => {
                    let live_limit = pump_stop.load(Ordering::Acquire);
                    if pumped_bytes >= live_limit {
                        break;
                    }
                    let max_take = usize::try_from(live_limit - pumped_bytes).unwrap_or(usize::MAX);
                    let hit_stop = chunk.len() > max_take;
                    if hit_stop {
                        chunk = chunk.slice(..max_take);
                    }
                    pumped_bytes = pumped_bytes.saturating_add(chunk.len() as u64);
                    if tx.send(Ok(chunk)).await.is_err() || hit_stop || pumped_bytes >= live_limit {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });

    BufferedProbeStream {
        rx,
        metrics_probe,
        clock,
        stop_at_offset,
    }
}

fn classify_handoff_fallback(status: StatusCode, headers: &HeaderMap) -> Option<ProbeOutcome> {
    match status {
        StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE => {
            let is_waf = headers.contains_key("cf-mitigated")
                || headers
                    .get("server")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|s| s.to_ascii_lowercase().contains("cloudflare"));
            let reason = if is_waf || status == StatusCode::SERVICE_UNAVAILABLE {
                FallbackReason::WafTlsBound
            } else {
                FallbackReason::SingleUseToken
            };
            Some(ProbeOutcome::FallbackToBrowser {
                reason,
                status_code: status.as_u16(),
                detail: format!(
                    "Origin returned {} during live handoff probe",
                    status.as_u16()
                ),
            })
        }
        StatusCode::GONE => Some(ProbeOutcome::FallbackToBrowser {
            reason: FallbackReason::SingleUseToken,
            status_code: status.as_u16(),
            detail: "Single-use token returned 410 Gone on secondary probe".to_owned(),
        }),
        StatusCode::UNAUTHORIZED | StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            Some(ProbeOutcome::FallbackToBrowser {
                reason: FallbackReason::AuthChallenge,
                status_code: status.as_u16(),
                detail: format!("Origin requested authentication ({})", status.as_u16()),
            })
        }
        _ => None,
    }
}

/// Reads at most `MAX_ERROR_BODY_SNIFF_BYTES` (`1 KiB`) from a non-2xx HTTP error body within
/// `500ms` and strips control characters so error classifiers stay bounded and secret-safe (`P1.6-T03`).
async fn read_bounded_error_snippet(mut body: Incoming) -> String {
    let mut collected = Vec::with_capacity(MAX_ERROR_BODY_SNIFF_BYTES);
    let _ = tokio::time::timeout(ERROR_BODY_SNIFF_TIMEOUT, async {
        while collected.len() < MAX_ERROR_BODY_SNIFF_BYTES {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        let take = (MAX_ERROR_BODY_SNIFF_BYTES - collected.len()).min(data.len());
                        if let Some(slice) = data.get(..take) {
                            collected.extend_from_slice(slice);
                        }
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
    })
    .await;

    let raw = String::from_utf8_lossy(&collected);
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[allow(clippy::too_many_lines)]
fn map_http_status_error(
    status: StatusCode,
    headers: &HeaderMap,
    body_snippet: &str,
    now_unix_ms: i64,
    trace_id: TraceId,
) -> VortexError {
    let lower_body = body_snippet.to_ascii_lowercase();

    // 1. Detect clock skew on signed URLs (`RequestTimeTooSkewed` or `|HTTP Date - Local UTC| > 5 min` — `UX-M07`)!
    if status == StatusCode::FORBIDDEN {
        let body_says_skew = lower_body.contains("requesttimetooskewed")
            || lower_body.contains("clock skew")
            || lower_body.contains("time too skewed");
        let date_skew_secs = headers
            .get(DATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_http_date_epoch_secs)
            .filter(|_| now_unix_ms > 1_500_000_000_000)
            .map(|srv_secs| (srv_secs - (now_unix_ms / 1_000)).abs())
            .filter(|&delta| delta > CLOCK_SKEW_THRESHOLD_SECS);

        if body_says_skew || date_skew_secs.is_some() {
            let mins = date_skew_secs.map_or(5, |s| (s / 60).max(5));
            return VortexError::new(
                ApiError::new(ErrorCode::AuthClockSkew, trace_id)
                    .with_param("skewMinutes", mins.to_string())
                    .with_technical_detail(format!(
                        "HTTP 403 signed URL rejected due to system clock skew (~{mins} min)"
                    )),
            );
        }
    }

    // 2. Detect explicit host concurrency-cap rejection (`VTX-RES-005` — `Host-Resilience-And-Contract-Integrity-Spec.md`)!
    if matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) && (lower_body.contains("too_many_connections")
        || lower_body.contains("too many connections")
        || lower_body.contains("concurrent connection")
        || lower_body.contains("simultaneous connection")
        || lower_body.contains("parallel connection"))
    {
        return VortexError::coded(
            ErrorCode::ResConcurrencyCapped,
            trace_id,
            format!("HTTP {} host concurrency limit reached", status.as_u16()),
        );
    }

    // 3. Detect host CAPTCHA / free-tier quota limit (`VTX-RES-006` — Pixeldrain, Cloudflare, etc.)!
    let has_cf_challenge = headers.contains_key("cf-mitigated");
    if matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) && (has_cf_challenge
        || lower_body.contains("file_rate_limited_captcha_required")
        || lower_body.contains("captcha")
        || lower_body.contains("bandwidth_limit")
        || lower_body.contains("bandwidth limit")
        || lower_body.contains("transfer quota")
        || lower_body.contains("quota exceeded")
        || lower_body.contains("download limit"))
    {
        return VortexError::coded(
            ErrorCode::ResQuotaOrCaptcha,
            trace_id,
            format!(
                "HTTP {} host requested CAPTCHA verification or enforced free download quota",
                status.as_u16()
            ),
        );
    }

    match status {
        StatusCode::NOT_FOUND => {
            VortexError::coded(ErrorCode::ResNotFound, trace_id, "HTTP 404 Not Found")
        }
        StatusCode::FORBIDDEN | StatusCode::GONE => VortexError::coded(
            ErrorCode::ResLinkExpired,
            trace_id,
            format!("HTTP {} link expired or forbidden", status.as_u16()),
        ),
        StatusCode::UNAUTHORIZED | StatusCode::PROXY_AUTHENTICATION_REQUIRED => VortexError::coded(
            ErrorCode::AuthRequired,
            trace_id,
            format!("HTTP {} authentication required", status.as_u16()),
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_ms = parse_retry_after_ms(headers, now_unix_ms);
            VortexError::new(
                ApiError::new(ErrorCode::NetRateLimited, trace_id)
                    .with_recoverability(Recoverability::Transient {
                        retry_after_ms: retry_ms,
                    })
                    .with_technical_detail(format!(
                        "HTTP 429 Too Many Requests (retry_after_ms={retry_ms:?})"
                    )),
            )
        }
        s if s.is_server_error() => {
            let retry_ms = parse_retry_after_ms(headers, now_unix_ms);
            VortexError::new(
                ApiError::new(ErrorCode::NetConnectionLost, trace_id)
                    .with_recoverability(Recoverability::Transient {
                        retry_after_ms: retry_ms,
                    })
                    .with_technical_detail(format!("HTTP {} server error", s.as_u16())),
            )
        }
        other => VortexError::coded(
            ErrorCode::ResNotFound,
            trace_id,
            format!("Unexpected HTTP status {}", other.as_u16()),
        ),
    }
}

fn ensure_identity_encoding(headers: &HeaderMap, trace_id: TraceId) -> AppResult<()> {
    if let Some(enc) = headers.get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()) {
        let trimmed = enc.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("identity") {
            return Err(VortexError::coded(
                ErrorCode::NetMalformedResponse,
                trace_id,
                format!(
                    "Server forced Content-Encoding `{trimmed}` despite `Accept-Encoding: identity`"
                ),
            ));
        }
    }
    Ok(())
}

fn clean_mime(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

fn origin_key_from_url(url: &Url, trace_id: TraceId) -> AppResult<OriginKey> {
    let Some(host) = url.host_str() else {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "URL missing host",
        ));
    };
    let Some(port) = url.port_or_known_default() else {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "URL missing known port",
        ));
    };
    Ok(OriginKey {
        scheme: url.scheme().to_ascii_lowercase(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn total_header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| {
            name.as_str()
                .len()
                .saturating_add(value.len())
                .saturating_add(4)
        })
        .sum()
}

#[allow(clippy::too_many_arguments)]
fn build_http_request(
    url: &Url,
    origin: &OriginKey,
    req: &ResourceRequest,
    send_credentials: bool,
    range_header: &str,
    if_range_header: Option<&str>,
    is_http2: bool,
    trace_id: TraceId,
) -> AppResult<Request<Empty<Bytes>>> {
    let mut builder = Request::builder().method(Method::GET);

    if is_http2 {
        // RFC 9113 HTTP/2 derives `:scheme`, `:authority`, and `:path` from the full URI (no duplicate `Host` header!)
        builder = builder.uri(url.as_str());
    } else {
        let path_and_query = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let host_header = match (origin.scheme.as_str(), origin.port) {
            ("http", 80) | ("https", 443) => origin.host.clone(),
            _ => format!("{}:{}", origin.host, origin.port),
        };
        builder = builder.uri(path_and_query).header(HOST, host_header);
    }

    builder = builder
        .header(RANGE, range_header)
        // Mandatory (`§8.2` & `§11.3`): prevents transparent gzip compression corrupting byte offsets!
        .header(ACCEPT_ENCODING, "identity");

    let has_custom_accept = req.headers.keys().any(|k| k.eq_ignore_ascii_case("accept"));
    if !has_custom_accept {
        builder = builder.header(ACCEPT, "*/*");
    }

    if let Some(if_range_val) = if_range_header {
        builder = builder.header(IF_RANGE, if_range_val);
    }

    let ua = req
        .user_agent
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Vortex/0.1");
    builder = builder.header(USER_AGENT, ua);

    if let Some(refer) = req.referrer.as_deref().filter(|s| !s.is_empty()) {
        builder = builder.header(REFERER, refer);
    }

    for (k, v) in &req.headers {
        let lower = k.to_ascii_lowercase();
        if !send_credentials && (lower == "cookie" || lower == "authorization") {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            builder = builder.header(hn, hv);
        }
    }

    if send_credentials {
        if let Some(cookie_sec) = &req.cookies
            && !cookie_sec.expose_secret().is_empty()
            && let Ok(hv) = HeaderValue::from_str(cookie_sec.expose_secret())
        {
            builder = builder.header(COOKIE, hv);
        }
        if let Some(auth_sec) = &req.authorization
            && !auth_sec.expose_secret().is_empty()
            && let Ok(hv) = HeaderValue::from_str(auth_sec.expose_secret())
        {
            builder = builder.header(AUTHORIZATION, hv);
        }
    }

    builder.body(Empty::new()).map_err(|e| {
        VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!("Invalid HTTP request headers: {e}"),
        )
    })
}
