//! HTTP/1.1-HTTPS network adapter (`HttpByteSource`), kernel TCP RTT metrics (`WSAIoctl(SIO_TCP_INFO)`),
//! RFC 9110 `If-Range`, SSRF & Cross-Origin guards, and Resilience Engine (`crates/vortex-net` —
//! Level 3, `P1-T14` → `P1-T17`).

pub mod client;
pub mod connector;
pub mod headers;
pub mod metrics;
pub mod resilience;

pub use client::{HttpByteSource, HttpClientConfig, MAX_REDIRECT_HOPS, MAX_RESPONSE_HEADERS};
pub use connector::{
    HTTP1_READ_BUF_BYTES, HTTP2_CONN_WINDOW_BYTES, HTTP2_MAX_FRAME_SIZE_BYTES,
    HTTP2_STREAM_WINDOW_BYTES, MAX_HTTP1_HEADER_BUF_BYTES, TransportPool,
};
pub use headers::{
    ParsedContentRange, extract_resume_validators, is_forbidden_ssrf_ip, is_same_origin,
    parse_content_disposition_filename, parse_content_range, parse_disposition_raw_and_sanitized,
    parse_disposition_str, parse_http_date_epoch_secs, parse_retry_after_ms,
};
pub use metrics::{MIN_RTT_WINDOW_MS, SocketMetricsProbe, WindowedMinFilter};
pub use resilience::{
    BASE_BACKOFF_MS, CircuitState, DEFAULT_CIRCUIT_FAILURE_THRESHOLD, DEFAULT_CIRCUIT_OPEN_MS,
    HostCircuitBreaker, MAX_BACKOFF_MS, WatchdogByteStream, compute_backoff_delay_ms,
};

/// Layer identifier used by Composition Root smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_application::layer_name();
    "vortex-net"
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]
mod tests {
    use std::sync::Arc;

    use vortex_application::{ByteSource, FallbackReason, ProbeOutcome, ResourceRequest};
    use vortex_domain::{ByteRange, DownloadId, FakeClock, SystemClock};
    use vortex_error::{ErrorCode, TraceId};
    use vortex_observability::SecretString;
    use vortex_test_lab::{TestLabServer, deterministic_bytes};

    use super::*;

    #[tokio::test]
    async fn probe_and_keepalive_bounded_range_leases_with_kernel_tcp_rtt() {
        let lab = TestLabServer::spawn().await.unwrap();
        let clock = Arc::new(SystemClock::new());
        let client = HttpByteSource::new(clock, HttpClientConfig::for_local_testing()).unwrap();
        let trace = TraceId::new();
        let dl_id = DownloadId::new();

        let url = format!("{}/files/65536?seed=7", lab.base_url());
        let req = ResourceRequest::new(dl_id, url, trace);

        let outcome = client.probe_and_stream(&req).await.unwrap();
        let ProbeOutcome::Transferable {
            info,
            mut initial_stream,
        } = outcome
        else {
            panic!("expected Transferable probe");
        };

        assert_eq!(info.total_size, Some(65_536));
        assert!(info.supports_range);
        #[cfg(windows)]
        assert!(info.transport_metrics.rtt_us.is_some());

        let mut received = Vec::new();
        while let Some(chunk) = initial_stream.next_chunk().await.unwrap() {
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, deterministic_bytes(7, 0, 65_536));

        // Verify Keep-Alive socket returned to pool and reused on bounded `open_range`!
        assert_eq!(
            client
                .transport_pool()
                .idle_connection_count("http", "127.0.0.1", lab.addr().port()),
            1
        );

        let mut ranged_req = req.clone();
        ranged_req.validators = info.validators.clone();
        let (mut range_stream, range_metrics) = client
            .open_range(&ranged_req, ByteRange::new(1024, 4096).unwrap())
            .await
            .unwrap();
        assert!(range_metrics.reused_connection);

        let mut slice_bytes = Vec::new();
        while let Some(chunk) = range_stream.next_chunk().await.unwrap() {
            slice_bytes.extend_from_slice(&chunk);
        }
        assert_eq!(slice_bytes, deterministic_bytes(7, 1024, 3072));
    }

