// The demo. External for the same reason `csp-watch.js` is: this page
// enforces `script-src 'self'`, so nothing may live inline in the HTML.
import init, { Store } from './pkg/stax_wasm.js';
// The module's bytes, inlined at build time. wasm-bindgen's default path is
// `fetch('stax_wasm_bg.wasm')`, and a fetch — even a same-origin one — is
// exactly what `connect-src 'none'` forbids. Handing `init` the bytes keeps the
// CSP at its strictest setting, which is the whole pitch of this page.
import { WASM_BASE64 } from './pkg/stax_wasm_inline.js';

const el = (id) => document.getElementById(id);
const drop = el('drop'), fileInput = el('file'), status = el('status');
const consolePane = el('console'), out = el('out');

let store = null;
let verb = 'decisions';

const PLACEHOLDERS = {
  decisions: 'a topic, e.g. caching',
  file:      'a path, e.g. stackunderflow/cli.py',
  worked:    'an action, e.g. pytest',
  sessions:  'a directory or file (blank = every project)',
  store:     '(no argument)',
};

const wasmBytes = Uint8Array.from(atob(WASM_BASE64), (c) => c.charCodeAt(0));
await init({ module_or_path: wasmBytes });

// ── picking a file ───────────────────────────────────────────────────────────
drop.addEventListener('click', () => fileInput.click());
drop.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') fileInput.click(); });
drop.addEventListener('dragover', (e) => { e.preventDefault(); drop.classList.add('hot'); });
drop.addEventListener('dragleave', () => drop.classList.remove('hot'));
drop.addEventListener('drop', (e) => {
  e.preventDefault();
  drop.classList.remove('hot');
  if (e.dataTransfer.files.length) load(e.dataTransfer.files[0]);
});
fileInput.addEventListener('change', () => {
  if (fileInput.files.length) load(fileInput.files[0]);
});

async function load(file) {
  status.hidden = false;
  status.innerHTML = `<h2>opening</h2><div class="meta">${esc(file.name)} — ${mib(file.size)} MiB…</div>`;
  const readStarted = performance.now();
  const bytes = new Uint8Array(await file.arrayBuffer());
  const readMs = performance.now() - readStarted;
  const openStarted = performance.now();
  try {
    store = Store.fromBytes(bytes);
  } catch (error) {
    status.innerHTML = `<h2 class="err">that file could not be opened</h2><div class="meta">${esc(String(error))}</div>`;
    return;
  }
  const openMs = performance.now() - openStarted;
  status.innerHTML =
    `<h2>${esc(file.name)}</h2>` +
    `<div class="meta">${mib(file.size)} MiB · schema v${store.schemaVersion()} · ` +
    `read ${readMs.toFixed(0)} ms · opened ${openMs.toFixed(0)} ms · ` +
    `<span style="color:var(--good)">nothing left this tab</span></div>`;
  consolePane.hidden = false;
  showcase();
}

// ── the showcase: what a first-time visitor sees without typing ──────────────
//
// The fourth card is chained rather than hard-coded: `memory sessions` needs a
// path, a page has no cwd to default to, and a guessed path answers "0 results"
// truthfully but uselessly. So the decisions card's first hit supplies its own
// `project_path` — which is a real path out of the visitor's own store — and
// the session list is scoped to that. No new API, no invented default.
function showcase() {
  out.innerHTML = '';
  render('what is in this store', { verb: 'store', options: opts() });
  const worked = render('where "test" worked', { verb: 'worked', action: 'test', options: opts() });
  const decisions = render('past decisions about "cache"',
                           { verb: 'decisions', query: 'cache', options: opts() });
  const seed = firstProjectPath(decisions) || firstProjectPath(worked);
  if (seed) {
    render(`sessions under ${seed}`, { verb: 'sessions', path: seed, options: opts() });
  }
}

function firstProjectPath(envelope) {
  const first = envelope && envelope.results && envelope.results[0];
  return first ? first.project_path || null : null;
}

