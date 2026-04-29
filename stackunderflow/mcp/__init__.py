"""MCP server for StackUnderflow.

Exposes adapter-layer queries to MCP clients. Stateless: parses JSONL
session logs directly through `stackunderflow.adapters` without touching
the SQLite store. The adapter layer is the canonical normalised form;
SQLite is downstream.

Run with: `stackunderflow-mcp` (stdio transport).
"""