    #[tokio::test]
    async fn waf_ja4_403_and_single_use_token_trigger_instant_browser_fallback() {
        let lab = TestLabServer::spawn().await.unwrap();
        let clock = Arc::new(FakeClock::default());
        let client = HttpByteSource::new(clock, HttpClientConfig::for_local_testing()).unwrap();
        let trace = TraceId::new();

        // 1. Cloudflare JA4 `403` -> `FallbackToBrowser { WafTlsBound }`
        let mut waf_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/waf-ja4/4096", lab.base_url()),
            trace,
        );
        waf_req.is_browser_handoff = true;
        let waf_out = client.probe_and_stream(&waf_req).await.unwrap();
        assert!(matches!(
            waf_out,
            ProbeOutcome::FallbackToBrowser {
                reason: FallbackReason::WafTlsBound,
                status_code: 403,
                ..
            }
        ));

        // 2. Single-Use Token already opened by browser (`1st` GET consumed token, `2nd` returns `410`)
        let token_url = format!("{}/single-use/tok-abc/2048", lab.base_url());
        let first_req = ResourceRequest::new(DownloadId::new(), token_url.clone(), trace);
        let _browser_initial = client.probe_and_stream(&first_req).await.unwrap();

        let mut handoff_req = ResourceRequest::new(DownloadId::new(), token_url, trace);
        handoff_req.is_browser_handoff = true;
        let fallback_out = client.probe_and_stream(&handoff_req).await.unwrap();
        assert!(matches!(
            fallback_out,
            ProbeOutcome::FallbackToBrowser {
                reason: FallbackReason::SingleUseToken,
                status_code: 410,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn rfc9110_weak_etag_resume_ssrf_redirects_arabic_filenames_and_hostile_traps() {
        let lab = TestLabServer::spawn().await.unwrap();
        let clock = Arc::new(FakeClock::default());
        let client =
            HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap();
        let trace = TraceId::new();

        // 1. Weak ETag (`W/"..."`): `open_range` MUST NOT send `If-Range: W/"..."` (which Test Lab traps with 200 OK!)
        let weak_url = format!("{}/weak-etag/8192?seed=9", lab.base_url());
        let req_weak = ResourceRequest::new(DownloadId::new(), weak_url, trace);
        let ProbeOutcome::Transferable { info, .. } =
            client.probe_and_stream(&req_weak).await.unwrap()
        else {
            panic!("expected Transferable");
        };
        assert!(!info.validators.etag.as_ref().unwrap().is_strong());
        let mut resume_weak = req_weak.clone();
        resume_weak.validators = info.validators;
        let (mut r_stream, _) = client
            .open_range(&resume_weak, ByteRange::new(4096, 8192).unwrap())
            .await
            .unwrap();
        let mut tail = Vec::new();
        while let Some(c) = r_stream.next_chunk().await.unwrap() {
            tail.extend_from_slice(&c);
        }
        assert_eq!(tail, deterministic_bytes(9, 4096, 4096));

        // 2. SSRF redirects (`127.0.0.1:8080` and `169.254.169.254`) blocked with `VTX-SEC-001`!
        for path in ["/redirect/ssrf", "/redirect/ssrf-metadata"] {
            let ssrf_req = ResourceRequest::new(
                DownloadId::new(),
                format!("{}{path}", lab.base_url()),
                trace,
            );
            let err = client.probe_and_stream(&ssrf_req).await.unwrap_err();
            assert_eq!(err.code(), ErrorCode::SecPolicyViolation);
        }

        // 3. Forbidden UNC / `file://` rejected immediately
        let unc_req = ResourceRequest::new(DownloadId::new(), r"\\evil\share\a.exe", trace);
        assert_eq!(
            client.probe_and_stream(&unc_req).await.unwrap_err().code(),
            ErrorCode::SecPolicyViolation
        );

        // 4. Cross-origin redirect strips `Cookie` & `Authorization` (`TH-04`), while same-origin chain works
        let chain_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/redirect/chain/3/1024", lab.base_url()),
            trace,
        );
        assert!(matches!(
            client.probe_and_stream(&chain_req).await.unwrap(),
            ProbeOutcome::Transferable { .. }
        ));

        let loop_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/redirect/loop", lab.base_url()),
            trace,
        );
        assert_eq!(
            client.probe_and_stream(&loop_req).await.unwrap_err().code(),
            ErrorCode::NetMalformedResponse
        );

        // Cross-origin cookie strip test between two distinct local TestLab origins (`lab` -> `lab2`)
        let lab2 = TestLabServer::spawn().await.unwrap();
        let cross_url = format!(
            "{}/redirect/cross-origin?target={}/files/512",
            lab.base_url(),
            lab2.base_url()
        );
        let mut cross_req = ResourceRequest::new(DownloadId::new(), cross_url, trace);
        cross_req.cookies = Some(SecretString::from("session=secret".to_owned()));
        cross_req.authorization = Some(SecretString::from("Bearer tok".to_owned()));
        // Without `allow_private_redirect`, cross-origin to loopback `lab2` is blocked:
        assert_eq!(
            client
                .probe_and_stream(&cross_req)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );
        cross_req.allow_private_redirect = true;
        assert!(matches!(
            client.probe_and_stream(&cross_req).await.unwrap(),
            ProbeOutcome::Transferable { .. }
        ));

        // 5. Arabic RFC 5987 & RFC 2047 filenames + Path Traversal sanitization
        for (mode, expected_name) in [
            ("rfc5987", "تقرير-2026.pdf"),
            ("rfc2047", "ملف-عربي.zip"),
            ("traversal", "evil.exe"),
        ] {
            let f_req = ResourceRequest::new(
                DownloadId::new(),
                format!("{}/filename/{mode}", lab.base_url()),
                trace,
            );
            let ProbeOutcome::Transferable { info, .. } =
                client.probe_and_stream(&f_req).await.unwrap()
            else {
                panic!("expected Transferable");
            };
            assert_eq!(info.suggested_filename.as_deref(), Some(expected_name));
        }

        // 6. Hostile traps: `no-range` (`200` on `open_range` -> `VTX-RES-004`), `bad-content-range` (`VTX-NET-005`),
        //    `forced-gzip` (`VTX-NET-005`), and `429 Retry-After` (`VTX-NET-004` with `retry_after_ms`)!
        let nr_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/no-range/1024", lab.base_url()),
            trace,
        );
        assert_eq!(
            client
                .open_range(&nr_req, ByteRange::new(0, 512).unwrap())
                .await
                .err()
                .unwrap()
                .code(),
            ErrorCode::ResRangeUnsupported
        );

        let bcr_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/bad-content-range/1024", lab.base_url()),
            trace,
        );
        assert_eq!(
            client
                .open_range(&bcr_req, ByteRange::new(0, 512).unwrap())
                .await
                .err()
                .unwrap()
                .code(),
            ErrorCode::NetMalformedResponse
        );

