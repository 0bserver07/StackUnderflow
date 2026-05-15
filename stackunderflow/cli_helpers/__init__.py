"""Helpers shared by Click subcommands.

These modules sit alongside ``stackunderflow/cli.py`` and host small pieces
of logic that don't belong in the CLI dispatch file but are CLI-only (i.e.
they shouldn't be imported by the server or the public Python API). Keep
them synchronous, side-effect-explicit, and free of FastAPI / background-
thread machinery — the CLI runs in one process, top-to-bottom.
"""
