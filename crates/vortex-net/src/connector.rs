//! DNS caching (`hickory-resolver`), Multi-IP CDN Round-Robin Striping + Slow-IP Quarantine
//! (`P1.7-T05`), Happy Eyeballs v2 (`RFC 8305`), `socket2` (`TCP_NODELAY` with untouched Windows
//! TCP Auto-Tuning `SO_RCVBUF`), `rustls` TLS 1.3 Session Resumption (`ALPN: ["h2", "http/1.1"]`),
//! `512 KiB` HTTP/1.1 Read Buffer (`P1.7-T03`), and High-BDP Multi-Connection `HTTP/2` Pool
//! (`16/64 MiB` windows — `P1.7-T04` / `ADR-021`).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hickory_resolver::TokioResolver;
use http::Request;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustc_hash::FxHashMap;
use rustls::ClientConfig;
use rustls::client::Resumption;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use vortex_application::AppResult;
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::headers::is_forbidden_ssrf_ip;
use crate::metrics::{SocketMetricsProbe, WindowedMinFilter};

/// RFC 8305 Happy Eyeballs v2 connection attempt delay (`250ms`).
const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);

/// Maximum idle Keep-Alive HTTP/1.1 sockets retained per origin (`32`).
const MAX_IDLE_PER_ORIGIN: usize = 32;

/// Maximum independent TCP+TLS+H2 connections per origin (defeats TCP `HoL` blocking & single-socket policing!).
const MAX_H2_CONNS_PER_ORIGIN: usize = 8;

/// Target max concurrent streams per H2 connection before opening a parallel H2 socket.
const TARGET_STREAMS_PER_H2_CONN: usize = 1;

/// Duration (`60s`) an IP failing TCP connect is quarantined during Multi-IP CDN Striping (`P1.7-T05`).
const SLOW_IP_QUARANTINE_DURATION: Duration = Duration::from_secs(60);

/// Strict HTTP response header byte ceiling (`64 KiB` — `TH-27` Header-Bomb defense, enforced on
/// headers only without throttling the `512 KiB` body read buffer!).
pub const MAX_HTTP1_HEADER_BUF_BYTES: usize = 64 * 1024;

/// High-throughput HTTP/1.1 socket read buffer (`512 KiB` — `P1.7-T03` / `ADR-021`).
pub const HTTP1_READ_BUF_BYTES: usize = 512 * 1024;

/// High-BDP HTTP/2 per-stream flow-control window (`16 MiB` — saturates 1 Gbps up to 128ms RTT!).
pub const HTTP2_STREAM_WINDOW_BYTES: u32 = 16 * 1024 * 1024;

/// High-BDP HTTP/2 connection-level flow-control window (`64 MiB`).
pub const HTTP2_CONN_WINDOW_BYTES: u32 = 64 * 1024 * 1024;

