#!/usr/bin/env python3
"""
FastAPI server for stackunderflow.dev site with admin functionality
"""

import os
import sys
from pathlib import Path

# Add parent directory to path to import stackunderflow modules
sys.path.insert(0, str(Path(__file__).parent.parent))

# Load .env.stackunderflow.dev for site configuration
from dotenv import load_dotenv

env_file = Path(__file__).parent.parent / ".env.stackunderflow.dev"
if env_file.exists():
    load_dotenv(env_file)

import uvicorn
from admin import router as admin_router
from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, HTMLResponse
from fastapi.staticfiles import StaticFiles

app = FastAPI(title="StackUnderflow Site")

# CORS for development
app.add_middleware(
    CORSMiddleware,
    allow_origins=["http://localhost:8000", "http://localhost:8081", "http://localhost:4001"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Mount admin routes
app.include_router(admin_router, prefix="/admin")

# Mount static files
site_dir = Path(__file__).parent
app.mount("/static", StaticFiles(directory=site_dir / "static"), name="static")


@app.get("/")
async def homepage():
    """Serve the homepage."""
    return FileResponse(site_dir / "index.html")


@app.get("/admin")
async def admin_page(request: Request):
    """Serve the admin dashboard."""
    # Check if authenticated
    from auth import GoogleOAuth

    oauth = GoogleOAuth()
    session_id = request.cookies.get("admin_session")

    if not session_id or not oauth.get_session(session_id):
        # Redirect to login
        return HTMLResponse("""
        <html>
        <head>
            <meta http-equiv="refresh" content="0; url=/admin/login">
        </head>
        <body>
            Redirecting to login...
        </body>
        </html>
        """)

    # Serve admin page
    admin_file = site_dir / "admin.html"
    if admin_file.exists():
        return FileResponse(admin_file)
    else:
        # We'll create this file next
        return HTMLResponse("<h1>Admin Dashboard (Coming Soon)</h1>")


@app.get("/gallery-index.json")
async def gallery_index():
    """Serve the gallery index."""
    from admin import load_gallery_index

    gallery = load_gallery_index()

    # Sort projects: featured first, then by date
    projects = gallery.get("projects", [])
    featured = [p for p in projects if p.get("featured", False)]
    non_featured = [p for p in projects if not p.get("featured", False)]

    # Sort each group by date (newest first)
    featured.sort(key=lambda x: x.get("created_at", ""), reverse=True)
    non_featured.sort(key=lambda x: x.get("created_at", ""), reverse=True)

    # Combine: featured first, then non-featured
    gallery["projects"] = featured + non_featured

    return gallery


if __name__ == "__main__":
    port = int(os.getenv("SITE_PORT", "8000"))
    print(f"\n🚀 StackUnderflow site server starting on http://localhost:{port}")
    print("📋 Admin panel available at http://localhost:{port}/admin")
    print("\nNote: Set GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET, and ADMIN_EMAILS environment variables for OAuth\n")

    uvicorn.run(app, host="localhost", port=port, log_level="warning", access_log=False)
