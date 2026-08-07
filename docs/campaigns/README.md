# Campaign / handoff working docs

Per-campaign planning, handoff, and audit docs — the durable spec for multi-session
efforts (the TaskCreate list is ephemeral). Distinct from the permanent reference
docs at `docs/` and `docs/specs/`, and from `docs/HANDOFF.md` (the general
architecture/handoff map).

- `intelligence-layer.md` — "rear-view dashboard → live intelligence layer" campaign:
  foundation status + specs for the remaining tasks (active-recall hooks, pattern
  mining, prescriptive cost). **Committed / durable.**
- `rust-port-field-log.md` — the Python → Rust port as it actually happened:
  chronology with receipts, the nuances worth keeping, and (§5) the only diagrams
  of the cross-machine agent wiring — the five channels between machines and the
  `stax msg` message lifecycle. Read §5 before touching the telephone; it is what
  documents that hook installation, not `msg send`, is what delivers a message.
  **Committed / durable.**
- `cost-audit.md`, `ui-perf-audit.md` — earlier working audit docs (untracked).
