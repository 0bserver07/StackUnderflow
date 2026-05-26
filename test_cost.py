import sys
import asyncio
from pathlib import Path
from fastapi import FastAPI
from stackunderflow.routes import cost
from stackunderflow import deps

deps.store_path = Path("/Users/yadkonrad/.stackunderflow/store.db")
deps.current_log_path = "/Users/yadkonrad/.claude/projects/-Users-yadkonrad-dev-dev-year25-aug25-KayOS-ETL-Pipelines"

async def main():
    try:
        res = await cost.get_cost_data(log_path=None)
        print("Success!")
        print(list(res.keys()))
        print("session_costs:", res.get("session_costs"))
    except Exception as e:
        import traceback
        traceback.print_exc()

asyncio.run(main())
