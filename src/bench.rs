//! A release-mode benchmark over a real dataset, and the parity dump that proves a refactor ranked nothing
//! differently:
//!
//!   DEN_BENCH_DATA=<dir with dataset.meta.json> DEN_BENCH_OUT=<file> \
//!     cargo test --release -p den-atlas bench_real_dataset -- --ignored --nocapture
//!
//! It times the index load (and the heap it keeps), More Like This for unrestricted and Kid policies with
//! the memos cleared between rounds, You Might Also Like under a Kid policy, search with a person's name and
//! the display index's trigram search. Every answer is written to `DEN_BENCH_OUT`, so two builds' dumps can
//! be compared byte for byte.

use crate::maturity::{KidAudience, Policy, UnknownRatings};
use crate::queries::Indexes;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

/// The test binary's allocator: the system's, counting the bytes currently allocated, so the bench can say
/// how much heap a load keeps.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// CPU time rather than wall time: the bench runs on shared machines whose load swings wall time threefold
/// between identical runs, while the CPU a call burns stays put.
#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

extern "C" {
    fn clock_gettime(clock: i32, out: *mut Timespec) -> i32;
}

#[cfg(target_os = "macos")]
const THREAD_CPU: i32 = 16;
#[cfg(not(target_os = "macos"))]
const THREAD_CPU: i32 = 3;
#[cfg(target_os = "macos")]
const PROCESS_CPU: i32 = 12;
#[cfg(not(target_os = "macos"))]
const PROCESS_CPU: i32 = 2;

fn cpu(clock: i32) -> Duration {
    let mut t = Timespec { sec: 0, nsec: 0 };
    // SAFETY: `t` is a valid, writable timespec; both clocks exist on macOS and Linux.
    assert_eq!(unsafe { clock_gettime(clock, &mut t) }, 0);
    Duration::new(t.sec as u64, t.nsec as u32)
}

/// A single-threaded call's CPU time.
struct Clock(Duration);

impl Clock {
    fn start() -> Clock {
        Clock(cpu(THREAD_CPU))
    }

    fn elapsed(&self) -> Duration {
        cpu(THREAD_CPU) - self.0
    }
}

const ROUNDS: usize = 5;
const SEEDS: usize = 200;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

fn kid() -> Policy {
    Policy {
        maximum_age: Some(12),
        unknown_ratings: UnknownRatings::Block,
        region: None,
        kid_audience: Some(KidAudience::ChildOrTeen),
    }
}

fn age_only() -> Policy {
    Policy { maximum_age: Some(15), unknown_ratings: UnknownRatings::Block, ..Policy::default() }
}

/// Every `n`th store row with a plot vector, as seeds: spread over films and series, fixed for a store.
fn seeds(indexes: &Indexes) -> Vec<(den_index::MediaType, u32)> {
    let view = indexes.store.view();
    let keys = view.per_row::<u64>("keys").expect("keys");
    let step = (keys.len() / SEEDS).max(1);
    keys.iter()
        .step_by(step)
        .map(|&packed| {
            let media =
                if packed >> 32 == 1 { den_index::MediaType::Tv } else { den_index::MediaType::Movie };
            (media, packed as u32)
        })
        .filter(|&(media, id)| indexes.plot.row_of(id, media).is_some())
        .take(SEEDS)
        .collect()
}