/// HTTP/2 max frame size (`256 KiB` when peer permits, reducing framing overhead vs 16 KiB default).
pub const HTTP2_MAX_FRAME_SIZE_BYTES: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OriginKey {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

pub(crate) struct PooledHttp1Conn {
    pub sender: http1::SendRequest<Empty<Bytes>>,
    pub metrics_probe: SocketMetricsProbe,
}

#[derive(Clone)]
pub(crate) struct PooledHttp2Conn {
    pub sender: http2::SendRequest<Empty<Bytes>>,
    pub metrics_probe: SocketMetricsProbe,
    pub active_streams: Arc<AtomicUsize>,
}

/// Active checked-out HTTP/1.1 or HTTP/2 connection.
pub(crate) enum ActiveHttpConn {
    Http1(PooledHttp1Conn),
    Http2(PooledHttp2Conn),
}

impl ActiveHttpConn {
    pub(crate) fn metrics_probe(&self) -> &SocketMetricsProbe {
        match self {
            Self::Http1(c) => &c.metrics_probe,
            Self::Http2(c) => &c.metrics_probe,
        }
    }

    pub(crate) async fn send_request(
        &mut self,
        req: Request<Empty<Bytes>>,
    ) -> Result<http::Response<Incoming>, hyper::Error> {
        match self {
            Self::Http1(c) => c.sender.send_request(req).await,
            Self::Http2(c) => c.sender.send_request(req).await,
        }
    }
}

/// Shared connector + HTTP/1.1 Keep-Alive & High-BDP Multi-Connection HTTP/2 pool preserving warm
/// sockets across bounded chunk leases (`ADR-009` / `ADR-021`), with `ALPN: ["h2", "http/1.1"]`,
/// Multi-IP CDN Round-Robin Striping (`P1.7-T05`), and Anti-DNS-Rebinding pinning (`TH-26`).
#[derive(Clone)]
pub struct TransportPool {
    resolver: Arc<TokioResolver>,
    tls_connector: TlsConnector,
    enable_http2: bool,
    http2_prior_knowledge_for_test: bool,
    idle_http1_pool: Arc<Mutex<FxHashMap<OriginKey, Vec<PooledHttp1Conn>>>>,
    active_http2_pool: Arc<Mutex<FxHashMap<OriginKey, Vec<PooledHttp2Conn>>>>,
    host_min_filters: Arc<Mutex<FxHashMap<String, Arc<Mutex<WindowedMinFilter>>>>>,
    /// Tracks hosts first seen resolving to global public IPs so a DNS rebind to `127.0.0.1`/RFC1918
    /// on later `open_range` workers is unconditionally rejected (`TH-26`).
    public_pinned_hosts: Arc<Mutex<FxHashMap<String, bool>>>,
    /// Round-robin IP striping cursor per host (`P1.7-T05`).
    host_rr_cursors: Arc<Mutex<FxHashMap<String, usize>>>,
    /// Temporarily quarantined slow/failing CDN Edge IPs (`60s` TTL — `P1.7-T05`).
    quarantined_ips: Arc<Mutex<FxHashMap<IpAddr, Instant>>>,
    connect_timeout: Duration,
}

impl std::fmt::Debug for TransportPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportPool")
            .field("connect_timeout", &self.connect_timeout)
            .field("enable_http2", &self.enable_http2)
            .finish_non_exhaustive()
    }
}

impl TransportPool {
    /// Initializes [`TransportPool`] with High-BDP `HTTP/2` enabled (`ALPN: ["h2", "http/1.1"]`).
    ///
    /// # Errors
    /// Returns `VTX-NET-003` if initializing the OS TLS certificate verifier fails.
    pub fn new(connect_timeout: Duration) -> AppResult<Self> {
        Self::new_with_options(connect_timeout, true, false)
    }

    /// Initializes [`TransportPool`] with explicit `HTTP/2` and test prior-knowledge options.
    ///
    /// # Errors
    /// Returns `VTX-NET-003` or `VTX-NET-001` if DNS or OS TLS verifier init fails.
    pub fn new_with_options(
        connect_timeout: Duration,
        enable_http2: bool,
        http2_prior_knowledge_for_test: bool,
    ) -> AppResult<Self> {
        let resolver_builder = TokioResolver::builder_tokio().map_err(|e| {
            VortexError::coded(
                ErrorCode::NetDnsFailure,
                TraceId::new(),
                format!("DNS system config init error: {e}"),
            )
        })?;
        let resolver = resolver_builder.build().map_err(|e| {
            VortexError::coded(
                ErrorCode::NetDnsFailure,
                TraceId::new(),
                format!("DNS resolver build error: {e}"),
            )
        })?;

        let crypto_provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls_cfg: ClientConfig = ClientConfig::builder_with_provider(crypto_provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                VortexError::coded(
                    ErrorCode::NetTlsCertificate,
                    TraceId::new(),
                    format!("TLS protocol config error: {e}"),
                )
            })?
            .with_platform_verifier()
            .map_err(|e| {
                VortexError::coded(
                    ErrorCode::NetTlsCertificate,
                    TraceId::new(),
                    format!("Platform TLS certificate verifier init error: {e}"),
                )
            })?
            .with_no_client_auth();

