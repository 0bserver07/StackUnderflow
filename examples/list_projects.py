"""Print every project StackUnderflow has indexed across every provider.

Run with: python examples/list_projects.py
"""

import stackunderflow

projects = stackunderflow.list_projects()
if not projects:
    raise SystemExit("No projects in the store. Run `stackunderflow init` to ingest.")

for p in projects:
    # p is a dict with keys: slug, provider, display_name, path,
    # first_seen, last_modified
    print(f"[{p['provider']:<7}] {p['slug']}")
