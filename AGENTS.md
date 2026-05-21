<!-- stackunderflow:guide:start -->
## StackUnderflow — query your past coding sessions

This machine indexes every past AI coding session locally with StackUnderflow.
Before re-deriving something, check whether the answer is already recorded:

- `stackunderflow memory file <path>` — a file's history: past edits, failure
  modes, and sessions that touched it. Worth a look before a non-trivial edit.
- `stackunderflow memory decisions "<topic>"` — past decisions on a topic.
- `stackunderflow memory worked "<action>"` — past sessions where an action
  succeeded, with evidence.
- `stackunderflow memory sessions` — recent sessions in this project.
- `stackunderflow memory ask "<question>"` — natural-language query over history.

Pass `--json` for a stable, token-bounded envelope (`schema:
stackunderflow.memory/1`) meant for programmatic use. Every query is local and
read-only — nothing leaves the machine.
<!-- stackunderflow:guide:end -->
