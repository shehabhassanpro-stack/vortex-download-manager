//! Kernel TCP RTT reader (`WSAIoctl(SIO_TCP_INFO)` `v1` -> `v0` fallback) and BBR-style
//! 60-second `WindowedMinFilter` (`P1-T14` / `Vortex-Architecture-Design.md` §8.5).

use std::sync::{Arc, Mutex};

use tokio::net::TcpStream;
use vortex_application::TransportMetrics;

/// Sliding window duration (`60,000 ms`) for BBR `RTT_min` tracking (`§8.5`).
pub const MIN_RTT_WINDOW_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy)]
struct RttSample {
    rtt_us: u64,
    at_ms: i64,
}

/// Kathleen Nichols / Google BBR 3-slot windowed minimum RTT filter (`60s` horizon).
#[derive(Debug, Clone)]
pub struct WindowedMinFilter {
    window_ms: i64,
    slots: [Option<RttSample>; 3],
}

impl Default for WindowedMinFilter {
    fn default() -> Self {
        Self::new(MIN_RTT_WINDOW_MS)
    }
}

impl WindowedMinFilter {
    /// Creates a new [`WindowedMinFilter`] with `window_ms` total span.
    #[must_use]
    pub const fn new(window_ms: i64) -> Self {
        Self {
            window_ms: if window_ms < 1_000 { 1_000 } else { window_ms },
            slots: [None, None, None],
        }
    }

    /// Feeds a fresh RTT measurement (`rtt_us` > 0) at `now_ms` and returns the current windowed minimum.
    pub fn update(&mut self, rtt_us: u64, now_ms: i64) -> u64 {
        let clamped_rtt = rtt_us.max(1);
        let sample = RttSample {
            rtt_us: clamped_rtt,
            at_ms: now_ms,
        };

        let Some(first) = self.slots[0] else {
            self.slots = [Some(sample), Some(sample), Some(sample)];
            return clamped_rtt;
        };

        let dt = now_ms.saturating_sub(first.at_ms);
        if clamped_rtt <= first.rtt_us || dt > self.window_ms {
            self.slots = [Some(sample), Some(sample), Some(sample)];
            return clamped_rtt;
        }

        if let Some(second) = self.slots[1]
            && clamped_rtt <= second.rtt_us
        {
            self.slots[1] = Some(sample);
            self.slots[2] = Some(sample);
        } else if let Some(third) = self.slots[2]
            && clamped_rtt <= third.rtt_us
        {
            self.slots[2] = Some(sample);
        }

        let quarter = self.window_ms / 4;
        if let Some(s1) = self.slots[1]
            && now_ms.saturating_sub(first.at_ms) > quarter
        {
            self.slots[0] = Some(s1);
            self.slots[1] = self.slots[2];
            self.slots[2] = Some(sample);
            if let Some(new_first) = self.slots[0]
                && now_ms.saturating_sub(new_first.at_ms) > self.window_ms
            {
                self.slots = [Some(sample), Some(sample), Some(sample)];
            }
        }

        self.slots[0].map_or(clamped_rtt, |s| s.rtt_us)
    }

    /// Current minimum RTT in microseconds (`None` if no sample recorded yet).
    #[must_use]
    pub fn current_min_us(&self) -> Option<u64> {
        self.slots[0].map(|s| s.rtt_us)
    }
}

/// Shared socket telemetry handle attached to every pooled connection so `TransportMetrics` can be
/// sampled without owning the `TcpStream`.
#[derive(Debug, Clone)]
pub struct SocketMetricsProbe {
    #[cfg(windows)]
    raw_socket: u64,
    is_proxied: bool,
    reused: bool,
    is_http2: bool,
    min_filter: Arc<Mutex<WindowedMinFilter>>,
    fallback_header_rtt_us: Option<u64>,
}

impl SocketMetricsProbe {
    /// Creates a [`SocketMetricsProbe`] from a connected [`TcpStream`].
    #[must_use]
    pub fn from_stream(
        stream: &TcpStream,
        is_proxied: bool,
        reused: bool,
        min_filter: Arc<Mutex<WindowedMinFilter>>,
        fallback_header_rtt_us: Option<u64>,
    ) -> Self {
        #[cfg(windows)]
        let raw_socket = {
            use std::os::windows::io::AsRawSocket;
            stream.as_raw_socket()
        };
        #[cfg(not(windows))]
        let _ = stream;

        Self {
            #[cfg(windows)]
            raw_socket,
            is_proxied,
            reused,
            is_http2: false,
            min_filter,
            fallback_header_rtt_us,
        }
    }

    /// Marks whether this connection negotiated `HTTP/2` (`ALPN: h2`).
    #[must_use]
    pub const fn with_http2(mut self, is_http2: bool) -> Self {
        self.is_http2 = is_http2;
        self
    }

    /// Marks this connection handle as warm/reused from the Keep-Alive / H2 pool.
    #[must_use]
    pub fn mark_reused(&self, header_rtt_us: Option<u64>) -> Self {
        let mut copy = self.clone();
        copy.reused = true;
        copy.fallback_header_rtt_us = header_rtt_us.or(self.fallback_header_rtt_us);
        copy
    }

