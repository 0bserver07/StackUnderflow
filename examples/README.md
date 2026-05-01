# Examples

Short, runnable scripts showing how to use StackUnderflow as a Python library. Each one prints to stdout and works against whatever local data the machine has — no fixtures, no extra setup beyond `pip install stackunderflow`.

| File | What it shows |
|------|---------------|
| `list_projects.py` | Iterate every project the local store knows about — provider-tagged |
| `process_session.py` | Pull pipeline messages + stats for the first project via `stackunderflow.process` |
| `cross_provider_costs.py` | Sum cost per provider across the store; uses beta adapters when their env vars are set |

Run any of them directly:

```bash
python examples/list_projects.py
python examples/process_session.py
python examples/cross_provider_costs.py
```

The cross-provider example only sees Cursor and Cline data when the corresponding `STACKUNDERFLOW_BETA_*` env vars are set at ingest time — see `docs/multi-provider.md` for the opt-in flow.
