//! Benchmarks that validate the allocation audit:
//!
//! 1. `serde_json::to_string(&call)` (per-call String alloc — current code in
//!    `conn.rs`) vs. `serde_json::to_writer(&mut Vec<u8>, &call)` with a
//!    scratch buffer reused across a batch.  This is the "flagged-for-future-
//!    work" refactor from the audit.
//!
//! 2. `HashMap::new()` + loop inserts vs. `HashMap::with_capacity(n)` + loop
//!    inserts, sized from the source `.len()`.  This validates the change
//!    applied to `cache/manager.rs`.
//!
//! 3. `Vec::with_capacity(hint)` with the raw (possibly-attacker-supplied)
//!    hint vs. the clamped version the audit produced for `cache/stream.rs`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::collections::HashMap;

use chromey_types::{CallId, MethodCall, MethodId};
use serde_json::json;

// ---------------------------------------------------------------------------
// Realistic CDP commands (mirrors what the WS writer loop actually serialises)
// ---------------------------------------------------------------------------

/// Small command (e.g. `Page.enable`): no params, no session.
fn small_call(id: usize) -> MethodCall {
    MethodCall {
        id: CallId::new(id),
        method: MethodId::from("Page.enable"),
        session_id: None,
        params: json!({}),
    }
}

/// Medium command (e.g. `Page.navigate`): typical flat params with a session.
fn medium_call(id: usize) -> MethodCall {
    MethodCall {
        id: CallId::new(id),
        method: MethodId::from("Page.navigate"),
        session_id: Some("1A2B3C4D5E6F7A8B9C0D1E2F3A4B5C6D".into()),
        params: json!({
            "url": "https://example.com/some/path?query=value&other=thing",
            "transitionType": "link",
            "frameId": "A1B2C3D4E5F6",
        }),
    }
}

/// Large command (e.g. `Runtime.evaluate` with an inline script body).
fn large_call(id: usize) -> MethodCall {
    let script = r#"
        (() => {
            const results = [];
            const nodes = document.querySelectorAll('article, section, main, div.content');
            for (const n of nodes) {
                const r = n.getBoundingClientRect();
                if (r.width > 200 && r.height > 100) {
                    results.push({
                        tag: n.tagName.toLowerCase(),
                        id: n.id || null,
                        cls: n.className || null,
                        text: (n.innerText || '').slice(0, 512),
                        bbox: { x: r.x, y: r.y, w: r.width, h: r.height },
                    });
                }
            }
            return JSON.stringify(results);
        })()
    "#;
    MethodCall {
        id: CallId::new(id),
        method: MethodId::from("Runtime.evaluate"),
        session_id: Some("1A2B3C4D5E6F7A8B9C0D1E2F3A4B5C6D".into()),
        params: json!({
            "expression": script,
            "awaitPromise": false,
            "returnByValue": true,
            "userGesture": false,
            "generatePreview": true,
        }),
    }
}

