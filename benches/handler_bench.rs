//! Benchmarks for the internal handler hot paths.
//!
//! These benchmarks measure the overhead of the chromey machinery itself
//! (channel dispatch, serialization, event fan-out) without requiring a
//! running Chrome instance.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

use chromey::cmd::CommandMessage;
use chromey::handler::commandfuture::CommandFuture;
use chromey::handler::sender::PageSender;
use chromey::handler::target::TargetMessage;
use chromey::listeners::{EventListenerRequest, EventListeners};

use chromey_cdp::cdp::browser_protocol::page::NavigateParams;
use chromey_cdp::cdp::browser_protocol::target::SessionId;

/// Create a no-op waker for synchronous polling in benchmarks.
fn noop_waker() -> Waker {
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Benchmark: CommandMessage creation (serialisation overhead).
fn bench_command_message_creation(c: &mut Criterion) {
    c.bench_function("CommandMessage::new (NavigateParams)", |b| {
        b.iter(|| {
            let cmd = NavigateParams::new("https://example.com");
            let (tx, _rx) = tokio::sync::oneshot::channel::<
                chromey::error::Result<chromey_types::Response>,
            >();
            let msg = CommandMessage::new(cmd, tx).unwrap();
            black_box(msg);
        });
    });
}

/// Benchmark: CommandMessage::with_session (includes session id).
fn bench_command_message_with_session(c: &mut Criterion) {
    c.bench_function("CommandMessage::with_session (NavigateParams)", |b| {
        b.iter(|| {
            let cmd = NavigateParams::new("https://example.com");
            let (tx, _rx) = tokio::sync::oneshot::channel::<
                chromey::error::Result<chromey_types::Response>,
            >();
            let session = Some(SessionId::from("session-1".to_string()));
            let msg = CommandMessage::with_session(cmd, tx, session).unwrap();
            black_box(msg);
        });
    });
}

/// Benchmark: try_send fast path on a page channel.
fn bench_try_send_fast_path(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("try_send fast path (channel has capacity)", |b| {
        b.iter(|| {
            let (tx, _rx) = tokio::sync::mpsc::channel::<TargetMessage>(2048);
            let cmd = NavigateParams::new("https://example.com");
            let (otx, _orx) = tokio::sync::oneshot::channel();
            let msg =
                CommandMessage::with_session(cmd, otx, Some(SessionId::from("s1".to_string())))
                    .unwrap();
            let target_msg = TargetMessage::Command(msg);
            let result = tx.try_send(target_msg);
            let _ = black_box(result);
        });
    });

    c.bench_function("async send path (channel has capacity)", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, _rx) = tokio::sync::mpsc::channel::<TargetMessage>(2048);
                let cmd = NavigateParams::new("https://example.com");
                let (otx, _orx) = tokio::sync::oneshot::channel();
                let msg =
                    CommandMessage::with_session(cmd, otx, Some(SessionId::from("s1".to_string())))
                        .unwrap();
                let result = tx.send(TargetMessage::Command(msg)).await;
                let _ = black_box(result);
            });
        });
    });
}

/// Benchmark: CommandFuture creation (measures allocation overhead).
fn bench_command_future_creation(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("CommandFuture::new (NavigateParams)", |b| {
        b.iter(|| {
            let _guard = rt.enter();
            let (tx, _rx) = tokio::sync::mpsc::channel::<TargetMessage>(2048);
            let sender = PageSender::new(tx, None);
            let cmd = NavigateParams::new("https://example.com");
            let session = Some(SessionId::from("session-1".to_string()));
            let fut =
                CommandFuture::<NavigateParams>::new(cmd, sender, session, Duration::from_secs(30))
                    .unwrap();
            black_box(fut);
        });
    });
}

