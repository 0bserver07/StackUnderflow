#!/usr/bin/env python3
"""Conformance checker for the ``stackunderflow.memory/1`` agent-output contract.

Dependency-free (stdlib only) so it runs anywhere Python does -- no jsonschema,
no pip install. It implements just the JSON-Schema 2020-12 subset the contract
uses: ``$ref`` (local), ``oneOf``, ``type``, ``required``, ``properties``,
``items``, ``const`` and ``enum``. Unknown keywords are ignored and objects are
open (no ``additionalProperties`` handling), which is deliberate: an unknown
ADDITIVE field is never visited, so it is preserved and ignored, never rejected.

Run ``python scripts/check_memory_contract.py`` (used by CI). Three phases, all
must pass:

1. CONFORMANCE -- every ``contracts/stackunderflow-memory-v1/fixtures/*.json``
   validates against ``schema.json``.
2. FORWARD-COMPAT -- injecting an unknown additive field (top-level and into a
   result row) still validates, and the field is preserved.
3. NEGATIVE SELF-TEST -- deliberately breaking a fixture (drop a required field,
   corrupt the ``schema`` const, use an out-of-enum command) is REJECTED. This is
   what proves the checker actually bites.

``validate`` is importable with no side effects; the CLI lives under __main__.
"""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
CONTRACT_DIR = ROOT / "contracts" / "stackunderflow-memory-v1"
SCHEMA_PATH = CONTRACT_DIR / "schema.json"
FIXTURES_DIR = CONTRACT_DIR / "fixtures"

_TYPE_CHECKS = {
    "object": lambda x: isinstance(x, dict),
    "array": lambda x: isinstance(x, list),
    "string": lambda x: isinstance(x, str),
    # bool is a subclass of int in Python -- exclude it from the numeric types.
    "boolean": lambda x: isinstance(x, bool),
    "integer": lambda x: isinstance(x, int) and not isinstance(x, bool),
    "number": lambda x: isinstance(x, (int, float)) and not isinstance(x, bool),
    "null": lambda x: x is None,
}


def _typename(value: Any) -> str:
    for name, check in _TYPE_CHECKS.items():
        if name != "number" and check(value):
            return name
    return type(value).__name__


def _resolve_ref(ref: str, root: dict) -> dict:
    """Resolve a local JSON pointer (``#`` or ``#/$defs/Name``)."""
    if ref == "#":
        return root
    if not ref.startswith("#/"):
        raise ValueError(f"only local $ref is supported, got {ref!r}")
    node: Any = root
    for part in ref[2:].split("/"):
        node = node[part.replace("~1", "/").replace("~0", "~")]
    return node


def validate(instance: Any, schema: dict, root: dict, path: str = "$") -> list[str]:
    """Validate ``instance`` against ``schema``; return a list of error strings.

    An empty list means valid. ``root`` is the top-level schema document, used to
    resolve ``$ref``. Only the keyword subset documented in the module docstring
    is honoured; every other keyword (``$defs``, ``title``, ``description``, ...)
    is ignored.
    """
    if "$ref" in schema:  # in this contract $ref always stands alone
        return validate(instance, _resolve_ref(schema["$ref"], root), root, path)

    if "oneOf" in schema:  # exactly one branch must validate
        matched = [i for i, sub in enumerate(schema["oneOf"])
                   if not validate(instance, sub, root, path)]
        if len(matched) != 1:
            return [f"{path}: expected exactly one oneOf branch to match, "
                    f"{len(matched)} did"]
        return []

    errors: list[str] = []
    if "const" in schema and instance != schema["const"]:
        errors.append(f"{path}: const mismatch: {instance!r} != {schema['const']!r}")
    if "enum" in schema and instance not in schema["enum"]:
        errors.append(f"{path}: {instance!r} not in enum {schema['enum']}")
    if "type" in schema:
        types = schema["type"]
        types = types if isinstance(types, list) else [types]
        if not any(_TYPE_CHECKS[t](instance) for t in types):
            errors.append(f"{path}: expected type {schema['type']}, "
                          f"got {_typename(instance)}")

    if isinstance(instance, dict):
        for key in schema.get("required", []):
            if key not in instance:
                errors.append(f"{path}: missing required property {key!r}")
        for key, subschema in schema.get("properties", {}).items():
            if key in instance:  # unknown keys are intentionally not visited
                errors += validate(instance[key], subschema, root, f"{path}.{key}")
    if isinstance(instance, list) and "items" in schema:
        for i, item in enumerate(instance):
            errors += validate(item, schema["items"], root, f"{path}[{i}]")
    return errors