        // Advertise `h2` + `http/1.1` ALPN when HTTP/2 is enabled (real browser fingerprint + High-BDP H2 — `P1.7-T04`),
        // or explicit `http/1.1` when disabled (never empty `ALPN: None`!).
        tls_cfg.alpn_protocols = if enable_http2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };
        // Enable TLS 1.3 Session Tickets + TLS 1.2 session IDs (256 hosts cached in RAM — `§11.2`)
        tls_cfg.resumption = Resumption::in_memory_sessions(256);

        Ok(Self {
            resolver: Arc::new(resolver),
            tls_connector: TlsConnector::from(Arc::new(tls_cfg)),
            enable_http2,
            http2_prior_knowledge_for_test,
            idle_http1_pool: Arc::new(Mutex::new(FxHashMap::default())),
            active_http2_pool: Arc::new(Mutex::new(FxHashMap::default())),
            host_min_filters: Arc::new(Mutex::new(FxHashMap::default())),
            public_pinned_hosts: Arc::new(Mutex::new(FxHashMap::default())),
            host_rr_cursors: Arc::new(Mutex::new(FxHashMap::default())),
            quarantined_ips: Arc::new(Mutex::new(FxHashMap::default())),
            connect_timeout,
        })
    }

    /// Resolves `host` into candidate IP addresses (interleaving IPv6 & IPv4 for Happy Eyeballs v2
    /// and rotating Round-Robin across healthy CDN Edge IPs — `P1.7-T05`), enforcing universal
    /// SSRF & Anti-DNS-Rebinding checks (`TH-04` & `TH-26`).
    pub(crate) async fn resolve_host_ips(
        &self,
        host: &str,
        enforce_public_ip: bool,
        trace_id: TraceId,
    ) -> AppResult<Vec<IpAddr>> {
        let clean_host = host.trim_matches(['[', ']']);
        let normalized_host = clean_host.to_ascii_lowercase();
        let ips: Vec<IpAddr> = if let Ok(literal_ip) = clean_host.parse::<IpAddr>() {
            vec![literal_ip]
        } else {
            let lookup = self.resolver.lookup_ip(clean_host).await.map_err(|e| {
                VortexError::coded(
                    ErrorCode::NetDnsFailure,
                    trace_id,
                    format!("DNS lookup failed for `{clean_host}`: {e}"),
                )
            })?;
            interleave_v6_v4(lookup.iter().collect())
        };

        if ips.is_empty() {
            return Err(VortexError::coded(
                ErrorCode::NetDnsFailure,
                trace_id,
                format!("DNS returned 0 records for `{clean_host}`"),
            ));
        }

        let was_pinned_public = self
            .public_pinned_hosts
            .lock()
            .ok()
            .and_then(|m| m.get(&normalized_host).copied())
            .unwrap_or(false);

        let must_be_public = enforce_public_ip || was_pinned_public;
        let all_public = ips.iter().all(|&ip| !is_forbidden_ssrf_ip(ip));

        if must_be_public
            && !all_public
            && let Some(&bad_ip) = ips.iter().find(|&&ip| is_forbidden_ssrf_ip(ip))
        {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                format!(
                    "SSRF / Anti-DNS-Rebinding guard blocked non-global IP `{bad_ip}` for host `{clean_host}` (TH-04 / TH-26)"
                ),
            ));
        }

        if all_public && let Ok(mut pins) = self.public_pinned_hosts.lock() {
            if pins.len() > 2048 {
                pins.clear();
            }
            pins.insert(normalized_host.clone(), true);
        }

        Ok(self.stripe_and_filter_ips(&normalized_host, ips))
    }

    /// Filters out temporarily quarantined slow/failed IPs and rotates candidates Round-Robin so
    /// parallel workers distribute load across all CDN Edge IPs (`P1.7-T05`).
    pub(crate) fn stripe_and_filter_ips(
        &self,
        normalized_host: &str,
        ips: Vec<IpAddr>,
    ) -> Vec<IpAddr> {
        if ips.len() <= 1 {
            return ips;
        }
        let now = Instant::now();
        let healthy: Vec<IpAddr> = if let Ok(mut q) = self.quarantined_ips.lock() {
            q.retain(|_, &mut until| until > now);
            let filtered: Vec<IpAddr> = ips
                .iter()
                .copied()
                .filter(|ip| !q.contains_key(ip))
                .collect();
            if filtered.is_empty() { ips } else { filtered }
        } else {
            ips
        };

        if healthy.len() <= 1 {
            return healthy;
        }

        let rot = self.host_rr_cursors.lock().map_or(0, |mut cursors| {
            if cursors.len() > 2048 {
                cursors.clear();
            }
            let entry = cursors.entry(normalized_host.to_owned()).or_insert(0);
            let current = *entry;
            *entry = current.wrapping_add(1);
            current % healthy.len()
        });

        let mut rotated = healthy;
        rotated.rotate_left(rot);
        rotated
    }

    /// Quarantines a slow or failing Edge IP for `60s` so parallel workers route around it (`P1.7-T05`).
    pub fn quarantine_ip(&self, ip: IpAddr) {
        if let Ok(mut q) = self.quarantined_ips.lock() {
            if q.len() > 1024 {
                let now = Instant::now();
                q.retain(|_, &mut until| until > now);
            }
            q.insert(ip, Instant::now() + SLOW_IP_QUARANTINE_DURATION);
        }
    }

    /// Marks `host` as public-pinned (used by Anti-DNS-Rebinding tests to verify rebind rejection — `TH-26`).
    pub fn pin_host_as_public_for_test(&self, host: &str) {
        if let Ok(mut pins) = self.public_pinned_hosts.lock() {
            pins.insert(host.trim().to_ascii_lowercase(), true);
        }
    }

    /// Checks out a live warm HTTP/2 or HTTP/1.1 Keep-Alive connection, or dials a fresh socket
    /// striped across CDN Edge IPs (`P1.7-T04` & `P1.7-T05`).
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn checkout_or_connect(
        &self,
        origin: &OriginKey,
        enforce_public_ip: bool,
        trace_id: TraceId,
    ) -> AppResult<ActiveHttpConn> {
        // 1. Try reusing an existing ready HTTP/2 connection when pool already has an under-loaded
        //    socket (or reached `MAX_H2_CONNS_PER_ORIGIN` sockets to balance HoL vs socket count!).
        if self.enable_http2 {
            let h2_candidate = self.checkout_ready_h2(origin);
            if let Some(mut h2) = h2_candidate {
                if h2.sender.ready().await.is_ok() {
                    h2.metrics_probe = h2.metrics_probe.mark_reused(None);
                    return Ok(ActiveHttpConn::Http2(h2));
                }
                h2.active_streams.fetch_sub(1, Ordering::Relaxed);
            }
        }

        // 2. Try reusing an existing ready HTTP/1.1 Keep-Alive connection
        loop {
            let candidate_opt = self
                .idle_http1_pool
                .lock()
                .ok()
                .and_then(|mut map| map.get_mut(origin).and_then(Vec::pop));
            let Some(mut candidate) = candidate_opt else {
                break;
            };
            if candidate.sender.ready().await.is_ok() {
                candidate.metrics_probe = candidate.metrics_probe.mark_reused(None);
                return Ok(ActiveHttpConn::Http1(candidate));
            }
        }

        // 3. Dial fresh TCP (+ optional TLS 1.3 with ALPN `["h2", "http/1.1"]`) via Striped Happy Eyeballs
        let ips = self
            .resolve_host_ips(&origin.host, enforce_public_ip, trace_id)
            .await?;
        let t0 = Instant::now();
        let tcp_stream = self
            .dial_happy_eyeballs(&ips, origin.port, trace_id)
            .await?;
        let connect_rtt_us = u64::try_from(t0.elapsed().as_micros()).ok();

        let min_filter = self.min_filter_for_host(&origin.host);
        let metrics_probe =
            SocketMetricsProbe::from_stream(&tcp_stream, false, false, min_filter, connect_rtt_us);

        if origin.scheme == "https" {
            let server_name = ServerName::try_from(origin.host.trim_matches(['[', ']']).to_owned())
                .map_err(|e| {
                    VortexError::coded(
                        ErrorCode::NetTlsCertificate,
                        trace_id,
                        format!("Invalid TLS SNI host `{}`: {e}", origin.host),
                    )
                })?;

            let tls_stream = tokio::time::timeout(
                self.connect_timeout,
                self.tls_connector.connect(server_name, tcp_stream),
            )
            .await
            .map_err(|_| {
                VortexError::coded(
                    ErrorCode::NetConnectionLost,
                    trace_id,
                    "TLS handshake timed out",
                )
            })?
            .map_err(|tls_err| {
                VortexError::coded(
                    ErrorCode::NetTlsCertificate,
                    trace_id,
                    format!("TLS handshake/certificate verification failed: {tls_err}"),
                )
            })?;

            let negotiated_h2 = self.enable_http2
                && tls_stream
                    .get_ref()
                    .1
                    .alpn_protocol()
                    .is_some_and(|p| p == b"h2");

            if negotiated_h2 {
                let io = TokioIo::new(tls_stream);
                let h2_conn = self
                    .handshake_http2(origin, io, metrics_probe.with_http2(true), trace_id)
                    .await?;
                return Ok(ActiveHttpConn::Http2(h2_conn));
            }

            let io = TokioIo::new(tls_stream);
            let (sender, conn) = http1::Builder::new()
                .preserve_header_case(true)
                .max_buf_size(HTTP1_READ_BUF_BYTES)
                .handshake(io)
                .await
                .map_err(|e| {
                    VortexError::coded(
                        ErrorCode::NetConnectionLost,
                        trace_id,
                        format!("HTTP/1.1 handshake over TLS failed: {e}"),
                    )
                })?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(ActiveHttpConn::Http1(PooledHttp1Conn {
                sender,
                metrics_probe,
            }))
        } else if self.enable_http2 && self.http2_prior_knowledge_for_test {
            let io = TokioIo::new(tcp_stream);
            let h2_conn = self
                .handshake_http2(origin, io, metrics_probe.with_http2(true), trace_id)
                .await?;
            Ok(ActiveHttpConn::Http2(h2_conn))
        } else {
            let io = TokioIo::new(tcp_stream);
            let (sender, conn) = http1::Builder::new()
                .preserve_header_case(true)
                .max_buf_size(HTTP1_READ_BUF_BYTES)
                .handshake(io)
                .await
                .map_err(|e| {
                    VortexError::coded(
                        ErrorCode::NetConnectionLost,
                        trace_id,
                        format!("HTTP/1.1 handshake failed: {e}"),
                    )
                })?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(ActiveHttpConn::Http1(PooledHttp1Conn {
                sender,
                metrics_probe,
            }))
        }
    }

    async fn handshake_http2<T>(
        &self,
        origin: &OriginKey,
        io: TokioIo<T>,
        metrics_probe: SocketMetricsProbe,
        trace_id: TraceId,
    ) -> AppResult<PooledHttp2Conn>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, conn) = http2::Builder::new(TokioExecutor::new())
            .timer(hyper_util::rt::TokioTimer::new())
            .initial_stream_window_size(HTTP2_STREAM_WINDOW_BYTES)
            .initial_connection_window_size(HTTP2_CONN_WINDOW_BYTES)
            .adaptive_window(true)
            .max_frame_size(Some(HTTP2_MAX_FRAME_SIZE_BYTES))
            .max_header_list_size(u32::try_from(MAX_HTTP1_HEADER_BUF_BYTES).unwrap_or(65_536))
            .keep_alive_interval(Some(Duration::from_secs(20)))
            .keep_alive_timeout(Duration::from_secs(10))
            .handshake(io)
            .await
            .map_err(|e| {
                VortexError::coded(
                    ErrorCode::NetConnectionLost,
                    trace_id,
                    format!("HTTP/2 handshake failed: {e}"),
                )
            })?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let active_streams = Arc::new(AtomicUsize::new(1));
        let pooled = PooledHttp2Conn {
            sender,
            metrics_probe,
            active_streams,
        };
        if let Ok(mut map) = self.active_http2_pool.lock() {
            let list = map.entry(origin.clone()).or_default();
            list.retain(|c| !c.sender.is_closed());
            if list.len() < MAX_H2_CONNS_PER_ORIGIN {
                list.push(pooled.clone());
            }
        }
        Ok(pooled)
    }

    fn checkout_ready_h2(&self, origin: &OriginKey) -> Option<PooledHttp2Conn> {
        let mut map = self.active_http2_pool.lock().ok()?;
        let list = map.get_mut(origin)?;
        list.retain(|c| !c.sender.is_closed());
        if list.is_empty() {
            return None;
        }
        let best = list
            .iter()
            .min_by_key(|c| c.active_streams.load(Ordering::Relaxed))?;
        let load = best.active_streams.load(Ordering::Relaxed);
        // Open fresh parallel TCP+H2 sockets up to `MAX_H2_CONNS_PER_ORIGIN` before multiplexing
        // multiple streams onto one socket so TCP HoL blocking & single-socket policing are bypassed!
        if load >= TARGET_STREAMS_PER_H2_CONN && list.len() < MAX_H2_CONNS_PER_ORIGIN {
            return None;
        }
        best.active_streams.fetch_add(1, Ordering::Relaxed);
        Some(best.clone())
    }

    /// Returns a healthy HTTP/1.1 Keep-Alive connection back to the idle pool, or decrements active
    /// stream count on a pooled HTTP/2 connection after body EOF!
    pub(crate) fn checkin(&self, origin: OriginKey, conn: ActiveHttpConn) {
        match conn {
            ActiveHttpConn::Http1(c) => {
                if c.sender.is_closed() {
                    return;
                }
                if let Ok(mut map) = self.idle_http1_pool.lock() {
                    let list = map.entry(origin).or_default();
                    if list.len() < MAX_IDLE_PER_ORIGIN {
                        list.push(c);
                    }
                }
            }
            ActiveHttpConn::Http2(c) => {
                let _ = c
                    .active_streams
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(1))
                    });
                if c.sender.is_closed()
                    && let Ok(mut map) = self.active_http2_pool.lock()
                    && let Some(list) = map.get_mut(&origin)
                {
                    list.retain(|entry| !entry.sender.is_closed());
                }
            }
        }
    }

    /// Releases a failed/aborted stream slot on an HTTP/2 connection cleanly (`RST_STREAM`).
    pub(crate) fn release_aborted(&self, origin: &OriginKey, conn: ActiveHttpConn) {
        if let ActiveHttpConn::Http2(c) = conn {
            let _ = c
                .active_streams
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                });
            if c.sender.is_closed()
                && let Ok(mut map) = self.active_http2_pool.lock()
                && let Some(list) = map.get_mut(origin)
            {
                list.retain(|entry| !entry.sender.is_closed());
            }
        }
    }

    /// Current count of warm idle HTTP/1.1 Keep-Alive or live HTTP/2 connections cached for `(scheme, host, port)`.
    #[must_use]
    pub fn idle_connection_count(&self, scheme: &str, host: &str, port: u16) -> usize {
        let key = OriginKey {
            scheme: scheme.to_ascii_lowercase(),
            host: host.to_ascii_lowercase(),
            port,
        };
        let h1 = self
            .idle_http1_pool
            .lock()
            .ok()
            .and_then(|m| m.get(&key).map(Vec::len))
            .unwrap_or(0);
        let h2 = self
            .active_http2_pool
            .lock()
            .ok()
            .and_then(|m| {
                m.get(&key)
                    .map(|v| v.iter().filter(|c| !c.sender.is_closed()).count())
            })
            .unwrap_or(0);
        h1 + h2
    }

    fn min_filter_for_host(&self, host: &str) -> Arc<Mutex<WindowedMinFilter>> {
        let mut map = self
            .host_min_filters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            map.entry(host.to_ascii_lowercase())
                .or_insert_with(|| Arc::new(Mutex::new(WindowedMinFilter::default()))),
        )
    }

    async fn dial_happy_eyeballs(
        &self,
        ips: &[IpAddr],
        port: u16,
        trace_id: TraceId,
    ) -> AppResult<TcpStream> {
        // Fast path for single IP
        if let [only_ip] = ips {
            return connect_configured_socket(
                SocketAddr::new(*only_ip, port),
                self.connect_timeout,
            )
            .await
            .map_err(|e| {
                self.quarantine_ip(*only_ip);
                VortexError::coded(
                    ErrorCode::NetConnectionLost,
                    trace_id,
                    format!("TCP connect to {only_ip}:{port} failed: {e}"),
                )
            });
        }

        // RFC 8305 staggered Happy Eyeballs race (250ms head start per candidate) over Round-Robin IPs
        let mut join_set = tokio::task::JoinSet::new();
        let timeout = self.connect_timeout;
        let mut last_err = String::from("all candidate IPs failed");

        for (idx, &ip) in ips.iter().take(6).enumerate() {
            let addr = SocketAddr::new(ip, port);
            join_set.spawn(async move {
                if idx > 0 {
                    let stagger = u32::try_from(idx).unwrap_or(1);
                    tokio::time::sleep(HAPPY_EYEBALLS_DELAY.saturating_mul(stagger)).await;
                }
                (ip, connect_configured_socket(addr, timeout).await)
            });
        }

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((_, Ok(stream))) => {
                    join_set.abort_all();
                    return Ok(stream);
                }
                Ok((bad_ip, Err(io_err))) => {
                    self.quarantine_ip(bad_ip);
                    last_err = io_err.to_string();
                }
                Err(join_err) => {
                    last_err = join_err.to_string();
                }
            }
        }

        Err(VortexError::coded(
            ErrorCode::NetConnectionLost,
            trace_id,
            format!("Happy Eyeballs TCP connect failed: {last_err}"),
        ))
    }
}