/// Benchmark: EventListeners dispatch throughput.
fn bench_event_listeners_dispatch(c: &mut Criterion) {
    use chromey_cdp::cdp::browser_protocol::animation::EventAnimationCanceled;

    c.bench_function("EventListeners: dispatch to 10 listeners", |b| {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        b.iter(|| {
            let mut listeners = EventListeners::default();

            // Register 10 listeners
            let mut receivers = Vec::new();
            for _ in 0..10 {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                listeners.add_listener(EventListenerRequest::new::<EventAnimationCanceled>(tx));
                receivers.push(rx);
            }

            // Dispatch 100 events
            for i in 0..100 {
                listeners.start_send(EventAnimationCanceled {
                    id: format!("anim-{i}"),
                });
            }

            // Flush
            listeners.poll(&mut cx);
            black_box(&listeners);
        });
    });

    c.bench_function("EventListeners: poll with disconnected listeners", |b| {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        b.iter(|| {
            let mut listeners = EventListeners::default();

            // Register 50 listeners then drop all receivers
            for _ in 0..50 {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                listeners.add_listener(EventListenerRequest::new::<EventAnimationCanceled>(tx));
                // _rx dropped here — listener is disconnected
            }

            // Dispatch events to disconnected listeners
            for i in 0..10 {
                listeners.start_send(EventAnimationCanceled {
                    id: format!("anim-{i}"),
                });
            }

            // Poll should clean up all disconnected listeners
            listeners.poll(&mut cx);
            black_box(&listeners);
        });
    });
}

/// Benchmark: CommandChain state machine polling.
fn bench_command_chain_polling(c: &mut Criterion) {
    use chromey::cmd::CommandChain;
    use chromey_types::MethodId;

    c.bench_function("CommandChain: poll 10 commands to completion", |b| {
        b.iter(|| {
            let cmds: Vec<(MethodId, serde_json::Value)> = (0..10)
                .map(|i| {
                    (
                        MethodId::from(format!("Method.{i}")),
                        serde_json::json!({"param": i}),
                    )
                })
                .collect();

            let mut chain = CommandChain::new(cmds, Duration::from_secs(30));
            let now = Instant::now();

            // Simulate polling each command and receiving a response
            for _i in 0..10 {
                match chain.poll(now) {
                    Poll::Ready(Some(Ok((method, _params)))) => {
                        chain.received_response(method.as_ref());
                    }
                    _ => panic!("expected command"),
                }
            }

            // Should be done
            assert!(matches!(chain.poll(now), Poll::Ready(None)));
            black_box(&chain);
        });
    });
}

/// Benchmark: Oneshot channel creation + response round-trip.
fn bench_oneshot_roundtrip(c: &mut Criterion) {
    c.bench_function("oneshot create + send + recv", |b| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        b.iter(|| {
            rt.block_on(async {
                let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
                tx.send(42).unwrap();
                let val = rx.await.unwrap();
                black_box(val);
            });
        });
    });
}

// ---------------------------------------------------------------------------
//  Concurrent benchmarks — multi-page throughput
// ---------------------------------------------------------------------------

/// Benchmark: N concurrent tasks sending to independent channels (simulates
/// N pages each with their own target channel).  Measures total throughput
/// and proves no task blocks another.
fn bench_concurrent_independent_channels(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    for num_pages in [1, 4, 16, 64] {
        c.bench_function(
            &format!("concurrent {num_pages} pages x 100 cmds (independent channels)"),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        let mut handles = Vec::with_capacity(num_pages);

                        for _ in 0..num_pages {
                            let (tx, mut rx) = tokio::sync::mpsc::channel::<TargetMessage>(2048);
                            let sender = PageSender::new(tx, None);

                            // Consumer: drain the channel
                            let consumer = tokio::spawn(async move {
                                let mut count = 0u64;
                                while let Some(_msg) = rx.recv().await {
                                    count += 1;
                                    if count >= 100 {
                                        break;
                                    }
                                }
                                count
                            });

                            // Producer: send 100 commands via try_send fast path
                            let producer = tokio::spawn(async move {
                                for _ in 0..100u64 {
                                    let cmd = NavigateParams::new("https://example.com");
                                    let (otx, _orx) = tokio::sync::oneshot::channel::<
                                        chromey::error::Result<chromey_types::Response>,
                                    >();
                                    let msg = CommandMessage::with_session(
                                        cmd,
                                        otx,
                                        Some(SessionId::from("s1".to_string())),
                                    )
                                    .unwrap();
                                    let _ = sender.try_send(TargetMessage::Command(msg));
                                }
                            });

                            handles.push((producer, consumer));
                        }

                        for (p, c) in handles {
                            let _ = p.await;
                            let count = c.await.unwrap();
                            black_box(count);
                        }
                    });
                });
            },
        );
    }
}