    /// Samples RTT (`WSAIoctl(SIO_TCP_INFO)` on direct Windows sockets; `WindowedMinFilter` on
    /// protocol RTT when proxied or on non-Windows targets — `§8.5` & `P1-T14`).
    #[must_use]
    pub fn sample(&self, now_ms: i64) -> TransportMetrics {
        if !self.is_proxied
            && let Some((rtt_us, kernel_min_us)) = self.query_kernel_tcp_info()
        {
            let min_rtt_us = if let Some(k_min) = kernel_min_us.filter(|&m| m > 0) {
                if let Ok(mut f) = self.min_filter.lock() {
                    let _ = f.update(k_min, now_ms);
                }
                Some(k_min)
            } else if let Ok(mut f) = self.min_filter.lock() {
                Some(f.update(rtt_us, now_ms))
            } else {
                Some(rtt_us)
            };

            return TransportMetrics {
                rtt_us: Some(rtt_us),
                min_rtt_us,
                is_proxied: false,
                reused_connection: self.reused,
                is_http2: self.is_http2,
            };
        }

        // Proxied or fallback protocol-level header RTT filtered through 60s WindowedMinFilter
        if let Some(proto_rtt) = self.fallback_header_rtt_us {
            let min_us = self
                .min_filter
                .lock()
                .ok()
                .map_or(proto_rtt, |mut f| f.update(proto_rtt, now_ms));
            return TransportMetrics {
                rtt_us: Some(proto_rtt),
                min_rtt_us: Some(min_us),
                is_proxied: self.is_proxied,
                reused_connection: self.reused,
                is_http2: self.is_http2,
            };
        }

        TransportMetrics {
            rtt_us: None,
            min_rtt_us: self.min_filter.lock().ok().and_then(|f| f.current_min_us()),
            is_proxied: self.is_proxied,
            reused_connection: self.reused,
            is_http2: self.is_http2,
        }
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn query_kernel_tcp_info(&self) -> Option<(u64, Option<u64>)> {
        use windows_sys::Win32::Networking::WinSock::{
            SIO_TCP_INFO, SOCKET, TCP_INFO_v0, TCP_INFO_v1, WSAIoctl,
        };

        #[allow(clippy::cast_possible_truncation)]
        let sock = self.raw_socket as SOCKET;

        // 1. Try `TCP_INFO_v1` (Windows 10 2004+ / Windows 11): provides both `RttUs` and `MinRtt`!
        let mut info_version_v1: u32 = 1;
        // SAFETY: `TCP_INFO_v1` is a plain old data (`#[repr(C)]`) C struct of integers where all-zeros is valid.
        let mut info_v1: TCP_INFO_v1 = unsafe { std::mem::zeroed() };
        let mut bytes_returned: u32 = 0;

        // SAFETY: `sock` is an open WinSock `SOCKET` owned by the pooled connection; `info_version_v1`
        // (4-byte `u32 = 1`) and `info_v1` (`TCP_INFO_v1`) match the exact Win32 `SIO_TCP_INFO` contract.
        let rc_v1 = unsafe {
            WSAIoctl(
                sock,
                SIO_TCP_INFO,
                (&raw mut info_version_v1).cast(),
                u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4),
                (&raw mut info_v1).cast(),
                u32::try_from(std::mem::size_of::<TCP_INFO_v1>()).unwrap_or(0),
                &raw mut bytes_returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc_v1 == 0 && info_v1.RttUs > 0 {
            let min_opt = (info_v1.MinRttUs > 0).then_some(u64::from(info_v1.MinRttUs));
            return Some((u64::from(info_v1.RttUs), min_opt));
        }

        // 2. Fallback to `TCP_INFO_v0` (Windows 10 1809..1909) + `WindowedMinFilter`
        let mut info_version_v0: u32 = 0;
        // SAFETY: `TCP_INFO_v0` is a plain old data (`#[repr(C)]`) C struct of integers where all-zeros is valid.
        let mut info_v0: TCP_INFO_v0 = unsafe { std::mem::zeroed() };
        bytes_returned = 0;

        // SAFETY: Same contract as above with `info_version_v0 = 0` and `TCP_INFO_v0` output buffer.
        let rc_v0 = unsafe {
            WSAIoctl(
                sock,
                SIO_TCP_INFO,
                (&raw mut info_version_v0).cast(),
                u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4),
                (&raw mut info_v0).cast(),
                u32::try_from(std::mem::size_of::<TCP_INFO_v0>()).unwrap_or(0),
                &raw mut bytes_returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc_v0 == 0 && info_v0.RttUs > 0 {
            return Some((u64::from(info_v0.RttUs), None));
        }
        None
    }

    #[cfg(not(windows))]
    #[allow(clippy::unused_self)]
    const fn query_kernel_tcp_info(&self) -> Option<(u64, Option<u64>)> {
        None
    }
}
