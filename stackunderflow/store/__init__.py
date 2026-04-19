"""SQLite-backed session store.

Exposes a thin connection helper and typed query helpers. Route handlers
and CLI reports import from `store.queries`; nothing else should touch
the raw `sqlite3` API.
"""
