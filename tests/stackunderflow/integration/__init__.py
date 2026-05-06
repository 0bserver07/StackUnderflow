"""Wave 4E — real-data integration + per-route latency regression tests.

Two slow-marker test files live here:

* ``test_etl_pipeline_e2e.py`` — builds a 10K-message synthetic store across
  five providers, runs the normalize → marts → routes path end-to-end, and
  asserts every dashboard route returns 200 + non-empty data within its
  per-route latency budget.

* ``test_route_perf_regression.py`` — parametrises every dashboard route
  against a pre-populated mart fixture (~100K daily, ~50K session, ~1K
  project, ~2K provider_day, ~5K model_day rows) and pins each route's
  cold + warm latency budget. Fails CI if any route regresses.

Both files are gated on the ``slow`` pytest marker (registered in
``pyproject.toml``) so the default ``pytest tests/ -q`` run skips them. Run
with ``pytest -m slow tests/stackunderflow/integration -q`` to exercise.

Synthetic stores are always built in ``tmp_path`` (the test never touches
the user's real ``~/.stackunderflow/store.db``) and adapter normalization
runs against in-process objects, never against real provider source files.
"""
