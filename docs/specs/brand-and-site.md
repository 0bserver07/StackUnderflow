# Brand & site

Brand decisions and the build plan for `stackunderflow.run`.

Scope: the positioning audit, the identity system, the site architecture, and the
landing-page plan. Market research and design sourcing are working material and
live outside this repo.

---

## 1. Where we are

The brand surface exists in five places and they have drifted apart.

| Surface | Current claim |
|---|---|
| `README.md` | "observability **and memory** toolkit" · **20** providers · 4 pillars |
| `docs-site/src/content/docs/index.md` + `og:description` | "observability toolkit" · **17** providers · **memory pillar absent** |
| `docs-site/astro.config.mjs` | "local observability… **Starts with Claude Code**" |
| GitHub repo description | "…**Starts with Claude Code**" |
| `stackunderflow-ui/tailwind.config.js:12` | accent remapped to indigo `#667eea` |

Additional drift:

- The site's "What it does" list is from an older era — Q&A extraction, bookmarks,
  `today`/`month`/`optimize`/`export`. It never mentions agent memory, which is the
  current campaign.
- `<title>StackUnderflow | StackUnderflow</title>` — duplicated.
- Two unrelated favicons: `stackunderflow/static/favicon.ico` (app) and a
  `favicon.svg` served by the docs site.
- **Three-way color disagreement.** Measured from `stackunderflow/static/images/logo.png`,
  the mark is violet `#8050f8` + emerald `#08d898`. The UI accent is indigo `#667eea`.
  The logo's emerald is not a token anywhere in the codebase.

The live site is Starlight's `template: splash` — a docs landing, not a marketing
page. Last content edit `dea5f1f`; last touch was a release commit.

No prior branding work is recoverable: `stackunderflow memory ask` returns zero
results for landing/positioning/branding on this project, and `rename/kaguya` is a
dead branch pointing at main's exact HEAD with an empty diff ("kaguya" appears
nowhere in tree or history). This is greenfield.

---

## 2. Positioning

**Lead with memory. Cost is pillar three, not pillar one.**

Local, multi-provider, no-upload cost analytics is a solved and crowded problem —
it is the weakest axis to compete on, and a provider count is a number that gets
matched. The uncontested capabilities are the memory layer (a retrieval surface
agents query *mid-task*, behind the versioned `stackunderflow.memory/1` contract
with golden fixtures and a CI validator) and filesystem-reconstructing playback.

So the pillar order inverts from what the README currently has:

1. **Local agent memory** — your agents stop re-deriving what you already solved
2. **Time-travel playback** — every frame of a session, filesystem reconstructed
3. **Cost & yield attribution** — spend correlated with `git log`, productive vs abandoned
4. **Offline chat** — ask your own history, nothing leaves the box

Canonical one-liner, to be propagated verbatim to all five surfaces:

> **Your coding agents keep solving the same problems. StackUnderflow remembers
> so they don't have to.** Local-first memory, playback, and cost analytics over
> every AI coding session on your machine.

State the provider count once, as a supporting fact, never as the headline. Prefer
"every agent on your machine" to a number that invites comparison — and fix 17 → 20
wherever the number does appear.

**Do not write comparison copy.** No external tool is named, linked, or compared on
any shipped surface. The positioning stands on what we do, not on what anyone else
doesn't.

---

## 3. The name

**Keep StackUnderflow.** `stackunderflow.run` is bought.

The name currently reads as a joke about a certain Q&A site, which makes it feel
derivative. It has a better reading available, and it is literally what the product
does:

> A stack underflow is what happens when you pop past the bottom of the stack —
> when you read *beneath* the current frame. That is the product. Your session is
> the top frame. Everything you already learned is underneath it.

Reframed, the name stops being a reference and becomes a description. It earns a
lore section on the landing page (§6), which is what converts an odd name from a
liability into the most memorable thing on the page. The lore section explains
"underflow" in stack semantics on its own terms and does not invoke the other mark.

Trademark note: nominative riffs on a well-known mark carry a real if modest risk.
The mitigation is exactly the copy rule above — let the name stand alone. Worth a
real legal opinion before any paid launch; this is not one.

**Migration cost, if the name ever did change:** the PyPI project name (permanent —
never reclaimable or reusable), the `stax` alias, repo + Pages URL,
`~/.stackunderflow/` on every existing install, the `stackunderflow.memory/1`
contract string baked into golden fixtures, and ~3300 tests. High. Recommendation
stands: keep it, reframe it.