#[test]
#[ignore = "release-mode benchmark over a real dataset: set DEN_BENCH_DATA"]
fn bench_real_dataset() {
    let Ok(dir) = std::env::var("DEN_BENCH_DATA") else {
        eprintln!("SKIP: set DEN_BENCH_DATA to a directory holding dataset.meta.json");
        return;
    };
    let dataset = crate::dataset::Dataset::load(std::path::Path::new(&dir)).expect("dataset");
    let mut out = String::new();

    // The load: median wall time, and the heap the loaded indexes keep (filter index and corpus included,
    // since the load builds both).
    let mut loads = Vec::new();
    let mut heap = 0isize;
    let mut kept = None;
    for _ in 0..ROUNDS {
        drop(kept.take());
        let before = LIVE.load(Ordering::Relaxed);
        let started = cpu(PROCESS_CPU);
        let indexes = crate::queries::load_for_tools(&dataset).expect("indexes");
        loads.push(cpu(PROCESS_CPU) - started);
        heap = LIVE.load(Ordering::Relaxed) - before;
        kept = Some(indexes);
    }
    let indexes = kept.expect("loaded");
    eprintln!("bench: load median {} CPU · heap kept {:.1} MB", ms(median(loads)), heap as f64 / 1e6);

    // Who holds that heap: each part built again on its own, its live bytes measured while it is held.
    {
        let view = indexes.store.view();
        let mapped =
            || std::sync::Arc::clone(&indexes.store.store) as std::sync::Arc<dyn den_index::StoreBytes>;
        let held = |name: &str, build: &dyn Fn() -> Box<dyn std::any::Any>| {
            let before = LIVE.load(Ordering::Relaxed);
            let part = build();
            eprintln!("bench: heap {name} {:.1} MB", (LIVE.load(Ordering::Relaxed) - before) as f64 / 1e6);
            drop(part);
        };
        held("facts", &|| Box::new(crate::facts::Facts::from_store(&view)));
        held("plot index", &|| Box::new(den_index::Index::from_store_plot(&view, mapped())));
        held("premise index", &|| Box::new(den_index::Index::from_store_premise(&view, mapped())));
        held("cards", &|| Box::new(crate::plotrows::cards_from_store(&view)));
        held("plot facets", &|| Box::new(crate::plotrows::PlotFacets::from_store(&view)));
        held("filter index", &|| Box::new(crate::filter::FilterIndex::build(&indexes)));
        eprintln!(
            "bench: heap display index ~{:.1} MB (approx_bytes)",
            indexes.display.as_ref().map_or(0, |d| d.approx_bytes()) as f64 / 1e6
        );
    }

    let seeds = seeds(&indexes);
    eprintln!("bench: {} seeds", seeds.len());

    // A restricted policy's admission bitset is worked out on its first request and kept; its one-off cost.
    for (name, policy) in [("kid", kid()), ("age15", age_only())] {
        let started = Clock::start();
        let admitted: u32 = indexes.admission(policy).iter().map(|w| w.count_ones()).sum();
        eprintln!("bench: admission ({name}) built in {}, {admitted} rows admitted", ms(started.elapsed()));
    }

    // More Like This, both rows, per policy, the memo cleared before every round so each call ranks.
    for (name, policy) in [("unrestricted", Policy::default()), ("kid", kid()), ("age15", age_only())] {
        let mut calls = Vec::new();
        for round in 0..ROUNDS {
            indexes.clear_memos();
            for &(media, id) in &seeds {
                let started = Clock::start();
                let (one, mixed) = indexes.more_like_this_rows_with_policy(id, media, policy);
                calls.push(started.elapsed());
                if round == 0 {
                    let _ = writeln!(out, "mlt {name} {media:?}:{id} one {one:?} mixed {mixed:?}");
                }
            }
        }
        eprintln!("bench: more like this ({name}) median {} per seed", ms(median(calls)));
    }

    // You Might Also Like under a Kid policy: structural affinity or fan picks, eligibility on each.
    let mut calls = Vec::new();
    for round in 0..ROUNDS {
        indexes.clear_memos();
        for &(media, id) in &seeds {
            let started = Clock::start();
            let row = indexes.you_might_also_like_with_policy(id, media, true, kid());
            calls.push(started.elapsed());
            if round == 0 {
                let _ = writeln!(out, "ymal kid {media:?}:{id} {row:?}");
            }
        }
    }
    eprintln!("bench: you might also like (kid) median {} per seed", ms(median(calls)));

    // Search naming people; vectors are not available offline, so this is the parse and the lanes.
    let people = [
        "christopher nolan",
        "films by bong joon ho",
        "meryl streep",
        "tom hanks movies",
        "david simon",
        "denis villeneuve sci-fi",
        "hayao miyazaki",
        "nicole kidman series",
    ];
    for (name, policy) in [("unrestricted", Policy::default()), ("kid", kid())] {
        let mut calls = Vec::new();
        for round in 0..ROUNDS {
            for query in people {
                let started = Clock::start();
                let parsed = crate::search::parse(query, &indexes);
                let answer =
                    crate::search::answer_with_policy(&indexes, None, &parsed, None, None, 0, 50, policy);
                calls.push(started.elapsed());
                if round == 0 {
                    let _ = writeln!(out, "search {name} {query:?} {answer}");
                }
            }
        }
        eprintln!("bench: person search ({name}) median {} per query", ms(median(calls)));
    }

    // The display index's trigram search alone.
    let titles =
        ["the godfather", "avengrs", "parasite", "breaking bad", "lord of the rings", "star", "amelie"];
    let display = indexes.display.as_ref().expect("display index");
    let mut calls = Vec::new();
    for round in 0..ROUNDS * 4 {
        for query in titles {
            let started = Clock::start();
            let hits = display.search_with(query, None, 200, den_titlesearch::DEFAULT_MIN_COVERAGE);
            calls.push(started.elapsed());
            if round == 0 {
                let ids: Vec<(u32, String)> =
                    hits.iter().map(|h| (h.tmdb_id, format!("{:?}", h.media_type))).collect();
                let _ = writeln!(out, "trigram {query:?} {ids:?}");
            }
        }
    }
    eprintln!("bench: trigram search median {} per query", ms(median(calls)));

    if let Ok(path) = std::env::var("DEN_BENCH_OUT") {
        std::fs::write(&path, out).expect("write the parity dump");
        eprintln!("bench: parity dump written to {path}");
    }
}