/// Build a realistic batch: small-heavy traffic (typical CDP chatter).
fn batch(n: usize) -> Vec<MethodCall> {
    (0..n)
        .map(|i| match i % 10 {
            0..=6 => small_call(i),
            7 | 8 => medium_call(i),
            _ => large_call(i),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Benchmark 1: serde_json::to_string vs. reusable Vec<u8> via to_writer
// ---------------------------------------------------------------------------

/// Current `conn.rs` pattern: fresh `String` per call.
fn serialize_each_to_string(batch: &[MethodCall]) -> usize {
    let mut total = 0usize;
    for call in batch {
        let s = serde_json::to_string(call).unwrap();
        total = total.wrapping_add(s.len());
    }
    total
}

/// Proposed pattern: reusable `Vec<u8>` scratch across the whole batch.
fn serialize_each_to_reused_vec(batch: &[MethodCall], scratch: &mut Vec<u8>) -> usize {
    let mut total = 0usize;
    for call in batch {
        scratch.clear();
        serde_json::to_writer(&mut *scratch, call).unwrap();
        total = total.wrapping_add(scratch.len());
    }
    total
}

fn bench_serialize_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("ws_writer/serialize_batch");

    for &n in &[1usize, 10, 100, 1000] {
        let b = batch(n);
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("to_string (current)", n), &b, |bi, b| {
            bi.iter(|| black_box(serialize_each_to_string(black_box(b))));
        });

        group.bench_with_input(
            BenchmarkId::new("to_writer + reused Vec", n),
            &b,
            |bi, b| {
                let mut scratch: Vec<u8> = Vec::with_capacity(512);
                bi.iter(|| {
                    black_box(serialize_each_to_reused_vec(black_box(b), &mut scratch));
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: HashMap::new vs HashMap::with_capacity(n) for headers
// ---------------------------------------------------------------------------

/// Realistic header set — ~22 entries like a typical HTTP response.
fn realistic_headers() -> Vec<(String, String)> {
    vec![
        ("content-type", "text/html; charset=utf-8"),
        ("content-length", "48212"),
        ("date", "Thu, 18 Apr 2026 12:34:56 GMT"),
        ("server", "nginx/1.25.3"),
        ("cache-control", "public, max-age=300, s-maxage=600"),
        ("etag", "\"6421fa81-bc54\""),
        ("last-modified", "Wed, 17 Apr 2026 09:12:44 GMT"),
        ("vary", "Accept-Encoding, User-Agent"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "SAMEORIGIN"),
        ("x-xss-protection", "1; mode=block"),
        (
            "strict-transport-security",
            "max-age=31536000; includeSubDomains; preload",
        ),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self' 'unsafe-inline'",
        ),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        ("x-request-id", "a1b2c3d4-e5f6-7890-abcd-ef0123456789"),
        ("x-served-by", "cache-sjc10038-SJC"),
        ("x-cache", "HIT"),
        ("x-cache-hits", "12"),
        ("accept-ranges", "bytes"),
        ("age", "142"),
        (
            "set-cookie",
            "session=abc123; Path=/; Secure; HttpOnly; SameSite=Lax",
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn build_map_new(src: &[(String, String)]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for (k, v) in src {
        m.insert(k.clone(), v.clone());
    }
    m
}

fn build_map_with_capacity(src: &[(String, String)]) -> HashMap<String, String> {
    let mut m = HashMap::with_capacity(src.len());
    for (k, v) in src {
        m.insert(k.clone(), v.clone());
    }
    m
}

fn bench_headers_to_map(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/headers_to_map");
    let headers = realistic_headers();
    group.throughput(Throughput::Elements(headers.len() as u64));

    group.bench_function("HashMap::new (pre-audit)", |b| {
        b.iter(|| black_box(build_map_new(black_box(&headers))));
    });

    group.bench_function("HashMap::with_capacity (post-audit)", |b| {
        b.iter(|| black_box(build_map_with_capacity(black_box(&headers))));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: Content-Length capacity hint (clamped vs raw)
// ---------------------------------------------------------------------------

const MAX_PREALLOC_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Simulate the chunk-read path: pre-alloc then extend with realistic chunks
/// until `actual_body_len` bytes accumulated.
fn accumulate_body(initial_capacity: usize, actual_body_len: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(initial_capacity);
    let chunk = [0u8; 65_536];
    let mut remaining = actual_body_len;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        body.extend_from_slice(&chunk[..n]);
        remaining -= n;
    }
    body
}

fn bench_body_prealloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/body_prealloc");

    // Honest server: Content-Length matches the body exactly.
    for &size in &[64 * 1024usize, 1_000_000, 10_000_000] {
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::new("raw_hint (pre-audit)", size),
            &size,
            |bi, &size| {
                bi.iter(|| black_box(accumulate_body(black_box(size), black_box(size))));
            },
        );

        group.bench_with_input(
            BenchmarkId::new("clamped_hint (post-audit)", size),
            &size,
            |bi, &size| {
                let clamped = size.min(MAX_PREALLOC_BODY_BYTES);
                bi.iter(|| black_box(accumulate_body(black_box(clamped), black_box(size))));
            },
        );
    }

    // Attacker: Content-Length = 2 GiB, actual body = 128 KiB.
    // Raw hint would alloc 2 GiB up front; clamped caps at 8 MiB; realistic
    // body never reaches either.  We skip the raw-hint case here because
    // running `Vec::with_capacity(2 GiB)` repeatedly in a bench will OOM /
    // thrash the machine — *that* is the DoS we're eliminating.
    group.bench_function("clamped_hint/attacker_2GiB_hint_128KiB_body", |bi| {
        let hint = 2 * 1024 * 1024 * 1024usize;
        let clamped = hint.min(MAX_PREALLOC_BODY_BYTES);
        let actual = 128 * 1024usize;
        bi.iter(|| black_box(accumulate_body(black_box(clamped), black_box(actual))));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_serialize_batch,
    bench_headers_to_map,
    bench_body_prealloc
);
criterion_main!(benches);