---

## 4. Identity

### 4.1 The palette is derived, not invented

Sampled from `logo.png` (saturated pixels only, grey field excluded):

| | measured |
|---|---|
| violet | `#8050f8` (dominant) |
| emerald | `#08d898` |

Running WCAG contrast on both against a paper background `#faf8ff` and a night
background `#120a24`:

| color | on paper `#faf8ff` | on night `#120a24` |
|---|---|---|
| violet `#8050f8` | **4.51** — AA text | 4.04 — large only |
| emerald `#08d898` | 1.77 — fails | **10.31** — AA text |
| indigo `#667eea` (current UI) | 3.47 — large only | — |

**The logo's two colors split cleanly by theme.** Violet is the ink-side accent and
only works on light. Emerald is the glow-side accent and only works on dark. The
mark already encodes this — violet is the lit upper curve, emerald the glowing
lower one.

That finding is the brand system, and it lines up with the name: **paper and violet
at the top, night and emerald underneath.** The site descends from light into dark
as you scroll into the history, and the accent hands off from violet to emerald
exactly the way the logo does. The mark is the scroll gradient.

Verified token set — every ratio measured, none asserted:

```
/* PAPER (default) */
--paper:          #faf8ff              /* background */
--ink:            #1a1030   /* 17.18 */
--violet:         #8050f8   /*  4.51 */  /* accent */
--emerald-deep:   #107e69   /*  4.73 */  /* emerald, deepened 55% into ink for AA */

/* NIGHT */
--night:          #120a24              /* background */
--night-fg:       #faf8ff   /* 18.21 */
--emerald:        #08d898   /* 10.31 */  /* accent */
--violet-light:   #8c61f9   /*  4.74 */  /* violet, lightened 10% toward paper for AA */
```

The ladder (`--surface-1..3`, `--hairline`, `--line`, `--fg-mut`, `--fg-dim`)
derives from these seeds via `color-mix`, with deeper mixes on paper than on night
so quiet text still clears AA in the default theme.

### 4.2 The app never recolors

Pin the dashboard's real colors — including the existing indigo `#667eea` /
`#764ba2`, which stays the *product's* accent — into `--app-*` tokens, and have
product-window containers re-seed the cascade:

```css
.app-window, .shot, .term {
  --paper: var(--app-1);
  --ink:   var(--app-fg);
  /* … every themed token re-seeded to its --app-* counterpart */
}
```

Every descendant then resolves to app colors in *both* site themes, so screenshots
and mock dashboards look identical whichever theme the visitor is in. Page chrome
around them stays site-themed.

This also resolves the three-way color drift honestly rather than by picking a
winner: the site brand is violet/emerald from the mark, the product keeps its
indigo, and they never fight because they never share a surface.

### 4.3 Type

- **Display** — a tight geometric sans or low-contrast slab. Candidates: Instrument
  Sans, Bricolage Grotesque, Newsreader (if we want serif warmth). Needs an eye.
- **Body** — Inter Variable
- **Mono** — JetBrains Mono Variable, and mono carries the eyebrows

Preload the exact hashed latin woff2 subsets in `<head>` so the display face does
not flash on first paint.

### 4.4 The mark

Current `logo.png` is a violet→emerald "S" with a soft glow. It's a letterform: it
says nothing about the product, and the glow will not survive a 16px favicon.

Redraw as inline SVG so it inherits nothing and loads with the document, keeping the
measured two-color split and encoding the name. The concept: **stack frames
descending, with the lowest frame lit** — three or four stacked bars in violet
stepping downward, the bottom one in emerald and offset below the baseline, the
frame *under* the stack. Reads at 16px because it is bars, not a glow.

Unify the favicons: one SVG source, exported to `.ico` for the app.

---

## 5. Site architecture

Rebuild as a real site with the docs scoped to a subpath.

| | now | target |
|---|---|---|
| Host | GitHub Pages (`.github/workflows/docs.yml`) | Netlify at `stackunderflow.run` |
| Landing | Starlight `template: splash` | custom `src/pages/index.astro` + `Base.astro` |
| Docs | site root | `/docs/` via Starlight |
| Styles | Starlight defaults | `global.css` (site) + `starlight.css` (docs bridge) |
| Analytics | none | Umami, cookieless — no consent banner owed |
| Agent surface | none | `/llms.txt` |
| Changelog | `CHANGELOG.md` only | `/changelog/` content collection |

Netlify over Pages because the newsletter needs serverless functions.

