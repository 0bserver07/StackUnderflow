#!/usr/bin/env python3
"""
FastAPI application for StackUnderflow Local Mode
"""

import asyncio
import logging
import os
from contextlib import asynccontextmanager

import importlib.metadata

from dotenv import load_dotenv
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

import stackunderflow.deps as deps
from stackunderflow.infra.preloader import warm as _warm_projects

# Route modules
from stackunderflow.routes import (
    bookmarks,
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
cache_warm_on_startup = config.get("cache_warm_on_startup")
enable_background_processing = config.get("enable_background_processing")
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

    async def warm_cache_background():
        logger.debug(f"[Server] Starting cache warming ({cache_warm_on_startup} projects)")
        await _warm_projects(deps.cache, deps.current_log_path, cap=cache_warm_on_startup)
        logger.debug("[Server] Cache warming completed")

    tasks: list[asyncio.Task] = []
    tasks.append(asyncio.create_task(warm_cache_background()))

    if enable_background_processing:
        tasks.append(asyncio.create_task(background_stats_processor()))

    try:
        yield
    finally:
        for t in tasks:
            t.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)


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
app.include_router(sessions.router)
app.include_router(search.router)
app.include_router(qa.router)
app.include_router(tags.router)
app.include_router(bookmarks.router)
app.include_router(misc.router)


async def background_stats_processor():
    """Background task to process stats for all projects."""
    logger.debug("[Server] Starting background stats processor")

    # Wait a bit for server to fully start
    await asyncio.sleep(5)

    while True:
        try:
            # Get all projects
            from stackunderflow.infra.discovery import project_metadata as get_all_projects_with_metadata

            all_projects = get_all_projects_with_metadata()

            # Find projects without cached stats
            uncached_projects = []
            for project in all_projects:
                log_path = project["log_path"]
                # Skip if in memory cache
                if deps.cache.fetch(log_path):
                    continue
                # Skip if in file cache
                if deps.cache.load_stats(log_path):
                    continue
                uncached_projects.append(project)

            if uncached_projects:
                logger.debug(f"[Background] Found {len(uncached_projects)} projects without cached stats")

                # Process them using global aggregator
                from stackunderflow.pipeline.cross_project import background_process

                processed = await background_process(uncached_projects, deps.cache, deps.cache, cap=5)

                logger.debug(f"[Background] Processed {processed} projects")

                # Wait before next batch
                await asyncio.sleep(30)
            else:
                logger.debug("[Background] All projects have cached stats")
                # Check again in 5 minutes
                await asyncio.sleep(300)

        except Exception as e:
            logger.error(f"[Background] Error in stats processor: {e}")
            await asyncio.sleep(60)



@app.get("/")
async def root():
    """Serve the React app."""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


# SPA catch-all -- serve React index.html for client-side routing
@app.get("/project/{full_path:path}")
async def spa_catch_all_project(full_path: str):
    """Serve React SPA for client-side routes under /project/"""
    return FileResponse(os.path.join(BASE_DIR, "static", "react", "index.html"))


# ── Backward compatibility ────────────────────────────────────────────────────
# Tests import these directly from server; delegate to the route modules.
from stackunderflow.pipeline import process as run_pipeline  # noqa: E402, F401
from stackunderflow.routes.data import refresh_all_projects, refresh_data  # noqa: E402, F401


def start_server_with_args(port=8081, host="localhost"):
    """Start the server with specified arguments"""
    import uvicorn

    uvicorn.run(app, host=host, port=port, log_level="warning", access_log=False)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=8081)