/// Benchmark: N concurrent tasks sending to a SINGLE shared channel
/// (simulates the browser→handler channel).  Measures contention.
fn bench_concurrent_shared_channel(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    for num_producers in [1, 4, 16, 64] {
        let total_msgs = num_producers * 100;
        c.bench_function(
            &format!("concurrent {num_producers} producers x 100 msgs (shared channel)"),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<TargetMessage>(4096);

                        // Consumer
                        let consumer = tokio::spawn(async move {
                            let mut count = 0u64;
                            while let Some(_msg) = rx.recv().await {
                                count += 1;
                                if count >= total_msgs as u64 {
                                    break;
                                }
                            }
                            count
                        });

                        // Producers
                        let mut producers = Vec::with_capacity(num_producers);
                        for _ in 0..num_producers {
                            let sender = PageSender::new(tx.clone(), None);
                            producers.push(tokio::spawn(async move {
                                for _ in 0..100u64 {
                                    let cmd = NavigateParams::new("https://example.com");
                                    let (otx, _orx) = tokio::sync::oneshot::channel::<
                                        chromey::error::Result<chromey_types::Response>,
                                    >();
                                    let msg = CommandMessage::with_session(
                                        cmd,
                                        otx,
                                        Some(SessionId::from("s1".to_string())),
                                    )
                                    .unwrap();
                                    let _ = sender.try_send(TargetMessage::Command(msg));
                                }
                            }));
                        }
                        drop(tx); // close sender so consumer can finish

                        for p in producers {
                            let _ = p.await;
                        }
                        let count = consumer.await.unwrap();
                        black_box(count);
                    });
                });
            },
        );
    }
}

/// Benchmark: Notify-based wakeup latency (PageSender with Notify).
fn bench_notify_wakeup(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("PageSender with Notify: 1000 send+wake cycles", |b| {
        b.iter(|| {
            rt.block_on(async {
                let notify = std::sync::Arc::new(tokio::sync::Notify::new());
                let (tx, mut rx) = tokio::sync::mpsc::channel::<TargetMessage>(2048);
                let sender = PageSender::new(tx, Some(notify.clone()));

                let consumer = tokio::spawn({
                    let notify = notify.clone();
                    async move {
                        let mut count = 0u64;
                        loop {
                            tokio::select! {
                                _ = notify.notified() => {
                                    while let Ok(_msg) = rx.try_recv() {
                                        count += 1;
                                    }
                                    if count >= 1000 {
                                        break;
                                    }
                                }
                            }
                        }
                        count
                    }
                });

                let producer = tokio::spawn(async move {
                    for _ in 0..1000u64 {
                        let cmd = NavigateParams::new("https://example.com");
                        let (otx, _orx) = tokio::sync::oneshot::channel::<
                            chromey::error::Result<chromey_types::Response>,
                        >();
                        let msg = CommandMessage::with_session(
                            cmd,
                            otx,
                            Some(SessionId::from("s1".to_string())),
                        )
                        .unwrap();
                        let _ = sender.try_send(TargetMessage::Command(msg));
                    }
                });

                let _ = producer.await;
                let count = consumer.await.unwrap();
                black_box(count);
            });
        });
    });
}

// ---------------------------------------------------------------------------
//  WS connection-layer benchmarks — bounded channel + batched serialization
// ---------------------------------------------------------------------------

/// Benchmark: Bounded WS command channel throughput (handler → writer path).
/// Measures try_send + recv + try_recv drain, matching the real ws_write_loop.
fn bench_ws_cmd_channel_throughput(c: &mut Criterion) {
    use chromey_types::{CallId, MethodCall, MethodId};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    for batch_size in [1, 10, 100, 500] {
        c.bench_function(
            &format!("ws_cmd channel: {batch_size} cmds (try_send → recv+drain)"),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<MethodCall>(2048);

                        // Producer: burst-send commands via try_send (non-blocking).
                        let producer = tokio::spawn(async move {
                            for i in 0..batch_size {
                                let call = MethodCall {
                                    id: CallId::new(i),
                                    method: MethodId::from("Page.navigate"),
                                    session_id: None,
                                    params: serde_json::json!({"url": "https://example.com"}),
                                };
                                let _ = tx.try_send(call);
                            }
                        });

                        // Consumer: recv first, then drain via try_recv (matches ws_write_loop).
                        let consumer = tokio::spawn(async move {
                            let mut count = 0usize;
                            while let Some(call) = rx.recv().await {
                                let msg = serde_json::to_string(&call).unwrap();
                                black_box(&msg);
                                count += 1;

                                // Drain batch.
                                while let Ok(call) = rx.try_recv() {
                                    let msg = serde_json::to_string(&call).unwrap();
                                    black_box(&msg);
                                    count += 1;
                                }

                                if count >= batch_size {
                                    break;
                                }
                            }
                            count
                        });

                        let _ = producer.await;
                        let count = consumer.await.unwrap();
                        black_box(count);
                    });
                });
            },
        );
    }
}