Non-obvious details that will otherwise cost a debugging session each:

- **Starlight does not use `Base.astro`.** The analytics tag must be duplicated into
  the Starlight `head` array or the docs are a blind spot.
- **Bridge the two theme systems.** An inline script maps our `paper`/`night` choice
  onto Starlight's `light`/`dark` before first paint, with a `change` listener for
  the reverse direction, or the docs and landing disagree on theme.
- **Theme init must run before paint**, with `?theme=night|paper` overriding
  localStorage (useful for screenshots) and `<meta name="theme-color">` updated on
  toggle.

Also: `disable404Route` plus a custom 404, and `@astrojs/sitemap`.

### 5.1 The differentiating build: ⌘K is a memory query

The pitch is `stax memory ask "…"`. So ⌘K on the site should open exactly that —
not a generic command palette, but the memory interface, answering from a canned
corpus about StackUnderflow itself, in the real output shape with the real
`stackunderflow.memory/1` envelope visible. Type "how do I change the port" and it
answers the way the CLI answers. Empty state in voice: `no memory of that yet.`

Style it with the app's own tokens and note in the footer that the same query runs
in the terminal. It demonstrates the feature by being it, and it lets a visitor use
the product before installing it.

Full a11y is not optional here: `role="combobox"`, `aria-activedescendant`, focus
restored to wherever the visitor was on close.

---

## 6. Landing page plan

Copy discipline: one metaphor sustained through every section label, headlines that
are claims rather than feature names, one short declarative h2 per section broken
over two lines.

Our metaphor is **the stack and what's under it** — depth, frames, popping down.

| # | Section | Eyebrow | Headline |
|---|---|---|---|
| 1 | Hero | — | *Your agent has already<br />solved this before.* |
| 2 | Strip | — | providers · messages indexed · `~/.stackunderflow/` · offline |
| 3 | **Memory** | Below the stack | *Your history, but your agent<br />can read it mid-task.* |
| 4 | Playback | Every frame | *Scrub the session. Watch<br />the files change.* |
| 5 | Cost | The ledger | *What it cost, and<br />whether it shipped.* |
| 6 | Sources | One store | *Every agent on your machine,<br />in one SQLite file.* |
| 7 | Privacy | Nothing leaves | *No account. No telemetry.<br />No network call.* |
| 8 | No lock-in | Read it yourself | *It's just SQLite.<br />Open it with anything.* |
| 9 | FAQ | Fair questions | *Fair questions.* |
| 10 | Lore | The name | *Why "StackUnderflow"?* |
| 11 | CTA | — | `pip install stackunderflow` |

Hero visual: not a static screenshot. A terminal window — traffic-light chrome,
semantic text classes, blinking cursor, reveal-on-scroll behind
`prefers-reduced-motion` guards — running a real `stax memory ask` and returning a
real answer.

**Section 7 carries weight.** Storing session content is the thing a privacy-minded
visitor will hesitate over, and the honest answer is the strong one: content is the
product, and locality is what makes keeping it safe. Nothing leaves
`~/.stackunderflow/`, there is no account, and there is no network call to audit.
State it plainly rather than defensively.

Sections 4–6 can reuse the existing `docs-site/src/assets/*.png` screenshots
(overview, cost, playback, agent-sidebar), wrapped in the re-seeding `.app-window`
container from §4.2 so they hold their own colors in both themes.

Build one component per section. A single thousand-line `index.astro` is the
predictable failure mode.

---

## 7. Sequencing

1. **Positioning propagation** — the canonical line into README, `index.md`,
   `astro.config.mjs`, og tags, GitHub description; fix 17 → 20 and the duplicated
   `<title>`. Cheap, independent of everything below.
2. **Identity** — commit the verified token set, redraw the mark as SVG, unify
   favicons, pick the display face.
3. **Site scaffold** — Astro + Starlight at `/docs/`, `Base.astro`, `global.css` +
   `starlight.css`, Netlify config, DNS for `stackunderflow.run`.
4. **Landing page** — sections per §6, component per section.
5. **The memory palette** (§5.1) — the differentiating build.
6. **`llms.txt`, changelog route, sitemap, Umami.**

Open decisions for the maintainer:

- Display typeface — needs an eye, not an argument.
- Does the paper→night scroll descent get built as a real gradient across sections,
  or just as a light hero and a dark memory section?
- Keep `0bserver07.github.io/StackUnderflow` redirecting to the new domain, or
  retire it?
