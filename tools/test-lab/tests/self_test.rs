//! Comprehensive self-test suite for `tools/test-lab` (`P0-T15` Exit Gate).
//!
//! Exercises every standard and hostile HTTP server behavior required by Phase 1–4 integration tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use http_body_util::BodyExt;
use hyper::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG,
    IF_RANGE, LOCATION, RANGE, RETRY_AFTER,
};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use vortex_test_lab::{TestLabServer, deterministic_bytes, deterministic_sha256};

fn http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, String> {
    Client::builder(TokioExecutor::new()).build_http()
}

#[tokio::test]
async fn standard_seeded_files_zero_byte_and_range_sha256() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    // 0-byte file
    let zero_resp = client
        .request(
            Request::get(format!("{}/files/0?seed=7", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(zero_resp.status(), StatusCode::OK);
    let zero_body = zero_resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(zero_body.len(), 0);

    // 128 KiB file split into two Range requests and verified against deterministic SHA-256
    let size = 128 * 1024_u64;
    let first = client
        .request(
            Request::get(format!("{}/files/{size}?seed=99", lab.base_url()))
                .header(RANGE, "bytes=0-65535")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        first.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 0-65535/131072"
    );
    let etag = first
        .headers()
        .get(ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let p1 = first.into_body().collect().await.unwrap().to_bytes();

    // Second half with matching Strong ETag in If-Range -> 206
    let second = client
        .request(
            Request::get(format!("{}/files/{size}?seed=99", lab.base_url()))
                .header(RANGE, "bytes=65536-")
                .header(IF_RANGE, etag)
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    let p2 = second.into_body().collect().await.unwrap().to_bytes();

    let mut hasher = Sha256::new();
    hasher.update(&p1);
    hasher.update(&p2);
    assert_eq!(
        hex::encode(hasher.finalize()),
        deterministic_sha256(99, size)
    );
}

#[tokio::test]
async fn hostile_weak_etag_cloudflare_ja4_and_single_use_token() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    // 1. Weak ETag trap (RFC 9110 §13.1.5): sending Weak ETag in If-Range returns 200 instead of 206
    let probe = client
        .request(
            Request::get(format!("{}/weak-etag/4096", lab.base_url()))
                .header(RANGE, "bytes=0-1023")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(probe.status(), StatusCode::PARTIAL_CONTENT);
    let weak_etag = probe
        .headers()
        .get(ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(weak_etag.starts_with("W/"));

    let trap_resp = client
        .request(
            Request::get(format!("{}/weak-etag/4096", lab.base_url()))
                .header(RANGE, "bytes=1024-2047")
                .header(IF_RANGE, weak_etag)
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(trap_resp.status(), StatusCode::OK);

    // 2. Cloudflare JA4 lock -> 403 + cf-mitigated: challenge
    let waf = client
        .request(
            Request::get(format!("{}/waf-ja4/4096", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(waf.status(), StatusCode::FORBIDDEN);
    assert_eq!(waf.headers().get("cf-mitigated").unwrap(), "challenge");

    // 3. Single-use token: 1st request 206, 2nd request 410 Gone
    let first = client
        .request(
            Request::get(format!("{}/single-use/tok-abc/2048", lab.base_url()))
                .header(RANGE, "bytes=0-")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);

    let second = client
        .request(
            Request::get(format!("{}/single-use/tok-abc/2048", lab.base_url()))
                .header(RANGE, "bytes=0-")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::GONE);
}

#[tokio::test]
async fn hostile_broken_http_behaviors() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    // No-Range server replies 200 despite Range header
    let no_range = client
        .request(
            Request::get(format!("{}/no-range/1024", lab.base_url()))
                .header(RANGE, "bytes=0-100")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_range.status(), StatusCode::OK);

    // Bad Content-Range
    let bad_cr = client
        .request(
            Request::get(format!("{}/bad-content-range/1024", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_cr.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        bad_cr.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 999999-1000000/500"
    );

    // Chunked without Content-Length
    let chunked = client
        .request(
            Request::get(format!("{}/chunked/2048", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chunked.status(), StatusCode::OK);
    assert!(chunked.headers().get(CONTENT_LENGTH).is_none());
    assert_eq!(
        chunked
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len(),
        2048
    );

    // Forced gzip despite Accept-Encoding: identity
    let gzip_resp = client
        .request(
            Request::get(format!("{}/forced-gzip/1024", lab.base_url()))
                .header(ACCEPT_ENCODING, "identity")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(gzip_resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");

    // Abrupt drop mid-stream yields body error
    let drop_resp = client
        .request(
            Request::get(format!("{}/drop-after/256/4096", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(drop_resp.into_body().collect().await.is_err());
}

#[tokio::test]
async fn hostile_rate_limit_expiring_and_mutating_etag() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    // 429 with Retry-After then 200
    let rl1 = client
        .request(
            Request::get(format!(
                "{}/rate-limit/512?fail_first=1&retry_after=3",
                lab.base_url()
            ))
            .body(String::new())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rl1.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rl1.headers().get(RETRY_AFTER).unwrap(), "3");

    let rl2 = client
        .request(
            Request::get(format!(
                "{}/rate-limit/512?fail_first=1&retry_after=3",
                lab.base_url()
            ))
            .body(String::new())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rl2.status(), StatusCode::OK);

    // Expiring URL: alive initially, then 403 after TTL
    let exp1 = client
        .request(
            Request::get(format!("{}/expiring/url1/512?ttl_ms=40", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exp1.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let exp2 = client
        .request(
            Request::get(format!("{}/expiring/url1/512?ttl_ms=40", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exp2.status(), StatusCode::FORBIDDEN);

    // Mutating ETag mid-download
    let m1 = client
        .request(
            Request::get(format!("{}/mutating-etag/doc1/512", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag1 = m1.headers().get(ETAG).unwrap().to_str().unwrap().to_owned();
    let _ = lab.mutate_resource("doc1");
    let m2 = client
        .request(
            Request::get(format!("{}/mutating-etag/doc1/512", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag2 = m2.headers().get(ETAG).unwrap().to_str().unwrap().to_owned();
    assert_ne!(etag1, etag2);
}

#[tokio::test]
async fn hostile_redirects_ssrf_and_arabic_filenames() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    // Redirect chain, loop, and SSRF targets
    let redir_ssrf = client
        .request(
            Request::get(format!("{}/redirect/ssrf", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(redir_ssrf.status(), StatusCode::FOUND);
    assert_eq!(
        redir_ssrf.headers().get(LOCATION).unwrap(),
        "http://127.0.0.1:8080/admin"
    );

    let redir_loop = client
        .request(
            Request::get(format!("{}/redirect/loop", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        redir_loop.headers().get(LOCATION).unwrap(),
        "/redirect/loop"
    );

    // Arabic RFC 5987 & RFC 2047 filenames
    let fn5987 = client
        .request(
            Request::get(format!("{}/filename/rfc5987", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        fn5987
            .headers()
            .get(CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("filename*=UTF-8''")
    );

    let fn2047 = client
        .request(
            Request::get(format!("{}/filename/rfc2047", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        fn2047
            .headers()
            .get(CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("=?UTF-8?B?")
    );
}

#[tokio::test]
async fn strict_zero_416_and_cross_origin_redirect_modes() {
    let lab = TestLabServer::spawn().await.unwrap();
    let client = http_client();

    let z416 = client
        .request(
            Request::get(format!("{}/files/0?strict_zero_416=true", lab.base_url()))
                .header(RANGE, "bytes=0-")
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(z416.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(z416.headers().get(CONTENT_RANGE).unwrap(), "bytes */0");

    let xorig = client
        .request(
            Request::get(format!("{}/redirect/cross-origin", lab.base_url()))
                .body(String::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(xorig.status(), StatusCode::FOUND);
}

proptest! {
    #[test]
    fn prop_word_aligned_splitmix64_is_invariant_across_unaligned_splits(
        seed in any::<u64>(),
        total in 1_usize..512,
        cut1 in 0_usize..512,
        cut2 in 0_usize..512,
    ) {
        let a = cut1.min(total);
        let b = cut2.min(total).max(a);
        let whole = deterministic_bytes(seed, 0, total);
        let mut joined = deterministic_bytes(seed, 0, a);
        joined.extend_from_slice(&deterministic_bytes(seed, a as u64, b - a));
        joined.extend_from_slice(&deterministic_bytes(seed, b as u64, total - b));
        prop_assert_eq!(whole, joined);
    }
}
