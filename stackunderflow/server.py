#!/usr/bin/env python3
"""
FastAPI application for StackUnderflow Local Mode
"""

import importlib.metadata
import logging
import os
from contextlib import asynccontextmanager

from dotenv import load_dotenv
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

import stackunderflow.deps as deps

# Route modules
from stackunderflow.routes import (
    bookmarks,
    cost,
    data,
    misc,
    projects,
    qa,
    search,
    sessions,
    tags,
)
from stackunderflow.services.bookmark_service import BookmarkService
from stackunderflow.services.pricing_service import PricingService
from stackunderflow.services.qa_service import QAService
from stackunderflow.services.search_service import SearchService
from stackunderflow.services.tag_service import TagService

# Load environment variables
load_dotenv()

# Configure logging
log_level = os.getenv("LOG_LEVEL", "INFO").upper()
logging.basicConfig(
    level=getattr(logging, log_level, logging.INFO), format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
logger = logging.getLogger(__name__)
logger.info(f"Logging configured with level: {log_level}")

# Get version from package metadata
try:
    __version__ = importlib.metadata.version("stackunderflow")
except importlib.metadata.PackageNotFoundError:
    # Fallback for development mode
    from stackunderflow.__version__ import __version__

# Configuration (needed by lifespan)
config = deps.config
BASE_DIR = deps.BASE_DIR


@asynccontextmanager
async def _lifespan(_app: FastAPI):
    """Initialize services and start background tasks."""
    _svc_inits: list[tuple[str, type, dict]] = [
        ("search_service", SearchService, {}),
        ("tag_service", TagService, {}),
        ("qa_service", QAService, {}),
        ("bookmark_service", BookmarkService, {}),
        ("pricing_service", PricingService, {}),
    ]
    for name, cls, kw in _svc_inits:
        try:
            setattr(deps, name, cls(**kw))
        except Exception as e:
            logger.error(f"Failed to initialize {name}: {e}")

    active = [n for n, _, _ in _svc_inits if getattr(deps, n, None) is not None]
    failed = [n for n, _, _ in _svc_inits if getattr(deps, n, None) is None]
    logger.info(f"Services initialized: {len(active)} active, {len(failed)} failed")
    if failed:
        logger.warning(f"Failed services: {', '.join(failed)}")

    # Initialise the session store and run one ingest pass.
    from stackunderflow.adapters import registered
    from stackunderflow.ingest import run_ingest
    from stackunderflow.store import db, schema

    try:
        store_conn = db.connect(deps.store_path)
        schema.apply(store_conn)
        counts = run_ingest(store_conn, registered())
        logger.info("Ingest complete: %s", counts)
        store_conn.close()
        _maybe_clean_cold_cache()
    except Exception as e:
        logger.error("Ingest failed at startup: %s", e)

    yield


# Create FastAPI app
app = FastAPI(
    title="StackUnderflow - Local Mode",
    description="Analyze your Claude AI logs directly from your local machine",
    version=__version__,
    lifespan=_lifespan,
)

# Add CORS middleware — allow configured port and common dev-server ports
_server_port = config.get("port")
app.add_middleware(
    CORSMiddleware,
    allow_origins=[
        f"http://localhost:{_server_port}",
        f"http://127.0.0.1:{_server_port}",
        "http://localhost:5175",  # vite dev server
        "http://127.0.0.1:5175",
    ],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Add GZip compression middleware
# Disabled: GZip was actually increasing load time for large payloads
# app.add_middleware(GZipMiddleware, minimum_size=1000)

# Mount static files
app.mount("/static", StaticFiles(directory=os.path.join(BASE_DIR, "static")), name="static")

# Include all route modules
app.include_router(projects.router)
app.include_router(data.router)
app.include_router(cost.router)
app.include_router(sessions.router)
app.include_router(search.router)
app.include_router(qa.router)
app.include_router(tags.router)
app.include_router(bookmarks.router)
app.include_router(misc.router)



@app.get("/")
async def root():
    """Serve the React app."""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


# SPA catch-all -- serve React index.html for client-side routing
@app.get("/project/{full_path:path}")
async def spa_catch_all_project(full_path: str):
    """Serve React SPA for client-side routes under /project/"""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


from stackunderflow.routes.data import refresh_all_projects, refresh_data  # noqa: E402, F401


def _maybe_clean_cold_cache() -> None:
    """Remove the old JSON cache once the store is populated."""
    import shutil
    from pathlib import Path

    cold = Path.home() / ".stackunderflow" / "cache"
    if cold.exists():
        shutil.rmtree(cold, ignore_errors=True)


def start_server_with_args(port=8081, host="localhost"):
    """Start the server with specified arguments"""
    import uvicorn

    uvicorn.run(app, host=host, port=port, log_level="warning", access_log=False)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=8081)
