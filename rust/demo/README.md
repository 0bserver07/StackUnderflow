# rust/demo — drop your `store.db` in a browser

Wave 9's runnable proof: the read-only query core of `stax`, compiled to
WebAssembly, answering `memory decisions` / `memory file` / `memory worked` /
`memory sessions` / `store` about a `store.db` the visitor drops onto the page.
Plain HTML and one `<script type="module">` — no framework, no bundler, no
build step for the page itself.

The pitch is the privacy property, so the page enforces it instead of
promising it:

* the wasm module is **inlined as base64** at build time, so the page issues no
  `fetch` at all;
* the CSP is `default-src 'none'; connect-src 'none'` — fetch, XHR, WebSocket,
  beacons, remote images and frames are all refused **by the browser**, not by
  our good intentions;
* the page installs a `securitypolicyviolation` listener and `smoke.py`
  asserts it stayed empty, so a future edit that adds an upload fails the test
  rather than shipping;
* the store is opened `SQLITE_OPEN_READ_ONLY` inside an in-memory VFS. Nothing
  is written to disk, to OPFS, to IndexedDB or to localStorage.

## Files

| file | what it is |
|---|---|
| `index.html` | the whole demo: drop zone, verb console, result cards |
| `build.sh` | cargo → wasm-bindgen → base64 inline → `SIZE.md` |
| `inline_wasm.py` | the base64 step, split out so `build.sh` stays readable |
| `differ.js` | node harness — the wasm half of `rust/wasm-differ.sh` |
| `smoke.py` | headless-browser proof that the page actually answers |
| `make-subset.py` | builds the differ's store: `backup()` snapshot → drop old partitions → VACUUM |
| `SIZE.md` | generated; the measured artifact sizes |
| `pkg/`, `pkg-node/` | generated; the wasm + glue for the browser and for node |

## Build

```sh
export STAX_WASI_SDK=/path/to/wasi-sdk-33.0-x86_64-linux    # a wasm32 clang
export STAX_WASM_BINDGEN=/path/to/wasm-bindgen              # 0.2.126
rustup target add wasm32-unknown-unknown
./build.sh
```

Two prerequisites, both stated because neither is on a stock box:

* **a clang that targets wasm32.** `sqlite-wasm-rs` compiles `sqlite3.c`, and
  this machine has no clang at all (`/usr/lib/llvm-10` is libraries only).
  wasi-sdk is used *purely as a C compiler* — the artifact targets
  `wasm32-unknown-unknown` and imports no WASI syscall.
* **the `wasm-bindgen` CLI, version-matched to `Cargo.lock`.** `build.sh`
  compares them and refuses to continue on a mismatch, because the failure mode
  otherwise is an unhelpful import error at runtime.

## Run the page

```sh
python3 -m http.server 8097 --bind 127.0.0.1   # never :8095
xdg-open http://127.0.0.1:8097/index.html
```

A static server is needed only because ES modules are not allowed to load from
`file://`. It serves this directory and nothing else; the page still talks to
no network, its own origin included.

Then drop a `store.db` on the page. It runs four showcase queries immediately —
what is in the store, where "test" worked, past decisions about "cache", and
the sessions under a real project path — and the console below re-runs any verb
with your own arguments.

The fourth is chained rather than hard-coded: `memory sessions` needs a path, a
page has no cwd to default to, and a guessed path answers "0 results"
truthfully but uselessly. So the decisions card's first hit supplies its own
`project_path`, out of the visitor's own store, and the session list is scoped
to that.

### The size ceiling, stated plainly

The in-memory VFS holds the whole database in wasm linear memory, so the
ceiling is wasm32's 4 GiB address space minus SQLite's arena — call it ~1.5 GiB
in a real tab. **The maintainer's live store is 3.9 GB and does not fit.**
Lifting the ceiling needs a lazy VFS that pulls 4 KiB pages from the `File` on
demand (a Worker plus `FileReaderSync` gives synchronous slices); that is
DIV-332 on the maintainer's desk, and it requires an `unsafe` exception for the
C callbacks, which this crate does not take on its own authority.

## Prove it against the CLI

```sh
../wasm-differ.sh
```

Runs every case in `../parity/wasm-cases.txt` twice — once through the native
`stax` binary, once through the same wasm artifact this page loads, against the
**same** `store.db` — and `cmp`s the stdout bytes and the exit codes.

Wave-9 result: **32 cases, 32 identical, 0 divergent, 0 errors**, on a 227 MB
subset of the maintainer's real store, built by

```sh
python3 make-subset.py ~/…/stackunderflow-data/store.db ../.parity-state/wasm9/home/store.db
```

(a `sqlite3 backup()` snapshot of the live file with every message partition but
`202607`/`202608` emptied, then `VACUUM`ed — 3.9 GB → 227 MB). Both engines read
the same file, so the subset limits the *coverage* of the proof, never its
equality.

The differ has been shown to bite: with two constants mutated in the wasm verb
layer (`budget_default` 2000→1999 and the `touched` tag), the same 32 cases came
back **9 identical / 23 divergent**. A differ that has never failed is dead
corpus.

## Prove the page

```sh
STAX_WEBDRIVER=/path/to/geckodriver python3 smoke.py ../.parity-state/wasm9/small-store.db
```

Serves the directory, drives a real headless browser through the file input
(the same code path a drop takes), waits for the showcase, asserts the store
opened and a verb answered, checks no CSP violation was recorded, and writes a
screenshot to `../.parity-state/wasm9/demo-screenshot.png`.

Note on this box: Chrome is 87 (2020) and its chromedriver 2.41 (2018) cannot
drive it — and Chrome 87 predates the `'wasm-unsafe-eval'` CSP keyword anyway.
Firefox 136 is current, so the smoke test uses geckodriver.

## What the browser cannot do, and how each is handled

| the CLI reads | the page does | recorded as |
|---|---|---|
| `datetime.now(UTC)` | JS passes `now_epoch`; `pytime` takes an injected clock on wasm32 | DIV-331 |
| `Path.cwd()` | the caller passes `cwd`; empty means "every project" | DIV-336 |
| `Path(target).is_file()` | the caller declares `is_file` | DIV-335 |
| `search_index.db` beside the store | absent — the LIKE path, always | DIV-337 |
| symlinks (`canonicalize`) | lexical resolution only | DIV-336 |