def load_schema() -> dict:
    return json.loads(SCHEMA_PATH.read_text())


def load_fixtures() -> list[tuple[str, Any]]:
    return [(p.name, json.loads(p.read_text()))
            for p in sorted(FIXTURES_DIR.glob("*.json"))]


def _check_conformance(schema, fixtures) -> list[str]:
    problems = []
    for name, inst in fixtures:
        errs = validate(inst, schema, schema)
        problems += [f"[conformance] {name}: {e}" for e in errs]
    return problems


def _check_forward_compat(schema, fixtures) -> list[str]:
    """An unknown additive field (top-level and in a row) must be accepted and
    survive validation untouched."""
    problems = []
    for name, inst in fixtures:
        mutated = copy.deepcopy(inst)
        mutated["x_future_additive_field"] = {"added_later": [1, 2, 3]}
        rows = mutated.get("results")
        if isinstance(rows, list) and rows:
            rows[0]["x_future_row_field"] = "ignored, not rejected"
        errs = validate(mutated, schema, schema)
        if errs:
            problems += [f"[forward-compat] {name}: additive field rejected: {e}"
                         for e in errs]
        if "x_future_additive_field" not in mutated:
            problems.append(f"[forward-compat] {name}: additive field not preserved")
    return problems


def _check_negative(schema, fixtures) -> list[str]:
    """The checker must REJECT deliberately-broken envelopes. Each mutation below
    is expected to produce at least one error; a mutation that still validates is
    itself the failure."""
    by_name = dict(fixtures)
    ok = next(inst for n, inst in fixtures if n.endswith(".success.json"))
    err_fx = next(inst for n, inst in fixtures if n.endswith(".error.json"))

    def drop(inst, key):
        m = copy.deepcopy(inst)
        m.pop(key, None)
        return m

    def setval(inst, key, val):
        m = copy.deepcopy(inst)
        m[key] = val
        return m

    cases = [
        ("drop required 'schema'", drop(ok, "schema")),
        ("drop required 'results'", drop(ok, "results")),
        ("corrupt 'schema' const", setval(ok, "schema", "stackunderflow.memory/999")),
        ("out-of-enum 'command'", setval(ok, "command", "not-a-command")),
        ("'result_count' wrong type", setval(ok, "result_count", "seven")),
        ("'truncated' wrong type", setval(ok, "truncated", "false")),
        ("error envelope missing 'error'", drop(err_fx, "error")),
    ]
    problems = []
    for label, mutated in cases:
        if not validate(mutated, schema, schema):
            problems.append(f"[negative] mutation NOT rejected: {label}")
    if not by_name:  # defensive: fixtures must exist
        problems.append("[negative] no fixtures found")
    return problems


def main() -> int:
    if not SCHEMA_PATH.exists():
        print(f"FAIL: schema not found at {SCHEMA_PATH}", file=sys.stderr)
        return 1
    schema = load_schema()
    fixtures = load_fixtures()
    if not fixtures:
        print(f"FAIL: no fixtures under {FIXTURES_DIR}", file=sys.stderr)
        return 1

    problems: list[str] = []
    problems += _check_conformance(schema, fixtures)
    problems += _check_forward_compat(schema, fixtures)
    problems += _check_negative(schema, fixtures)

    if problems:
        print(f"FAIL: stackunderflow.memory/1 contract check ({len(problems)} problem(s)):")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(f"OK: {len(fixtures)} fixture(s) conform to stackunderflow.memory/1 "
          f"(conformance + forward-compat + negative self-test all pass).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
