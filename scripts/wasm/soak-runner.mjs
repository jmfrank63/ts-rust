// Runs the wasm32-wasip1 test binary of crates/ts_wasm built with the `soak`
// feature: test-runner.mjs plus the `soak` imports of src/soak.rs, which keep
// a JS stack for each allocation the soak traces (`SOAK_TRACE=1`) and print
// the growth per call site between the middle and the end of the window.
// The stack names come from the module's name section (a release test
// binary keeps it).
//
// usage: cargo test -p ts_wasm --release --features soak --target wasm32-wasip1 --no-run
//        node scripts/wasm/soak-runner.mjs <test.wasm> --ignored --nocapture --test-threads=1 soak::
// env:   SOAK_DEPTH  JS stack frames captured (default 30)
//        SOAK_GROUP  frames that key a call site (default 8)
//        SOAK_TOP    sites printed (default 30)
//        and the SOAK_* knobs of src/soak.rs
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";

if (isMainThread) {
    process.exitCode = 1;
    const worker = new Worker(new URL(import.meta.url), {
        workerData: process.argv.slice(2),
        // The trace keeps one record per live traced allocation.
        resourceLimits: { stackSizeMb: 256, maxOldGenerationSizeMb: 8192 },
    });
    worker.on("message", status => (process.exitCode = status));
    worker.on("error", error => console.error(error));
} else {
    const { WASI } = await import("node:wasi");
    const [file, ...args] = workerData;
    const root = await mkdtemp(join(tmpdir(), "ts-wasm-soak-"));
    Error.stackTraceLimit = Number(process.env.SOAK_DEPTH ?? 30);
    const DEPTH = Number(process.env.SOAK_GROUP ?? 8);
    const SKIP = /soak|__rust_alloc|__rust_realloc|__rdl_|alloc::alloc|alloc::raw_vec|RawVec|finish_grow|do_reserve|reserve_for_push|grow_one|grow_amortized|exchange_malloc|<alloc::|alloc::vec::|alloc::string::|hashbrown::raw|try_allocate_in|with_capacity|from_iter|dlmalloc|malloc|realloc|__rust_|core::ptr|clone/i;
    const live = new Map(); // ptr -> [size, stackId]
    const stacks = [];
    const stackIds = new Map();
    const site = () => {
        const lines = new Error().stack.split("\n").slice(2);
        const frames = [];
        for (const line of lines) {
            const m = /at (?:.*?\.wasm\.)?(.*?) \(wasm:/.exec(line) ?? /at (.*?) \(/.exec(line);
            if (!m) continue;
            const name = m[1].replace(/::h[0-9a-f]{16}$/, "");
            if (frames.length === 0 && SKIP.test(name)) continue;
            frames.push(name);
            if (frames.length >= DEPTH) break;
        }
        const key = frames.join("\n      ");
        let id = stackIds.get(key);
        if (id === undefined) {
            id = stacks.length;
            stacks.push(key);
            stackIds.set(key, id);
        }
        return id;
    };
    let snapBase = null;
    const soak = {
        alloc(ptr, size) {
            live.set(ptr, [size, site()]);
        },
        free(ptr) {
            live.delete(ptr);
        },
        snapshot() {
            snapBase = new Map();
            for (const [size, id] of live.values()) snapBase.set(id, (snapBase.get(id) ?? 0) + size);
        },
        report(runs) {
            const groups = new Map();
            let total = 0;
            for (const [size, id] of live.values()) {
                const g = groups.get(id) ?? [0, 0];
                g[0] += size;
                g[1] += 1;
                groups.set(id, g);
                total += size;
            }
            if (snapBase) {
                total = 0;
                for (const [id, g] of groups) {
                    g[0] -= snapBase.get(id) ?? 0;
                    total += g[0];
                }
                for (const [id, b] of snapBase) if (!groups.has(id)) { groups.set(id, [-b, 0]); total -= b; }
                console.log("SOAK-TRACE (growth since the mid-window snapshot)");
            }
            const sorted = [...groups.entries()].sort((a, b) => b[1][0] - a[1][0]);
            console.log(
                `SOAK-TRACE survivors of ${runs} traced runs: ${total} B (${Math.round(total / runs)} B/run) in ${sorted.length} sites`,
            );
            for (const [id, [bytes, count]] of sorted.slice(0, Number(process.env.SOAK_TOP ?? 30))) {
                console.log(
                    `--- ${bytes} B (${Math.round(bytes / runs)} B/run) in ${count} allocations (${(count / runs).toFixed(1)}/run)\n      ${stacks[id]}`,
                );
            }
        },
    };
    try {
        const wasi = new WASI({
            version: "preview1",
            args: [file, ...args],
            env: process.env,
            preopens: { "/": root },
            returnOnExit: true,
        });
        const module = await WebAssembly.compile(await readFile(file));
        const instance = await WebAssembly.instantiate(module, { ...wasi.getImportObject(), soak });
        parentPort.postMessage(wasi.start(instance));
    } finally {
        await rm(root, { recursive: true, force: true });
    }
}