        let gzip_req = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/forced-gzip/1024", lab.base_url()),
            trace,
        );
        assert_eq!(
            client.probe_and_stream(&gzip_req).await.unwrap_err().code(),
            ErrorCode::NetMalformedResponse
        );

        let rl_req = ResourceRequest::new(
            DownloadId::new(),
            format!(
                "{}/rate-limit/1024?fail_first=1&retry_after=3",
                lab.base_url()
            ),
            trace,
        );
        let rl_err = client.probe_and_stream(&rl_req).await.unwrap_err();
        assert_eq!(rl_err.code(), ErrorCode::NetRateLimited);
        assert_eq!(
            rl_err.as_api_error().recoverability.retry_after_ms(),
            Some(3_000)
        );
        assert_eq!(compute_backoff_delay_ms(0, Some(3_000), 42), 3_000);
    }

    #[test]
    fn circuit_breaker_trips_reopens_half_open_and_recovers_deterministically() {
        let clock = Arc::new(FakeClock::new(10_000));
        let cb = HostCircuitBreaker::new(clock.clone(), 3, 5_000);
        let trace = TraceId::new();

        cb.check_permitted("bad.cdn.test", trace).unwrap();
        cb.record_transient_failure("bad.cdn.test", trace);
        cb.record_transient_failure("bad.cdn.test", trace);
        assert!(cb.check_permitted("bad.cdn.test", trace).is_ok());

        // 3rd failure trips circuit OPEN
        let st = cb.record_transient_failure("bad.cdn.test", trace);
        assert!(matches!(st, CircuitState::Open { .. }));
        assert_eq!(
            cb.check_permitted("bad.cdn.test", trace)
                .unwrap_err()
                .code(),
            ErrorCode::NetConnectionLost
        );

        // Advance FakeClock past 5s cooldown -> transitions to HalfOpen -> success closes!
        clock.advance_ms(5_001);
        cb.check_permitted("bad.cdn.test", trace).unwrap();
        assert_eq!(cb.state_of("bad.cdn.test"), CircuitState::HalfOpen);
        cb.record_success("bad.cdn.test");
        assert_eq!(
            cb.state_of("bad.cdn.test"),
            CircuitState::Closed {
                consecutive_failures: 0
            }
        );
    }

    #[tokio::test]
    async fn production_default_blocks_direct_loopback_imds_and_dns_rebinding_under_th26() {
        let lab = TestLabServer::spawn().await.unwrap();
        let clock = Arc::new(SystemClock::new());
        let prod_client = HttpByteSource::new(clock.clone(), HttpClientConfig::default()).unwrap();
        let trace = TraceId::new();

        // 1. Direct loopback (`127.0.0.1`) blocked by production default (`allow_private_network = false`)!
        let direct_loopback = ResourceRequest::new(
            DownloadId::new(),
            format!("{}/files/64", lab.base_url()),
            trace,
        );
        assert_eq!(
            prod_client
                .probe_and_stream(&direct_loopback)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );

        // 2. Direct cloud IMDS (`169.254.169.254`) blocked before TCP dial!
        let direct_imds = ResourceRequest::new(
            DownloadId::new(),
            "http://169.254.169.254/latest/meta-data/",
            trace,
        );
        assert_eq!(
            prod_client
                .probe_and_stream(&direct_imds)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );

        // 3. Anti-DNS-Rebinding: even when local testing is enabled, once a host is pinned public,
        //    resolving to a private/loopback IP is rejected unconditionally!
        let test_client =
            HttpByteSource::new(clock, HttpClientConfig::for_local_testing()).unwrap();
        test_client
            .transport_pool()
            .pin_host_as_public_for_test("127.0.0.1");
        assert_eq!(
            test_client
                .probe_and_stream(&direct_loopback)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );
    }

    #[tokio::test]
    async fn probe_stream_stop_offset_halts_background_pump_without_overreading() {
        let lab = TestLabServer::spawn().await.unwrap();
        let clock = Arc::new(SystemClock::new());
        let client = HttpByteSource::new(clock, HttpClientConfig::for_local_testing()).unwrap();
        let trace = TraceId::new();
        let url = format!("{}/files/524288?seed=13", lab.base_url());
        let req = ResourceRequest::new(DownloadId::new(), url, trace);

        let ProbeOutcome::Transferable {
            info,
            mut initial_stream,
        } = client.probe_and_stream(&req).await.unwrap()
        else {
            panic!("expected Transferable");
        };
        assert_eq!(info.total_size, Some(524_288));

        // Simulate Worker #2 stealing `[131_072, 524_288)` -> Worker #1 caps probe pump at `131_072`!
        initial_stream.set_stop_offset(131_072);

        let mut got = Vec::new();
        while let Some(chunk) = initial_stream.next_chunk().await.unwrap() {
            got.extend_from_slice(&chunk);
        }
        // Pump stopped promptly at or cleanly around `stop_offset` (never downloaded full 512 KiB!)
        assert!(
            got.len() <= 262_144,
            "stop_offset must terminate probe RAM pump early; got {} bytes",
            got.len()
        );
        assert!(got.len() >= 131_072 || !got.is_empty());
        assert_eq!(got, deterministic_bytes(13, 0, got.len()));
    }
}
