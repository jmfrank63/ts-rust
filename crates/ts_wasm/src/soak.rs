//! Memory soaks of the multi-run flow (feature `soak`; measurement, not
//! CI): the live bytes of the allocator (`test_alloc::LIVE`) and the symbol
//! index spans (`program::symbol_index_spans`) across many runs of one
//! instance, for five guest patterns. Run them alone, as `#[ignore]` tests:
//!
//! ```text
//! cargo test -p ts_wasm --release --features soak --target wasm32-wasip1 --no-run
//! node scripts/wasm/soak-runner.mjs <test.wasm> --ignored --nocapture --test-threads=1 soak::
//! cargo test -p ts_wasm --release --features soak -- --ignored --nocapture --test-threads=1 soak::
//! ```
//!
//! (the last line is the native run). `LIVE` counts what the allocator
//! holds, so a steady rise is retention, not fragmentation. A wasm soak of
//! 300 runs or more fails when its slope passes the cap that its test
//! passes to `soak`: a regression check, not a claim that the flow holds
//! no bytes per run.
//!
//! `SOAK_TRACE=1` also records the allocation site of each allocation made
//! in a window of runs. On wasm the runner keeps a JS stack of each one
//! (the `soak` imports, named by the module's name section) and prints the
//! growth per site between the middle and the end of the window; natively
//! a Rust backtrace, and the report is the traced allocations still live at
//! the end. Knobs: `SOAK_RUNS` (default 400), `SOAK_WINDOW` (runs traced,
//! default a tenth), `SOAK_TEXT` (text diagnostics instead of JSON).
#![allow(unsafe_code)]

use std::sync::atomic::AtomicBool;

/// The allocator hook (`test_alloc`).
#[inline]
pub fn on_alloc(ptr: *mut u8, size: usize) {
    #[cfg(not(target_family = "wasm"))]
    trace::on_alloc(ptr, size);
    #[cfg(target_family = "wasm")]
    wasm_trace::on_alloc(ptr, size);
}

/// The free hook (`test_alloc`).
#[inline]
pub fn on_dealloc(ptr: *mut u8) {
    #[cfg(not(target_family = "wasm"))]
    trace::on_dealloc(ptr);
    #[cfg(target_family = "wasm")]
    wasm_trace::on_dealloc(ptr);
}

/// Set while the soak records allocation sites (native only).
static TRACING: AtomicBool = AtomicBool::new(false);

/// The wasm tracer: the JS runner (`scripts/wasm/soak-runner.mjs`) keeps
/// a stack of each traced allocation (`new Error().stack`, named by the
/// module's name section). Only with `SOAK_TRACE`: a binary run by the
/// plain test runner never calls the imports.
#[cfg(target_family = "wasm")]
mod wasm_trace {
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
            let stack = Backtrace::force_capture();
            let mut sites = SITES.lock().unwrap_or_else(PoisonError::into_inner);
            sites
                .get_or_insert_with(HashMap::new)
                .insert(ptr as usize, (size, stack));
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
    /// their site in the goport frames, largest group first.
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
                // The function line of each frame of the goport crates (a
                // build with no debuginfo has no `at` lines).
                .filter_map(|line| line.split_once(": ").map(|(_, name)| name))
                .filter(|name| name.contains("goport") || name.contains("ts_wasm"))
                .filter(|name| !name.contains("::soak::") && !name.contains("test_alloc"))
                .collect();
            let key = frames
                .iter()
                .take(6)
                .copied()
                .collect::<Vec<_>>()
                .join("\n    ");
            let entry = groups.entry(key).or_default();
            entry.0 += size;
            entry.1 += 1;
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_key(|group| std::cmp::Reverse(group.1.0));
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

mod tests {
    use super::TRACING;
    use crate::test_alloc::LIVE;
    use crate::{FLAG_DIAGNOSTICS_JSON, host, run};
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;
    use ts_goport::program::symbol_index_spans;

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

    /// The slope of `samples` over their second half, per run.
    #[allow(clippy::cast_precision_loss)]
    fn slope(samples: &[usize]) -> f64 {
        let half = samples.len() / 2;
        let last = samples.len() - 1;
        (samples[last] as f64 - samples[half] as f64) / (last - half) as f64
    }