// ── running one query ────────────────────────────────────────────────────────
function opts() {
  const project = el('project').value;
  return {
    now_epoch: Date.now() / 1000,
    cwd: '',                       // a page has no working directory
    limit: Number(el('limit').value || 20),
    context_budget: Number(el('budget').value || 0),
    since: el('since').value || null,
    project: project === '' ? '' : project,
    is_file: el('isfile').checked,
    store_label: 'store.db (in this tab)',
  };
}

function request() {
  const arg = el('arg').value;
  const options = opts();
  switch (verb) {
    case 'decisions': return { verb, query: arg, options };
    case 'worked':    return { verb, action: arg, options };
    case 'file':      return { verb, path: arg, options };
    case 'sessions':  return { verb, path: arg === '' ? null : arg, options };
    default:          return { verb: 'store', options };
  }
}

function render(title, req) {
  const card = document.createElement('div');
  card.className = 'card';
  const started = performance.now();
  const answer = JSON.parse(store.query(JSON.stringify(req)));
  const ms = performance.now() - started;

  if (answer.error !== undefined) {
    card.innerHTML = `<h2 class="err">${esc(title)}</h2><div class="meta">${esc(answer.error)}</div>`;
    out.prepend(card);
    return null;
  }
  if (req.verb === 'store') {
    card.innerHTML = `<h2>${esc(title)}</h2><div class="meta">${ms.toFixed(1)} ms</div>` +
                     `<pre>${esc(answer.stdout)}</pre>`;
    out.prepend(card);
    return null;
  }

  const envelope = JSON.parse(answer.stdout);
  let body = `<h2>${esc(title)}</h2>`;
  if (envelope.error !== undefined) {
    body += `<div class="meta err">${esc(envelope.error)}</div>`;
  } else {
    body += `<div class="meta">${envelope.result_count} result(s) · ` +
            `${envelope.token_estimate} est. tokens · budget ${envelope.budget}` +
            `${envelope.truncated ? ' · truncated to fit' : ''} · ${ms.toFixed(1)} ms</div>`;
    if (envelope.risk) {
      const r = envelope.risk;
      body += `<div class="meta">risk: ${r.total_sessions} session(s) touched · ` +
              `${r.worked} worked / ${r.failed} failed / ${r.reverted} reverted</div>`;
    }
    for (const hit of envelope.results) {
      body += `<div class="hit">` +
        `<div class="id">${esc(hit.session_id)}${hit.kind ? ` <span class="meta">(${esc(hit.kind)})</span>` : ''}</div>` +
        `<div class="facts">${esc(hit.project_path || hit.project_slug)} · ${esc(hit.provider)} · ` +
        `${esc(String(hit.last_ts || '').slice(0, 16))} · ${hit.message_count} msgs · ` +
        `$${Number(hit.cost_usd || 0).toFixed(2)}` +
        `${hit.outcome ? ` · ${esc(hit.outcome)}` : ''}</div>` +
        (hit.snippet ? `<div class="snip">${esc(hit.snippet)}</div>` : '') +
        `</div>`;
    }
  }
  body += `<details><summary class="meta">the envelope (stackunderflow.memory/1) — identical to <code>stax … --json</code></summary>` +
          `<pre>${esc(answer.stdout)}</pre></details>`;
  card.innerHTML = body;
  out.prepend(card);
  return envelope;
}

// ── controls ─────────────────────────────────────────────────────────────────
for (const button of document.querySelectorAll('[data-verb]')) {
  button.addEventListener('click', () => {
    verb = button.dataset.verb;
    for (const other of document.querySelectorAll('[data-verb]')) other.classList.toggle('on', other === button);
    el('arg').placeholder = PLACEHOLDERS[verb];
  });
}
el('run').addEventListener('click', () => {
  if (!store) return;
  const req = request();
  const label = verb === 'store' ? 'store' : `${verb} ${el('arg').value}`;
  render(label, req);
});
el('arg').addEventListener('keydown', (e) => { if (e.key === 'Enter') el('run').click(); });

function esc(text) {
  return String(text).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
function mib(bytes) { return (bytes / 1048576).toFixed(1); }

// A test hook, not a feature: the headless smoke test in rust/demo/smoke.py
// waits on this to know the showcase finished rendering.
window.__staxReady = true;
