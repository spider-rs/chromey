//! Lock-free DNS cache for the CDP WebSocket connect path.
//!
//! `tokio-tungstenite` resolves the debugger host on **every** connect
//! (`getaddrinfo` on tokio's blocking pool). For a hostname-addressed CDP
//! endpoint that adds a DNS round-trip plus a blocking-pool hop to each
//! (re)connect. This caches the resolved addresses per `host:port` with a short
//! TTL so reconnects skip resolution.
//!
//! ## Safety contract — must never redirect a connection
//!
//! This cache only ever maps **host → IP address(es)**. It must be impossible
//! for it to change *which endpoint* a connect targets — otherwise it could, for
//! example, send a connection meant for one port-addressed backend to another.
//! The guarantees:
//!
//!   * **Port (and path) are never cached.** The caller's URL supplies them
//!     verbatim; the key includes the port only to avoid mixing resolutions, and
//!     the value is purely a list of IPs. An endpoint distinguished by *port*
//!     (e.g. one listener per backend) is never confused with another.
//!   * **IP-literal hosts are never cached** (no DNS to cache — caller bypasses).
//!   * **`wss://` is never cached here** (caller keeps TLS on the default path).
//!   * **Invalidate-on-failure + short TTL:** a moved/replaced endpoint
//!     self-heals on the next attempt instead of being pinned to a stale IP.
//!
//! ## Concurrency / safety contract
//!   * **No `Mutex`/`RwLock`.** State is a single sharded `DashMap`; a shard
//!     guard is only ever held for an O(1) map op and **never across an
//!     `.await`** (resolution runs with no guard held), and the read path drops
//!     its guard before returning. No second map ⇒ no cross-map lock ordering ⇒
//!     no deadlock.
//!   * **No panics.** No `unwrap`/indexing; `lookup_host` errors map to
//!     [`CdpError`]; bounded growth control (reap + hard cap).
//!   * **Bounded.** Stale entries are reaped at a soft threshold and a hard cap
//!     prevents unbounded growth.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use lazy_static::lazy_static;

use crate::error::{CdpError, Result};

/// How long a resolution is trusted before the next connect re-resolves.
const TTL: Duration = Duration::from_secs(30);
/// Reap stale entries once the map reaches this size…
const REAP_AT: usize = 256;
/// …and never let it grow past this (hard memory bound).
const HARD_CAP: usize = 1024;

#[derive(Clone)]
struct Entry {
    addrs: Arc<[SocketAddr]>,
    at: Instant,
}

lazy_static! {
    /// host:port -> resolved addresses. Sharded `DashMap` — lock-free in the same
    /// sense the rest of the stack uses it (no `Mutex`/`RwLock`).
    static ref CACHE: DashMap<String, Entry> = DashMap::new();
}

#[inline]
fn key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Whether the WS-connect DNS cache is active. **Default ON.** Set
/// `CHROMEY_WS_DNS_CACHE` to `0`/`false`/`no`/`off` to disable and fall back to
/// tokio-tungstenite's per-connect resolution (a kill-switch with zero behavior
/// change when flipped off). Read once and cached.
#[inline]
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("CHROMEY_WS_DNS_CACHE")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0") | Some("false") | Some("no") | Some("off")
        )
    })
}

/// True if `host` is a bare IP literal — such hosts need no DNS and must not be
/// cached (the caller connects to them directly).
#[inline]
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Resolve `host:port` to socket addresses, serving a fresh cached result when
/// available. The returned addresses should be tried in order (as a fresh
/// resolution would be). Resolution runs with no shard guard held.
pub async fn resolve(host: &str, port: u16) -> Result<Arc<[SocketAddr]>> {
    let k = key(host, port);

    if let Some(addrs) = fresh(&k) {
        return Ok(addrs);
    }

    let addrs = lookup(host, port).await?;
    store(k, addrs.clone());
    Ok(addrs)
}

/// Drop the cached resolution for `host:port` after a connection problem so the
/// next attempt re-resolves instead of reusing a possibly-stale address.
#[inline]
pub fn invalidate(host: &str, port: u16) {
    CACHE.remove(&key(host, port));
}

/// Clone a fresh cached entry, releasing the shard guard before returning (so the
/// caller never holds it across `.await`).
fn fresh(k: &str) -> Option<Arc<[SocketAddr]>> {
    let e = CACHE.get(k)?; // brief shard read guard
    if e.at.elapsed() <= TTL {
        Some(e.addrs.clone())
    } else {
        None
    }
    // guard dropped here
}

fn store(k: String, addrs: Arc<[SocketAddr]>) {
    // Reap expired rows before the hard cap. `retain` takes brief per-shard
    // guards and runs no `.await`; it completes before the insert below, so no
    // guard is ever nested with another.
    if CACHE.len() >= REAP_AT {
        CACHE.retain(|_, e| e.at.elapsed() <= TTL);
    }
    if CACHE.len() < HARD_CAP {
        CACHE.insert(k, Entry { addrs, at: Instant::now() });
    }
}

async fn lookup(host: &str, port: u16) -> Result<Arc<[SocketAddr]>> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(CdpError::Io)?
        .collect();
    if addrs.is_empty() {
        return Err(CdpError::msg(format!("dns: no addresses for {host}:{port}")));
    }
    Ok(Arc::from(addrs.into_boxed_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sa(p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), p)
    }

    #[test]
    fn ip_literals_are_recognised() {
        assert!(is_ip_literal("127.0.0.1"));
        assert!(is_ip_literal("::1"));
        assert!(!is_ip_literal("localhost"));
        assert!(!is_ip_literal("chrome.internal"));
    }

    #[test]
    fn fresh_returns_cached_then_invalidate_clears() {
        let k = key("unit-host", 9222);
        store(k.clone(), Arc::from(vec![sa(9222)].into_boxed_slice()));
        assert!(fresh(&k).is_some(), "fresh entry should be served");
        invalidate("unit-host", 9222);
        assert!(fresh(&k).is_none(), "invalidated entry must be gone");
    }

    #[test]
    fn key_includes_port_so_same_host_different_port_are_distinct() {
        // The port distinguishes endpoints (one listener per backend) — caching
        // must never collapse two ports of the same host into one entry.
        assert_ne!(key("h2", 9222), key("h2", 9223));
        store(key("h2", 9222), Arc::from(vec![sa(9222)].into_boxed_slice()));
        assert!(fresh(&key("h2", 9222)).is_some());
        assert!(fresh(&key("h2", 9223)).is_none());
        invalidate("h2", 9222);
    }
}