    /// Runs `runs` runs; `world(i)` sets the host files of run `i`. Prints
    /// the live bytes at checkpoints, their slope over the second half and
    /// the symbol index spans, and on wasm fails when the slope passes
    /// `cap` bytes a run (from 300 runs on: a shorter soak has no settled
    /// half).
    fn soak(name: &str, cap: f64, clean: bool, world: impl Fn(usize)) {
        let runs = env("SOAK_RUNS").unwrap_or(400).max(4);
        let flags = if std::env::var_os("SOAK_TEXT").is_some() {
            0
        } else {
            FLAG_DIAGNOSTICS_JSON
        };
        let trace = std::env::var_os("SOAK_TRACE").is_some();
        let window = (runs / 2)..(runs / 2 + env("SOAK_WINDOW").unwrap_or(runs / 10).max(2));
        let mut live = Vec::with_capacity(runs);
        let mut lineage = Vec::with_capacity(runs);
        let mut tables = Vec::with_capacity(runs);
        let mut checker = 0;
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
            if i == 0 && clean && flags & FLAG_DIAGNOSTICS_JSON != 0 {
                assert_eq!(reply, "[]", "the guest must check clean");
            }
            live.push(LIVE.load(Ordering::Relaxed));
            let spans = symbol_index_spans();
            lineage.push(spans.lineage_symbols);
            tables.push(spans.lineage_tables);
            checker = checker.max(spans.checker_symbols);
        }
        let mib = |bytes: usize| {
            #[allow(clippy::cast_precision_loss)]
            let mib = bytes as f64 / 1_048_576.0;
            mib
        };
        let checkpoints: Vec<String> = [0, 1, 9, 49, runs / 2, runs - 1]
            .iter()
            .map(|&i| format!("r{}={:.2}MiB", i + 1, mib(live[i.min(runs - 1)])))
            .collect();
        let bytes_per_run = slope(&live);
        println!(
            "SOAK {name}: runs={runs} {} | slope(second half)={bytes_per_run:.0} B/run | \
             symbol index: lineage +{:.1}/run (end {}), checker max {checker} | \
             table index: lineage +{:.1}/run (end {})",
            checkpoints.join(" "),
            slope(&lineage),
            lineage[runs - 1],
            slope(&tables),
            tables[runs - 1],
        );
        #[cfg(not(target_family = "wasm"))]
        if trace {
            super::trace::report(window.len());
        }
        #[cfg(target_family = "wasm")]
        if trace {
            super::wasm_trace::print(window.len() - window.len() / 2);
        }
        if cfg!(target_family = "wasm") && runs >= 300 {
            assert!(
                bytes_per_run <= cap,
                "{name}: {bytes_per_run:.0} B/run is over the cap of {cap} B/run"
            );
        }
    }

    fn env(name: &str) -> Option<usize> {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
    }

    // The caps are about 1.5 to 2 times the slopes measured after the
    // config-parse reuse and the per-program synthetic flow free (wasm, 600
    // runs): a: 0, a2: 2.8 KB, b: 0.8 KB, c: 5.2 KB, d: 1.0 KB. What is left
    // is the symbol link pages of the growing index space and, in c, the
    // config parse of each new config text (see `multirun::CONFIG_PARSES`).

    /// (a) The same request over the same files each run (replayed).
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_a_identical() {
        let text = guest(0);
        soak("a-identical", 256.0, true, |_| {
            host::test_host::set_files([tsconfig(&["a.ts"]), ("/p/a.ts".into(), text.clone())]);
        });
    }

    /// (a2) The 300-line file alternates between two texts: no replay, and
    /// the parse cache keeps one parse per path, so each run parses and
    /// binds the file again (a large edit each run).
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_a2_alternating() {
        let texts = [guest(0), guest(1)];
        soak("a2-alternating", 5_000.0, true, |i| {
            host::test_host::set_files([
                tsconfig(&["a.ts"]),
                ("/p/a.ts".into(), texts[i % 2].clone()),
            ]);
        });
    }

    /// (b) One small file whose text is new each run.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_b_edited() {
        let base = guest(0);
        soak("b-edited", 2_000.0, true, |i| {
            host::test_host::set_files([
                tsconfig(&["a.ts", "b.ts"]),
                ("/p/a.ts".into(), base.clone()),
                (
                    "/p/b.ts".into(),
                    format!(
                        "import {{ value0 }} from './a';\nexport const b: number = value0 + {i};\n"
                    ),
                ),
            ]);
        });
    }

    /// (c) A new file name each run (so a new config text); the previous
    /// file is gone.
    #[test]
    #[ignore = "memory soak; run alone"]
    fn soak_c_new_name() {
        let base = guest(0);
        soak("c-new-name", 9_000.0, true, |i| {
            let name = format!("n{i}.ts");
            host::test_host::set_files([
                tsconfig(&["a.ts", &name]),
                ("/p/a.ts".into(), base.clone()),
                (
                    format!("/p/{name}"),
                    format!(
                        "import {{ value0 }} from './a';\nexport const c{i}: number = value0;\n"
                    ),
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
        soak("d-errors", 2_000.0, false, |i| {
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
}
