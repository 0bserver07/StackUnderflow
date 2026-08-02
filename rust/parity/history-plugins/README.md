# `stackunderflow-history-jsonl-v1` fixtures — the `import` corpus

Checked-in plugin manifests and export commands for RS-8-101 / RS-2-006. Two
consumers, and the split between them is a rule, not a convenience:

* **`rust/parity/cases.txt`** (`T7-imp-*`, 47 rows × 2 states) uses the `m-*`,
  `s-*` and `r-*` fixtures. Every one of those is a **rejection**, because a
  case row must be side-effect-free (DIV-078) and `import`'s success leg writes
  `store.db`. Their export scripts are pure stdout emitters for the same reason:
  a row that spawned a child which wrote a file would be a row with a side
  effect one level down.
* **`rust/import-differ.sh`** uses `ok*`, `record` and `x-*`. It copies the
  fixture into a per-side scratch home first, so a script that DOES write leaves
  its file there and never in this directory.

## The naming is the class

| prefix | what is refused | what must be true afterwards |
|---|---|---|
| `m-*` | the MANIFEST | the export command never ran |
| `s-*` | the STREAM | it ran; the store is untouched, the cursor un-advanced |
| `r-*` | the RUNNER | non-zero exit, a signal, the byte cap, or the clock |
| `ok*` | nothing | rows written, the cursor advanced |
| `x-*` | the two above, with a MARKER | DIV-447's proof, and its control |

## Why the command is `sh export.sh` and not `./export.sh`

`run_export` sets the child's cwd to the manifest's own directory. Whether an
argv0 of `./export.sh` is then resolved against the PARENT's cwd or the CHILD's
is platform-specific and explicitly unspecified in Rust's `Command::current_dir`
docs — so a fixture written that way would be measuring the difference between
two languages' `execve` wrappers rather than the port. `sh` comes off `PATH`
(which the allowlist forwards) and opens the script relative to the cwd it was
given, which tests the cwd contract without depending on that ambiguity.

Using a shell here is not a contradiction of "no shell": the runner spawns
`execve(argv)` with no shell of its own, and the user's command is allowed to
BE a shell. That is the reference's design and the doc says so plainly.

## `record` is the interceptor

`record/export.sh` appends its cwd, its argv and its whole environment to
`$STAX_IMPORT_LOG`, then emits a valid stream. Both implementations spawn it
through their own runner, so the differ compares two real `execve`s rather than
a Rust value against a re-derived Python one. Its manifest deliberately does
**not** list `STAX_LEAK_PROBE` in `env_passthrough` — the differ sets that
variable in the parent, and its ABSENCE from the log is what proves the
environment was cleared. It did list it once; the proof was a tautology and the
differ's own assertions are what caught it.