/// Benchmark: Serialization throughput for MethodCall (the per-message cost
/// in the WS write loop).
fn bench_ws_method_call_serialization(c: &mut Criterion) {
    use chromey_types::{CallId, MethodCall, MethodId};

    // Small payload (typical CDP command).
    let small_call = MethodCall {
        id: CallId::new(1),
        method: MethodId::from("Page.navigate"),
        session_id: None,
        params: serde_json::json!({"url": "https://example.com"}),
    };

    // Larger payload (e.g. Page.addScriptToEvaluateOnNewDocument).
    let large_call = MethodCall {
        id: CallId::new(2),
        method: MethodId::from("Page.addScriptToEvaluateOnNewDocument"),
        session_id: Some("session-abc-123".to_string()),
        params: serde_json::json!({
            "source": "x".repeat(4096),
            "worldName": "isolated",
        }),
    };

    c.bench_function("MethodCall serialize (small ~100B)", |b| {
        b.iter(|| {
            let msg = serde_json::to_string(black_box(&small_call)).unwrap();
            black_box(msg);
        });
    });

    c.bench_function("MethodCall serialize (large ~4KB)", |b| {
        b.iter(|| {
            let msg = serde_json::to_string(black_box(&large_call)).unwrap();
            black_box(msg);
        });
    });
}

/// Benchmark: Bounded channel back-pressure — producer sending faster than
/// consumer can drain. Measures how the bounded channel (try_send) degrades
/// gracefully vs. building up unbounded memory.
fn bench_ws_cmd_backpressure(c: &mut Criterion) {
    use chromey_types::{CallId, MethodCall, MethodId};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    for capacity in [256, 2048] {
        c.bench_function(
            &format!("ws_cmd backpressure: 1000 cmds, capacity {capacity}"),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<MethodCall>(capacity);

                        // Slow consumer: simulate serialization cost per message.
                        let consumer = tokio::spawn(async move {
                            let mut count = 0usize;
                            while let Some(call) = rx.recv().await {
                                let msg = serde_json::to_string(&call).unwrap();
                                black_box(&msg);
                                count += 1;
                                // Drain batch.
                                while let Ok(call) = rx.try_recv() {
                                    let msg = serde_json::to_string(&call).unwrap();
                                    black_box(&msg);
                                    count += 1;
                                }
                                if count >= 1000 {
                                    break;
                                }
                            }
                            count
                        });

                        // Fast producer: try_send, count drops.
                        let producer = tokio::spawn(async move {
                            let mut sent = 0usize;
                            let mut dropped = 0usize;
                            for i in 0..1000usize {
                                let call = MethodCall {
                                    id: CallId::new(i),
                                    method: MethodId::from("Page.navigate"),
                                    session_id: None,
                                    params: serde_json::json!({"url": "https://example.com"}),
                                };
                                match tx.try_send(call) {
                                    Ok(()) => sent += 1,
                                    Err(_) => dropped += 1,
                                }
                            }
                            (sent, dropped)
                        });

                        let (sent, dropped) = producer.await.unwrap();
                        let received = consumer.await.unwrap();
                        black_box((sent, dropped, received));
                    });
                });
            },
        );
    }
}

// ---------------------------------------------------------------------------
//  Network-utils benchmarks — SIMD-accelerated URL / host parsing
// ---------------------------------------------------------------------------

use chromey::handler::network_utils::{
    base_domain_from_any, base_domain_from_host, first_label, host_and_rest,
    host_contains_label_icase, host_is_subdomain_of, rel_for_ignore_script,
};