/// Configures `socket2::Socket` with `TCP_NODELAY = true` and **deliberately leaves `SO_RCVBUF`
/// untouched** so Windows TCP Receive Window Auto-Tuning scales dynamically up to 10 Gbps (`§11.2`).
async fn connect_configured_socket(
    addr: SocketAddr,
    connect_timeout: Duration,
) -> std::io::Result<TcpStream> {
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    socket.set_tcp_nodelay(true)?;
    let _ = socket.set_keepalive(true);
    // CRITICAL (`Engineering-Standards.md` §2.3 & `P1-T14`): NEVER call `socket.set_recv_buffer_size(...)`!

    match socket.connect(&addr.into()) {
        Ok(()) => {}
        Err(err) if is_connect_in_progress(&err) => {}
        Err(err) => return Err(err),
    }

    let std_stream: std::net::TcpStream = socket.into();
    let stream = TcpStream::from_std(std_stream)?;

    tokio::time::timeout(connect_timeout, stream.writable())
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "TCP connect timed out")
        })??;

    if let Some(sock_err) = stream.take_error()? {
        return Err(sock_err);
    }
    Ok(stream)
}

fn is_connect_in_progress(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock
        || matches!(err.raw_os_error(), Some(10035 | 115 | 36))
}

fn interleave_v6_v4(ips: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut v6 = Vec::new();
    let mut v4 = Vec::new();
    for ip in ips {
        if ip.is_ipv6() {
            v6.push(ip);
        } else {
            v4.push(ip);
        }
    }
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let mut i6 = v6.into_iter();
    let mut i4 = v4.into_iter();
    loop {
        match (i6.next(), i4.next()) {
            (Some(a), Some(b)) => {
                out.push(a);
                out.push(b);
            }
            (Some(a), None) => out.push(a),
            (None, Some(b)) => out.push(b),
            (None, None) => break,
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn stripes_multiple_cdn_ips_round_robin_and_quarantines_failed_ip() {
        let pool = TransportPool::new(Duration::from_secs(5)).unwrap();
        let ip1 = IpAddr::V4(Ipv4Addr::new(104, 16, 1, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(104, 16, 1, 2));
        let ip3 = IpAddr::V4(Ipv4Addr::new(104, 16, 1, 3));
        let all = vec![ip1, ip2, ip3];

        assert_eq!(
            pool.stripe_and_filter_ips("cdn.example", all.clone())
                .first(),
            Some(&ip1)
        );
        assert_eq!(
            pool.stripe_and_filter_ips("cdn.example", all.clone())
                .first(),
            Some(&ip2)
        );
        assert_eq!(
            pool.stripe_and_filter_ips("cdn.example", all.clone())
                .first(),
            Some(&ip3)
        );

        // Quarantine `ip2` -> subsequent rotations only return `ip1` and `ip3`!
        pool.quarantine_ip(ip2);
        let after = pool.stripe_and_filter_ips("cdn.example", all);
        assert_eq!(after.len(), 2);
        assert!(!after.contains(&ip2));
    }
}
