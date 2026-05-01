"""Sum cost per provider across every project in the local store.

Beta providers (Cursor, Cline, …) only appear if their adapters were
registered at ingest time. Set the matching ``STACKUNDERFLOW_BETA_*``
env var and run ``stackunderflow reindex`` first to include them.

Run with: python examples/cross_provider_costs.py
"""

from collections import defaultdict

import stackunderflow

projects = stackunderflow.list_projects()
if not projects:
    raise SystemExit(
        "No projects in the store. Run `stackunderflow init` to ingest first."
    )

totals: dict[str, float] = defaultdict(float)
sessions_seen: dict[str, int] = defaultdict(int)

for project in projects:
    try:
        _messages, stats = stackunderflow.process(
            project["slug"], provider=project["provider"]
        )
    except Exception:
        continue
    overview = stats.get("overview", {})
    totals[project["provider"]] += float(overview.get("total_cost", 0.0) or 0.0)
    sessions_seen[project["provider"]] += int(overview.get("sessions", 0) or 0)

if not totals:
    print("No usable projects in store. Run `stackunderflow reindex` first.")
    raise SystemExit(0)

grand_total = sum(totals.values()) or 1.0
width = 30
print(f"{'provider':<10} {'sessions':>9} {'cost':>12}  share")
print("-" * 50)
for provider in sorted(totals, key=totals.get, reverse=True):
    cost = totals[provider]
    share = cost / grand_total
    bar = "#" * int(round(share * width))
    print(
        f"{provider:<10} {sessions_seen[provider]:>9d} "
        f"{'$' + f'{cost:,.2f}':>12}  {bar}"
    )
print("-" * 50)
print(f"{'TOTAL':<10} {sum(sessions_seen.values()):>9d} {'$' + f'{grand_total:,.2f}':>12}")