fn bench_host_and_rest(c: &mut Criterion) {
    let urls = [
        "https://user:pass@staging.mainr.com:8443/a.js?x=1#y",
        "https://example.com/path/to/resource",
        "http://[::1]:8080/path",
        "blob:https://example.com/path/to/blob",
        "https://cdn.assets.example.co.uk/js/app.min.js?v=42",
        "//protocol-relative.example.com/resource",
    ];

    c.bench_function("host_and_rest: 6 diverse URLs", |b| {
        b.iter(|| {
            for url in &urls {
                black_box(host_and_rest(black_box(url)));
            }
        });
    });
}

fn bench_host_contains_label_icase(c: &mut Criterion) {
    c.bench_function("host_contains_label_icase: 5-label host", |b| {
        b.iter(|| {
            let host = "a.b.c.mainr.example.com";
            black_box(host_contains_label_icase(
                black_box(host),
                black_box("mainr"),
            ));
            black_box(host_contains_label_icase(
                black_box(host),
                black_box("EXAMPLE"),
            ));
            black_box(host_contains_label_icase(
                black_box(host),
                black_box("notfound"),
            ));
        });
    });
}

fn bench_base_domain_from_host(c: &mut Criterion) {
    let hosts = [
        "www.example.com",
        "staging.mainr.com",
        "a.b.example.co.uk",
        "mainr.chilipiper.com",
        "localhost",
        "cdn.assets.example.com",
    ];

    c.bench_function("base_domain_from_host: 6 hosts", |b| {
        b.iter(|| {
            for host in &hosts {
                black_box(base_domain_from_host(black_box(host)));
            }
        });
    });
}

fn bench_host_is_subdomain_of(c: &mut Criterion) {
    c.bench_function("host_is_subdomain_of: mixed match/miss", |b| {
        b.iter(|| {
            black_box(host_is_subdomain_of(
                black_box("staging.mainr.com"),
                black_box("mainr.com"),
            ));
            black_box(host_is_subdomain_of(
                black_box("a.b.c.mainr.com"),
                black_box("mainr.com"),
            ));
            black_box(host_is_subdomain_of(
                black_box("evil-mainr.com"),
                black_box("mainr.com"),
            ));
            black_box(host_is_subdomain_of(
                black_box("mainr.co"),
                black_box("mainr.com"),
            ));
        });
    });
}

fn bench_rel_for_ignore_script(c: &mut Criterion) {
    let base = "mainr.com";
    let urls = [
        "https://mainr.com/careers",
        "https://staging.mainr.com/mainr.min.js",
        "https://cdn.other.com/app.js",
        "/static/app.js",
        "https://mainr.chilipiper.com/concierge-js/cjs/concierge.js",
    ];

    c.bench_function("rel_for_ignore_script: 5 URLs", |b| {
        b.iter(|| {
            for url in &urls {
                black_box(rel_for_ignore_script(black_box(base), black_box(url)));
            }
        });
    });
}

fn bench_first_label(c: &mut Criterion) {
    c.bench_function("first_label: mixed hosts", |b| {
        b.iter(|| {
            black_box(first_label(black_box("www.example.com")));
            black_box(first_label(black_box("localhost")));
            black_box(first_label(black_box("a.b.c.d.e.f.example.com.")));
        });
    });
}

fn bench_base_domain_from_any(c: &mut Criterion) {
    let inputs = [
        "https://www.example.co.uk/path?q=1",
        "mainr.chilipiper.com",
        "https://staging.mainr.com:8080/resource",
    ];

    c.bench_function("base_domain_from_any: 3 inputs", |b| {
        b.iter(|| {
            for input in &inputs {
                black_box(base_domain_from_any(black_box(input)));
            }
        });
    });
}

criterion_group!(
    benches,
    bench_command_message_creation,
    bench_command_message_with_session,
    bench_try_send_fast_path,
    bench_command_future_creation,
    bench_event_listeners_dispatch,
    bench_command_chain_polling,
    bench_oneshot_roundtrip,
    bench_concurrent_independent_channels,
    bench_concurrent_shared_channel,
    bench_notify_wakeup,
    bench_ws_cmd_channel_throughput,
    bench_ws_method_call_serialization,
    bench_ws_cmd_backpressure,
    bench_host_and_rest,
    bench_host_contains_label_icase,
    bench_base_domain_from_host,
    bench_host_is_subdomain_of,
    bench_rel_for_ignore_script,
    bench_first_label,
    bench_base_domain_from_any,
);
criterion_main!(benches);
