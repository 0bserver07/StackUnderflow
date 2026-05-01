"""Open the local session store and print stats for the first project found.

Run with: python examples/process_session.py
"""

from pathlib import Path

import stackunderflow
from stackunderflow.store import db, queries

projects = stackunderflow.list_projects()
if not projects:
    raise SystemExit("No projects on disk. Run `stackunderflow init` first.")

store_path = Path.home() / ".stackunderflow" / "store.db"
if not store_path.is_file():
    raise SystemExit(f"Store not found at {store_path}. Run `stackunderflow init` to ingest.")

conn = db.connect(store_path)
project = queries.get_project(conn, slug=projects[0]["dir_name"])
if project is None:
    raise SystemExit(
        f"Project {projects[0]['dir_name']!r} not in store. Run `stackunderflow reindex`."
    )

messages, stats = queries.get_project_stats(conn, project_id=project.id)
conn.close()

overview = stats["overview"]
print(f"Project: {project.slug} (provider={project.provider})")
print(f"Messages: {len(messages)}")
print(f"Sessions: {overview['sessions']}")
print(f"Total cost: ${overview['total_cost']:.2f}")
print(f"Tokens in/out: {overview['total_tokens']['input']:,} / {overview['total_tokens']['output']:,}")
