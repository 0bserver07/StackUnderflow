"""Print stats for the first project the local store knows about.

Run with: python examples/process_session.py
"""

import stackunderflow

projects = stackunderflow.list_projects()
if not projects:
    raise SystemExit("No projects in the store. Run `stackunderflow init` to ingest.")

slug = projects[0]["slug"]
provider = projects[0]["provider"]
messages, stats = stackunderflow.process(slug, provider=provider)

overview = stats["overview"]
print(f"Project: {slug} (provider={provider})")
print(f"Messages: {len(messages)}")
print(f"Sessions: {overview['sessions']}")
print(f"Total cost: ${overview['total_cost']:.2f}")
print(
    f"Tokens in/out: "
    f"{overview['total_tokens']['input']:,} / {overview['total_tokens']['output']:,}"
)
