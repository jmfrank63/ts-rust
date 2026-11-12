//! Memory soaks of the multi-run flow (measurement, not CI): the live bytes
//! of the allocator across many runs of one process, for three guest
//! patterns. Run them alone, as `#[ignore]` tests:
//!
//!   cargo test -p ts_wasm --release soak -- --ignored --nocapture --test-threads=1
//!
//! `LIVE` counts what the allocator holds (allocated, not freed), so a
//! steady rise is retention, not fragmentation. On native builds,
//! `SOAK_TRACE=1` also records a backtrace of each allocation made during a
//! window of runs and prints the ones still live at the end, grouped by
//! call site.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The bytes that the process holds: allocated and not yet freed.
pub static LIVE: AtomicUsize = AtomicUsize::new(0);

/// `System`, counting in `LIVE`.
pub struct Counting;

// SAFETY: each call goes to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the `GlobalAlloc::alloc` contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
            #[cfg(not(target_family = "wasm"))]
            trace::on_alloc(ptr, layout.size());
            #[cfg(target_family = "wasm")]
            wasm_trace::on_alloc(ptr, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        #[cfg(not(target_family = "wasm"))]
        trace::on_dealloc(ptr);
        #[cfg(target_family = "wasm")]
        wasm_trace::on_dealloc(ptr);
        // SAFETY: the caller keeps the `GlobalAlloc::dealloc` contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the `GlobalAlloc::realloc` contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            #[cfg(not(target_family = "wasm"))]
            {
                trace::on_dealloc(ptr);
                trace::on_alloc(new, new_size);
            }
            #[cfg(target_family = "wasm")]
            {
                wasm_trace::on_dealloc(ptr);
                wasm_trace::on_alloc(new, new_size);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Set while the soak records allocation sites (native only).
static TRACING: AtomicBool = AtomicBool::new(false);

/// The wasm tracer: the JS runner (`scripts/wasm/soak-runner.mjs`) keeps
/// a stack of each traced allocation (`new Error().stack`, named by the
/// module's name section). Only with `SOAK_TRACE`: a binary run by the
/// plain test runner never calls the imports.
#[cfg(target_family = "wasm")]
pub mod wasm_trace {
    use super::TRACING;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[link(wasm_import_module = "soak")]
    unsafe extern "C" {
        fn alloc(ptr: usize, size: usize);
        fn free(ptr: usize);
        fn report(runs: usize);
        fn snapshot();
    }

    static ANY: AtomicBool = AtomicBool::new(false);

    pub fn on_alloc(ptr: *mut u8, size: usize) {
        if TRACING.load(Ordering::Relaxed) {
            ANY.store(true, Ordering::Relaxed);
            // SAFETY: the runner only records the two numbers.
            unsafe { alloc(ptr as usize, size) };
        }
    }

    pub fn on_dealloc(ptr: *mut u8) {
        if ANY.load(Ordering::Relaxed) {
            // SAFETY: the runner only forgets the address.
            unsafe { free(ptr as usize) };
        }
    }

    pub fn mark() {
        // SAFETY: the runner records its per-site totals.
        unsafe { snapshot() };
    }

    pub fn print(runs: usize) {
        // SAFETY: the runner prints its records.
        unsafe { report(runs) };
    }
}

#[cfg(not(target_family = "wasm"))]
mod trace {
    use super::TRACING;
    use std::backtrace::Backtrace;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, PoisonError};

    thread_local! {
        /// Set while this thread is inside the tracer, so the tracer's own
        /// allocations are not traced.
        static INSIDE: Cell<bool> = const { Cell::new(false) };
    }

    /// The traced allocations that are still live: address -> (size, site).
    pub static SITES: Mutex<Option<HashMap<usize, (usize, Backtrace)>>> = Mutex::new(None);

    /// Set once anything was traced: until then a free skips the map.
    static ANY: AtomicBool = AtomicBool::new(false);

    fn guarded(f: impl FnOnce()) {
        let entered = INSIDE
            .try_with(|inside| !inside.replace(true))
            .unwrap_or(false);
        if entered {
            f();
            let _ = INSIDE.try_with(|inside| inside.set(false));
        }
    }

    pub fn on_alloc(ptr: *mut u8, size: usize) {
        if !TRACING.load(Ordering::Relaxed) {
            return;
        }
        guarded(|| {
            let site = Backtrace::force_capture();
            let mut sites = SITES.lock().unwrap_or_else(PoisonError::into_inner);
            sites
                .get_or_insert_with(HashMap::new)
                .insert(ptr as usize, (size, site));
            ANY.store(true, Ordering::Relaxed);
        });
    }

    pub fn on_dealloc(ptr: *mut u8) {
        if !ANY.load(Ordering::Relaxed) {
            return;
        }
        guarded(|| {
            let mut sites = SITES.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(sites) = sites.as_mut() {
                sites.remove(&(ptr as usize));
            }
        });
    }

    /// Prints the live traced allocations, grouped by the first frames of
    /// their site in ts_goport/ts_wasm, largest group first.
    pub fn report(runs_traced: usize) {
        INSIDE.with(|inside| inside.set(true));
        let taken = SITES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let mut groups: HashMap<String, (usize, usize)> = HashMap::new();
        for (_, (size, site)) in taken {
            let text = format!("{site}");
            let frames: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("at ") && line.contains("crates/"))
                .collect();
            let key = frames.iter().take(6).copied().collect::<Vec<_>>().join("\n    ");
            let entry = groups.entry(key).or_default();
            entry.0 += size;
            entry.1 += 1;
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        let total: usize = groups.iter().map(|group| group.1.0).sum();
        println!(
            "SOAK-TRACE survivors of {runs_traced} traced runs: {total} B ({} B/run) in {} sites",
            total / runs_traced.max(1),
            groups.len()
        );
        for (key, (bytes, count)) in groups.iter().take(25) {
            println!(
                "--- {bytes} B ({} B/run) in {count} allocations\n    {key}",
                bytes / runs_traced.max(1)
            );
        }
        INSIDE.with(|inside| inside.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::{LIVE, TRACING};
    use crate::{FLAG_DIAGNOSTICS_JSON, host, run};
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;

    /// A guest of about 300 lines: interfaces, classes, generics, unions.
    fn guest(seed: usize) -> String {
        let mut text = String::new();
        for i in 0..20 {
            let _ = write!(
                text,
                "export interface Shape{i} {{ kind: 'k{i}'; size: number; tags: string[] }}\n\
                 export type Maybe{i}<T> = T | undefined | {{ err: string }};\n\
                 export class Box{i}<T extends Shape{i}> {{\n\
                 \x20 private items: T[] = [];\n\
                 \x20 add(item: T): this {{ this.items.push(item); return this; }}\n\
                 \x20 total(): number {{ return this.items.reduce((s, x) => s + x.size, {seed}); }}\n\
                 \x20 find(tag: string): Maybe{i}<T> {{ return this.items.find(x => x.tags.includes(tag)); }}\n\
                 }}\n\
                 export function make{i}(n: number): Box{i}<Shape{i}> {{\n\
                 \x20 const b = new Box{i}<Shape{i}>();\n\
                 \x20 for (let j = 0; j < n; j++) b.add({{ kind: 'k{i}', size: j * {i}, tags: [String(j)] }});\n\
                 \x20 return b;\n\
                 }}\n\
                 export const value{i}: number = make{i}({i}).total();\n\
                 \n"
            );
        }
        text
    }

    fn tsconfig(files: &[&str]) -> (String, String) {
        let list = files
            .iter()
            .map(|file| format!("\"{file}\""))
            .collect::<Vec<_>>()
            .join(",");
        (
            "/p/tsconfig.json".to_string(),
            format!(
                r#"{{"compilerOptions":{{"noEmit":true,"strict":true,"types":[],"lib":["es2022"]}},"files":[{list}]}}"#
            ),
        )
    }

    /// Runs `runs` runs; `world(i)` sets the host files of run `i`. Prints
    /// the live bytes at checkpoints and the slope over the second half.
    fn soak(name: &str, runs: usize, flags: u32, world: impl Fn(usize)) {
        let trace = std::env::var_os("SOAK_TRACE").is_some();
        let len = std::env::var("SOAK_WINDOW")
            .ok()
            .and_then(|len| len.parse().ok())
            .unwrap_or(runs / 10);
        let window = (runs / 2)..(runs / 2 + len);
        let mut samples = Vec::with_capacity(runs);
        for i in 0..runs {
            world(i);
            if trace && i == window.start {
                TRACING.store(true, Ordering::SeqCst);
            }
            let (status, reply) = run(&format!("/p\0{flags}\0-p\0."));
            #[cfg(target_family = "wasm")]
            if trace && i + 1 == window.start + window.len() / 2 {
                super::wasm_trace::mark();
            }
            if trace && i + 1 == window.end {
                TRACING.store(false, Ordering::SeqCst);
            }
            assert!(status == 0 || status == 2, "run {i}: {status} {reply}");
            if i == 0 && flags & FLAG_DIAGNOSTICS_JSON != 0 && !name.starts_with("d-") {
                assert_eq!(reply, "[]", "the guest must check clean");
            }
            samples.push(LIVE.load(Ordering::Relaxed));
        }
        let at = |i: usize| samples[i.min(runs - 1)];
        let half = runs / 2;
        #[allow(clippy::cast_precision_loss)]
        let slope = (at(runs - 1) as f64 - at(half) as f64) / (runs - 1 - half) as f64;
        let checkpoints: Vec<String> = [0, 1, 9, 49, half, runs - 1]
            .iter()
            .map(|&i| format!("r{}={:.2}MiB", i + 1, at(i) as f64 / 1_048_576.0))
            .collect();
        println!(
            "SOAK {name}: runs={runs} {} | slope(second half)={:.0} B/run",
            checkpoints.join(" "),
            slope
        );
        #[cfg(not(target_family = "wasm"))]
        if trace {
            super::trace::report(window.len());
        }
        #[cfg(target_family = "wasm")]
        if trace {
            super::wasm_trace::print(window.len() - window.len() / 2);
        }
    }

    fn runs() -> usize {
        std::env::var("SOAK_RUNS")
            .ok()
            .and_then(|runs| runs.parse().ok())
            .unwrap_or(400)
    }

    fn flags() -> u32 {
        if std::env::var_os("SOAK_TEXT").is_some() {
            0
        } else {
            FLAG_DIAGNOSTICS_JSON
        }
    }

    /// (a) The same request over the same files each run.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_a_identical() {
        let text = guest(0);
        soak("a-identical", runs(), flags(), |_| {
            host::test_host::set_files([tsconfig(&["a.ts"]), ("/p/a.ts".into(), text.clone())]);
        });
    }

    /// (a') Two alternating worlds: no fingerprint replay, but every parse
    /// is a cache hit after the first two runs.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_a2_alternating() {
        let texts = [guest(0), guest(1)];
        soak("a2-alternating", runs(), flags(), |i| {
            host::test_host::set_files([
                tsconfig(&["a.ts"]),
                ("/p/a.ts".into(), texts[i % 2].clone()),
            ]);
        });
    }

    /// (b) One file whose text is new each run.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_b_edited() {
        let base = guest(0);
        soak("b-edited", runs(), flags(), |i| {
            host::test_host::set_files([
                tsconfig(&["a.ts", "b.ts"]),
                ("/p/a.ts".into(), base.clone()),
                (
                    "/p/b.ts".into(),
                    format!("import {{ value0 }} from './a';\nexport const b: number = value0 + {i};\n"),
                ),
            ]);
        });
    }

    /// (d) One file edited each run, with type errors whose messages print
    /// types (the node builder's synthetic nodes).
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_d_errors() {
        let base = guest(0);
        soak("d-errors", runs(), flags(), |i| {
            host::test_host::set_files([
                tsconfig(&["a.ts", "b.ts"]),
                ("/p/a.ts".into(), base.clone()),
                (
                    "/p/b.ts".into(),
                    format!(
                        "import {{ make0, Box1, Shape1 }} from './a';\n\
                         export const b{i}: string = make0({i});\n\
                         export const c: Box1<Shape1> = make0(1);\n\
                         export const d: {{ x: number; y: Array<Map<string, Shape1>> }} = {{ x: 'no', y: [] }};\n"
                    ),
                ),
            ]);
        });
    }

    /// (c) A new file name each run; the previous one is gone.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_c_new_name() {
        let base = guest(0);
        soak("c-new-name", runs(), flags(), |i| {
            let name = format!("n{i}.ts");
            host::test_host::set_files([
                tsconfig(&["a.ts", &name]),
                ("/p/a.ts".into(), base.clone()),
                (
                    format!("/p/{name}"),
                    format!("import {{ value0 }} from './a';\nexport const c{i}: number = value0;\n"),
                ),
            ]);
        });
    }
}
