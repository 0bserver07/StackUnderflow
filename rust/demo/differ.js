// The wasm half of `rust/wasm-differ.sh`: run the SAME artifact the browser
// loads, against the SAME store.db the native `stax` reads, and write each
// case's stdout bytes to a file the shell can `cmp`.
//
//   node differ.js <store.db> <out-dir> < requests.jsonl
//
// stdin is one JSON object per line: {"id": "...", "request": { ... }}.
// For each, `<out-dir>/<id>.out` gets the exact stdout bytes and
// `<out-dir>/<id>.code` the exit code — no interpretation, no normalisation,
// nothing this script could accidentally make agree (ledger lesson: "a differ
// that under-reads agrees by accident").
//
// The wasm module is booted ONCE and the store imported once; every case then
// runs against the same in-memory database, which is also how the demo page
// behaves after a drop.

const fs = require('fs');
const path = require('path');

const [, , storePath, outDir] = process.argv;
if (!storePath || !outDir) {
    console.error('usage: node differ.js <store.db> <out-dir> < requests.jsonl');
    process.exit(2);
}

const { Store } = require('./pkg-node/stax_wasm.js');

const t0 = Date.now();
const bytes = fs.readFileSync(storePath);
const readMs = Date.now() - t0;

const t1 = Date.now();
const store = Store.fromBytes(bytes);
const openMs = Date.now() - t1;

fs.mkdirSync(outDir, { recursive: true });

const lines = fs.readFileSync(0, 'utf8').split('\n').filter((line) => line.trim() !== '');
let failures = 0;
const timings = [];

for (const line of lines) {
    const { id, request } = JSON.parse(line);
    const started = process.hrtime.bigint();
    const raw = store.query(JSON.stringify(request));
    const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
    const result = JSON.parse(raw);
    if (result.error !== undefined) {
        // An engine-level failure is written as an empty stdout and code 70 so
        // the shell sees a divergence rather than a missing file.
        fs.writeFileSync(path.join(outDir, `${id}.out`), '');
        fs.writeFileSync(path.join(outDir, `${id}.code`), '70\n');
        fs.writeFileSync(path.join(outDir, `${id}.err`), `${result.error}\n`);
        failures += 1;
    } else {
        fs.writeFileSync(path.join(outDir, `${id}.out`), result.stdout);
        fs.writeFileSync(path.join(outDir, `${id}.code`), `${result.code}\n`);
    }
    timings.push(`${id}\t${elapsedMs.toFixed(1)}`);
}

fs.writeFileSync(
    path.join(outDir, '_timings.tsv'),
    `# read ${readMs}ms  import+open ${openMs}ms  schema v${store.schemaVersion()}\n` +
        `${timings.join('\n')}\n`,
);
console.error(
    `wasm: ${lines.length} cases, store ${(bytes.length / 1048576).toFixed(1)} MiB, ` +
        `read ${readMs}ms, import+open ${openMs}ms, ${failures} engine failures`,
);
