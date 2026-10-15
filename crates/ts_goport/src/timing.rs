//! TEMPORARY perf instrumentation for the phase-9 incremental-recheck spike.
//! Gated by `timing::enable` (the `ts_debug_timings` wasm export); a run's
//! spans print as one `TIMINGS` stderr line at `report`. Not in the shipped
//! artifact: the production build comes from the branch without this module.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

thread_local! {
    static SPANS: RefCell<Vec<(&'static str, f64)>> = const { RefCell::new(Vec::new()) };
}

/// A wall-time span. Record nothing while the flag is off (`Span::new`
/// returns None, and `Option<Span>` still drops without effect).
pub struct Span(&'static str, Instant);

impl Span {
    #[must_use]
    pub fn new(name: &'static str) -> Option<Span> {
        enabled().then_some(Span(name, Instant::now()))
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let ms = self.1.elapsed().as_secs_f64() * 1000.0;
        SPANS.with(|spans| spans.borrow_mut().push((self.0, ms)));
    }
}

/// Parse-cache counters of the multi-run host (`program::multirun`).
pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
pub static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent parsing on a miss.
pub static CACHE_PARSE_US: AtomicU64 = AtomicU64::new(0);

/// Semantic-reuse counters of the run (phase 9 step 2).
pub static REUSE_FILES: AtomicU64 = AtomicU64::new(0);
pub static RECHECK_FILES: AtomicU64 = AtomicU64::new(0);
pub static CHANGED_FILES: AtomicU64 = AtomicU64::new(0);
pub static CACHE_PATHS: AtomicU64 = AtomicU64::new(0);

/// Cumulative check-phase accumulators (phase 9): per-file diagnostics
/// closures of `emit_files_and_report_errors`. Microseconds, and the file
/// counts they covered.
pub static BIND_US: AtomicU64 = AtomicU64::new(0);
pub static BIND_FILES: AtomicU64 = AtomicU64::new(0);
pub static SEMANTIC_US: AtomicU64 = AtomicU64::new(0);
pub static SEMANTIC_FILES: AtomicU64 = AtomicU64::new(0);
pub static GLOBAL_US: AtomicU64 = AtomicU64::new(0);
pub static OPTIONS_US: AtomicU64 = AtomicU64::new(0);
pub static DECLARATION_US: AtomicU64 = AtomicU64::new(0);
pub static SYNTACTIC_US: AtomicU64 = AtomicU64::new(0);

/// Adds microseconds to an accumulator.
pub fn add_us(cell: &AtomicU64, elapsed: std::time::Instant) {
    cell.fetch_add(
        u64::try_from(elapsed.elapsed().as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

fn ms(cell: &AtomicU64) -> f64 {
    f64::from(u32::try_from(cell.load(Ordering::Relaxed)).unwrap_or(u32::MAX)) / 1000.0
}

/// One `TIMINGS` line per run: every span of the run, then the counters
/// (reset), so the line describes exactly the run that just ended.
pub fn report() {
    if !enabled() {
        return;
    }
    SPANS.with(|spans| {
        let mut spans = spans.borrow_mut();
        let line = spans
            .drain(..)
            .map(|(name, ms)| format!("{name}={ms:.1}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("TIMINGS {line}");
    });
    let hits = CACHE_HITS.swap(0, Ordering::Relaxed);
    let misses = CACHE_MISSES.swap(0, Ordering::Relaxed);
    let parse_us = CACHE_PARSE_US.swap(0, Ordering::Relaxed);
    eprintln!(
        "TIMINGS_CACHE hits={hits} misses={misses} parseMs={:.2}",
        f64::from(u32::try_from(parse_us).unwrap_or(u32::MAX)) / 1000.0
    );
    eprintln!(
        "TIMINGS_REUSE reuse={} recheck={} changed={} cachePaths={}",
        REUSE_FILES.swap(0, Ordering::Relaxed),
        RECHECK_FILES.swap(0, Ordering::Relaxed),
        CHANGED_FILES.swap(0, Ordering::Relaxed),
        CACHE_PATHS.swap(0, Ordering::Relaxed),
    );
    if SEMANTIC_US.load(Ordering::Relaxed) != 0 || BIND_US.load(Ordering::Relaxed) != 0 {
        eprintln!(
            "TIMINGS_CHECK bindMs={:.1}({}) semanticMs={:.1}({}) globalMs={:.1} optionsMs={:.1} synMs={:.1} declMs={:.1}",
            ms(&BIND_US),
            BIND_FILES.load(Ordering::Relaxed),
            ms(&SEMANTIC_US),
            SEMANTIC_FILES.load(Ordering::Relaxed),
            ms(&GLOBAL_US),
            ms(&OPTIONS_US),
            ms(&SYNTACTIC_US),
            ms(&DECLARATION_US),
        );
        BIND_US.store(0, Ordering::Relaxed);
        BIND_FILES.store(0, Ordering::Relaxed);
        SEMANTIC_US.store(0, Ordering::Relaxed);
        SEMANTIC_FILES.store(0, Ordering::Relaxed);
        GLOBAL_US.store(0, Ordering::Relaxed);
        OPTIONS_US.store(0, Ordering::Relaxed);
        DECLARATION_US.store(0, Ordering::Relaxed);
        SYNTACTIC_US.store(0, Ordering::Relaxed);
    }
}
