//! Cost of the navigation-deadline lookup on the handler's poll path.
//!
//! The driver reads "when does the next navigation time out?" on every poll of
//! its hottest loop. The original shape answered that by scanning every target
//! and taking the minimum; the current shape answers from a cache that is
//! recomputed only when a navigation actually changed. This measures both
//! against target count so the difference is a number rather than a reading of
//! the code.
//!
//! The handler needs a live socket to construct, so the map is modelled here
//! with the same container (`hashbrown::HashMap`) and the same per-entry work:
//! reach through a struct for an `Option<Instant>` and fold to the minimum.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use hashbrown::HashMap;
use std::time::{Duration, Instant};

/// Stand-in for `Target`, holding a frame manager with an optional deadline.
struct TargetStub {
    frame_manager: FrameManagerStub,
}

struct FrameManagerStub {
    navigation: Option<((), Instant)>,
    deadline_changed: bool,
}

impl FrameManagerStub {
    #[inline]
    fn next_navigation_deadline(&self) -> Option<Instant> {
        self.navigation.as_ref().map(|(_, deadline)| *deadline)
    }

    #[inline]
    fn take_nav_deadline_changed(&mut self) -> bool {
        std::mem::take(&mut self.deadline_changed)
    }
}

/// One target in `count` carries an in-flight navigation, which is the usual
/// shape: a handler drives many targets and navigates one at a time.
fn targets(count: usize) -> HashMap<u32, TargetStub> {
    let base = Instant::now();
    (0..count)
        .map(|n| {
            let navigation = if n == count / 2 {
                Some(((), base + Duration::from_secs(30)))
            } else {
                None
            };
            (
                n as u32,
                TargetStub {
                    frame_manager: FrameManagerStub {
                        navigation,
                        deadline_changed: false,
                    },
                },
            )
        })
        .collect()
}

/// Every target carries one, the worst case for the scan.
fn saturated_targets(count: usize) -> HashMap<u32, TargetStub> {
    let base = Instant::now();
    (0..count)
        .map(|n| {
            (
                n as u32,
                TargetStub {
                    frame_manager: FrameManagerStub {
                        navigation: Some(((), base + Duration::from_secs(30 + n as u64))),
                        deadline_changed: false,
                    },
                },
            )
        })
        .collect()
}

#[inline]
fn scan(map: &HashMap<u32, TargetStub>) -> Option<Instant> {
    map.values()
        .filter_map(|target| target.frame_manager.next_navigation_deadline())
        .min()
}

#[inline]
fn cached(
    dirty: &mut bool,
    cache: &mut Option<Instant>,
    map: &HashMap<u32, TargetStub>,
) -> Option<Instant> {
    if *dirty {
        *dirty = false;
        *cache = scan(map);
    }
    *cache
}

/// The scan the poll path used to run on every poll.
fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("nav_deadline/scan_every_poll");
    for count in [1usize, 8, 64, 256] {
        let map = targets(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| black_box(scan(black_box(&map))));
        });
    }
    group.finish();
}

/// Worst case for the scan: every target has a deadline.
fn bench_scan_saturated(c: &mut Criterion) {
    let mut group = c.benchmark_group("nav_deadline/scan_all_navigating");
    for count in [1usize, 8, 64, 256] {
        let map = saturated_targets(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| black_box(scan(black_box(&map))));
        });
    }
    group.finish();
}

/// The cached read the poll path runs now, in the steady state where no
/// navigation changed since the last recompute.
fn bench_cached(c: &mut Criterion) {
    let mut group = c.benchmark_group("nav_deadline/cached_read");
    for count in [1usize, 8, 64, 256] {
        let map = targets(count);
        let mut dirty = true;
        let mut cache = None;
        // Prime the cache the way the first poll does.
        let _ = cached(&mut dirty, &mut cache, &map);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(cached(
                    black_box(&mut dirty),
                    black_box(&mut cache),
                    black_box(&map),
                ))
            });
        });
    }
    group.finish();
}

/// What the cache costs to keep honest: one bool read per target, taken while
/// the driver walks a target it is already holding. The walk itself is not new,
/// so this is the whole added cost, and it replaces a scan of the entire map.
fn bench_dirty_drain(c: &mut Criterion) {
    let mut map = targets(1);
    c.bench_function("nav_deadline/dirty_drain_one_target", |b| {
        b.iter(|| {
            let target = map.get_mut(&0u32).map(|t| &mut t.frame_manager);
            black_box(match target {
                Some(fm) => fm.take_nav_deadline_changed(),
                None => false,
            })
        });
    });
}

criterion_group!(
    benches,
    bench_scan,
    bench_scan_saturated,
    bench_cached,
    bench_dirty_drain
);
criterion_main!(benches);
