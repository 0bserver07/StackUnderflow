"""Admin API endpoints for managing the public gallery."""

import json
from datetime import datetime
from pathlib import Path
from typing import Any

from auth import GoogleOAuth, require_admin

# Load .env.stackunderflow.dev for production configuration
from dotenv import load_dotenv
from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse

env_file = Path(__file__).parent.parent / ".env.stackunderflow.dev"
if env_file.exists():
    load_dotenv(env_file)


# Helper functions
def is_dev_mode() -> bool:
    """Check if running in development mode."""
    import os

    return os.getenv("ENV", "DEV") == "DEV"


def get_r2_client():
    """Get R2 client for production."""
    if is_dev_mode():
        return None

    import os

    import boto3

    # R2 uses S3-compatible API
    return boto3.client(
        "s3",
        endpoint_url=os.getenv("R2_ENDPOINT"),
        aws_access_key_id=os.getenv("R2_ACCESS_KEY_ID"),
        aws_secret_access_key=os.getenv("R2_SECRET_ACCESS_KEY"),
        region_name="auto",
    )


def get_r2_base_path() -> Path:
    """Get the base path for R2 storage (fake-r2 in dev mode)."""
    if is_dev_mode():
        return Path(__file__).parent.parent / "fake-r2"
    else:
        # In production, this should not return a path since R2 is accessed via API
        # This function should only be called in dev mode
        raise RuntimeError("get_r2_base_path() should not be called in production mode. Use R2 API instead.")


router = APIRouter()


def get_gallery_index_path() -> Path:
    """Get the path to gallery index file."""
    if is_dev_mode():
        return get_r2_base_path() / "gallery-index.json"
    else:
        # In production, this would be accessed via R2 API, not filesystem
        raise RuntimeError("get_gallery_index_path() should not be called in production mode. Use R2 API instead.")


def load_gallery_index() -> dict[str, Any]:
    """Load the gallery index."""
    if is_dev_mode():
        index_path = get_gallery_index_path()
        if index_path.exists():
            with open(index_path) as f:
                return json.load(f)
        return {"projects": []}
    else:
        # Production: Load from R2
        import os

        client = get_r2_client()
        bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        try:
            response = client.get_object(Bucket=bucket, Key="gallery-index.json")
            return json.loads(response["Body"].read().decode("utf-8"))
        except client.exceptions.NoSuchKey:
            return {"projects": []}
        except Exception as e:
            print(f"Error loading gallery index from R2: {e}")
            return {"projects": []}


def save_gallery_index(data: dict[str, Any]):
    """Save the gallery index."""
    if is_dev_mode():
        index_path = get_gallery_index_path()
        index_path.parent.mkdir(parents=True, exist_ok=True)
        with open(index_path, "w") as f:
            json.dump(data, f, indent=2)
    else:
        # Production: Save to R2
        import os

        client = get_r2_client()
        bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        try:
            client.put_object(
                Bucket=bucket, Key="gallery-index.json", Body=json.dumps(data, indent=2), ContentType="application/json"
            )
        except Exception as e:
            print(f"Error saving gallery index to R2: {e}")
            raise HTTPException(status_code=500, detail="Failed to save gallery index")


def get_share_stats() -> dict[str, Any]:
    """Get share statistics from the log file."""
    stats = {
        "total": 0,
        "public": 0,
        "private": 0,
        "with_commands": 0,
        "total_deleted": 0,
        "public_deleted": 0,
        "private_deleted": 0,
        "with_commands_deleted": 0,
        "by_day": {},
        "by_project": {},
    }

    # Track share IDs to check for deletion
    share_entries = []

    if is_dev_mode():
        log_file = get_r2_base_path() / "shares-log.jsonl"
        if log_file.exists():
            with open(log_file) as f:
                content = f.read()
                process_share_log_content(content, stats, share_entries)

        # Check for deleted shares in dev mode
        r2_base = get_r2_base_path()
        for entry in share_entries:
            share_file = r2_base / f"{entry['id']}.json"
            if not share_file.exists():
                stats["total_deleted"] += 1
                if entry.get("is_public"):
                    stats["public_deleted"] += 1
                else:
                    stats["private_deleted"] += 1
                if entry.get("include_commands"):
                    stats["with_commands_deleted"] += 1
    else:
        # Production: Read from R2
        import os

        client = get_r2_client()
        bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        try:
            # Get all log files from share-logs/ directory
            response = client.list_objects_v2(Bucket=bucket, Prefix="share-logs/")
            content = ""

            # Read all log files
            if "Contents" in response:
                for obj in response["Contents"]:
                    log_response = client.get_object(Bucket=bucket, Key=obj["Key"])
                    content += log_response["Body"].read().decode("utf-8")
            process_share_log_content(content, stats, share_entries)

            # Check for deleted shares in production
            for entry in share_entries:
                try:
                    # Try to check if the share file exists
                    client.head_object(Bucket=bucket, Key=f"shares/{entry['id']}.json")
                except client.exceptions.NoSuchKey:
                    # File doesn't exist - it's deleted
                    stats["total_deleted"] += 1
                    if entry.get("is_public"):
                        stats["public_deleted"] += 1
                    else:
                        stats["private_deleted"] += 1
                    if entry.get("include_commands"):
                        stats["with_commands_deleted"] += 1
                except Exception as e:
                    # Check if it's a 404 error (R2 might not use NoSuchKey)
                    if hasattr(e, "response") and e.response.get("Error", {}).get("Code") == "404":
                        # File doesn't exist - it's deleted
                        stats["total_deleted"] += 1
                        if entry.get("is_public"):
                            stats["public_deleted"] += 1
                        else:
                            stats["private_deleted"] += 1
                        if entry.get("include_commands"):
                            stats["with_commands_deleted"] += 1
                    else:
                        # Other errors - assume file exists
                        print(f"Error checking share {entry['id']}: {e}")
                        pass

        except client.exceptions.NoSuchKey:
            # No log file yet
            pass
        except Exception as e:
            print(f"Error loading share stats from R2: {e}")

    # Calculate active shares
    stats["total_active"] = stats["total"] - stats["total_deleted"]
    stats["public_active"] = stats["public"] - stats["public_deleted"]
    stats["private_active"] = stats["private"] - stats["private_deleted"]
    stats["with_commands_active"] = stats["with_commands"] - stats["with_commands_deleted"]

    # Sort and limit top projects
    top_projects = sorted(stats["by_project"].items(), key=lambda x: x[1], reverse=True)[:10]
    stats["top_projects"] = [{"name": name, "count": count} for name, count in top_projects]
    del stats["by_project"]  # Remove full list, keep only top

    # Convert by_day to sorted list
    stats["daily_counts"] = [{"date": date, "count": count} for date, count in sorted(stats["by_day"].items())]
    del stats["by_day"]

    return stats


def process_share_log_content(content: str, stats: dict[str, Any], share_entries: list[dict[str, Any]] = None):
    """Process share log content and update stats."""
    for line in content.strip().split("\n"):
        if line:
            try:
                entry = json.loads(line)
                stats["total"] += 1

                if entry.get("is_public"):
                    stats["public"] += 1
                else:
                    stats["private"] += 1

                if entry.get("include_commands"):
                    stats["with_commands"] += 1

                # Group by day
                created_date = entry["created_at"][:10]  # YYYY-MM-DD
                stats["by_day"][created_date] = stats["by_day"].get(created_date, 0) + 1

                # Group by project
                project = entry.get("project_name", "Unknown")
                stats["by_project"][project] = stats["by_project"].get(project, 0) + 1

                # Store entry for deletion check if share_entries list is provided
                if share_entries is not None:
                    share_entries.append(entry)

            except Exception:
                continue


@router.get("/login")
async def admin_login(request: Request):
    """Initiate admin login flow."""
    try:
        oauth = GoogleOAuth()

        # Check if OAuth is properly configured
        if not oauth.client_id or not oauth.client_secret:
            return HTMLResponse(
                content="""
                <html>
                <head><title>OAuth Configuration Error</title></head>
                <body style="font-family: sans-serif; padding: 2rem;">
                    <h1>OAuth Configuration Missing</h1>
                    <p>Google OAuth is not properly configured. Please ensure:</p>
                    <ol>
                        <li>The <code>.env.stackunderflow.dev</code> file exists in the project root</li>
                        <li>GOOGLE_CLIENT_ID is set</li>
                        <li>GOOGLE_CLIENT_SECRET is set</li>
                    </ol>
                    <p>Current values:</p>
                    <ul>
                        <li>Client ID: {}</li>
                        <li>Client Secret: {}</li>
                    </ul>
                    <p><a href="/">Back to homepage</a></p>
                </body>
                </html>
                """.format("Set" if oauth.client_id else "Not set", "Set" if oauth.client_secret else "Not set"),
                status_code=500,
            )

        # Check if already authenticated
        session_id = request.cookies.get("admin_session")
        if session_id and oauth.get_session(session_id):
            return RedirectResponse(url="/admin", status_code=302)

        # Generate state for CSRF protection
        state = oauth.create_session({"temp": True})  # Temporary session for state
        auth_url = oauth.get_auth_url(state)

        return RedirectResponse(url=auth_url, status_code=302)
    except Exception as e:
        import traceback

        return HTMLResponse(
            content=f"""
            <html>
            <head><title>Error</title></head>
            <body style="font-family: sans-serif; padding: 2rem;">
                <h1>Error in admin login</h1>
                <pre>{str(e)}\n\n{traceback.format_exc()}</pre>
                <p><a href="/">Back to homepage</a></p>
            </body>
            </html>
            """,
            status_code=500,
        )


@router.get("/callback")
async def admin_callback(request: Request, code: str, state: str):
    """Handle OAuth callback."""
    oauth = GoogleOAuth()

    # Verify state (CSRF protection)
    if not oauth.get_session(state):
        raise HTTPException(status_code=400, detail="Invalid state")
    oauth.delete_session(state)  # Clean up temporary session

    # Exchange code for token
    token_data = await oauth.exchange_code(code)
    access_token = token_data["access_token"]

    # Get user info
    user_info = await oauth.get_user_info(access_token)
    email = user_info["email"]

    # Check if authorized
    if not oauth.is_authorized_admin(email):
        raise HTTPException(status_code=403, detail=f"Unauthorized: {email} is not an admin")

    # Create session
    session_id = oauth.create_session(user_info)

    # Set cookie and redirect
    response = RedirectResponse(url="/admin", status_code=302)
    response.set_cookie(
        key="admin_session",
        value=session_id,
        httponly=True,
        secure=not is_dev_mode(),
        samesite="lax",
        max_age=7 * 24 * 60 * 60,  # 7 days
    )

    return response


@router.get("/logout")
async def admin_logout(request: Request):
    """Log out admin."""
    session_id = request.cookies.get("admin_session")
    if session_id:
        oauth = GoogleOAuth()
        oauth.delete_session(session_id)

    response = RedirectResponse(url="/", status_code=302)
    response.delete_cookie("admin_session")
    return response


@router.get("/api/gallery")
async def get_gallery(admin: dict[str, Any] = Depends(require_admin)):
    """Get all gallery projects with admin info."""
    gallery = load_gallery_index()

    # Add share URLs for each project
    base_url = "http://localhost:4001" if is_dev_mode() else "https://stackunderflow.dev"
    for project in gallery.get("projects", []):
        project["share_url"] = f"{base_url}/share/{project['id']}"

    return gallery


@router.post("/api/gallery/{share_id}/feature")
async def feature_project(share_id: str, admin: dict[str, Any] = Depends(require_admin)):
    """Mark a project as featured."""
    gallery = load_gallery_index()

    for project in gallery.get("projects", []):
        if project["id"] == share_id:
            project["featured"] = True
            project["featured_by"] = admin["email"]
            project["featured_at"] = datetime.now().isoformat()
            save_gallery_index(gallery)
            return {"success": True}

    raise HTTPException(status_code=404, detail="Project not found")


@router.post("/api/gallery/{share_id}/unfeature")
async def unfeature_project(share_id: str, admin: dict[str, Any] = Depends(require_admin)):
    """Remove featured status from a project."""
    gallery = load_gallery_index()

    for project in gallery.get("projects", []):
        if project["id"] == share_id:
            project["featured"] = False
            project.pop("featured_by", None)
            project.pop("featured_at", None)
            save_gallery_index(gallery)
            return {"success": True}

    raise HTTPException(status_code=404, detail="Project not found")


@router.delete("/api/gallery/{share_id}")
async def remove_project(share_id: str, admin: dict[str, Any] = Depends(require_admin)):
    """Remove a project from the gallery and/or delete the share entirely."""
    gallery = load_gallery_index()

    # Check if it's in the gallery
    original_count = len(gallery.get("projects", []))
    gallery["projects"] = [p for p in gallery.get("projects", []) if p["id"] != share_id]
    was_in_gallery = len(gallery["projects"]) < original_count

    # Delete the share data file
    share_existed = False

    if is_dev_mode():
        # Development: Delete from filesystem
        share_path = get_r2_base_path() / f"{share_id}.json"
        share_existed = share_path.exists()
        if share_existed:
            share_path.unlink()
    else:
        # Production: Delete from R2
        import os

        client = get_r2_client()
        bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        try:
            # Check if object exists
            client.head_object(Bucket=bucket, Key=f"shares/{share_id}.json")
            share_existed = True
            # Delete the object
            client.delete_object(Bucket=bucket, Key=f"shares/{share_id}.json")
        except client.exceptions.NoSuchKey:
            share_existed = False
        except Exception as e:
            # Check if it's a 404 error (R2 might not use NoSuchKey)
            if hasattr(e, "response") and e.response.get("Error", {}).get("Code") == "404":
                share_existed = False
            else:
                print(f"Error deleting share from R2: {e}")
                raise HTTPException(status_code=500, detail="Failed to delete share")

    # Update gallery if it was there
    if was_in_gallery:
        save_gallery_index(gallery)

    if share_existed or was_in_gallery:
        return {"success": True, "was_public": was_in_gallery}

    raise HTTPException(status_code=404, detail="Share not found")


@router.get("/api/me")
async def get_current_admin(admin: dict[str, Any] = Depends(require_admin)):
    """Get current admin info."""
    return {"email": admin["email"], "name": admin["name"], "picture": admin["picture"]}


@router.get("/api/share-stats")
async def get_share_statistics(admin: dict[str, Any] = Depends(require_admin)):
    """Get share statistics for admin dashboard."""
    return get_share_stats()


@router.get("/api/all-shares")
async def get_all_shares(admin: dict[str, Any] = Depends(require_admin)):
    """Get all active shared links (public and private) for admin review."""
    all_shares = []

    if is_dev_mode():
        # In development, read from the log file
        log_file = get_r2_base_path() / "shares-log.jsonl"
        r2_base = get_r2_base_path()

        if log_file.exists():
            with open(log_file) as f:
                for line in f:
                    try:
                        entry = json.loads(line.strip())
                        # Check if share file exists (not deleted)
                        share_file = r2_base / f"{entry['id']}.json"
                        if share_file.exists():
                            # Add share URL
                            base_url = "http://localhost:4001"
                            entry["share_url"] = f"{base_url}/share/{entry['id']}"
                            all_shares.append(entry)
                    except Exception:
                        continue

        # Sort by creation date, newest first
        all_shares.sort(key=lambda x: x.get("created_at", ""), reverse=True)
    else:
        # Production: Read from R2 shares-log.jsonl
        import os

        client = get_r2_client()
        bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")
        base_url = "https://stackunderflow.dev"

        try:
            # Get all log files from share-logs/ directory
            response = client.list_objects_v2(Bucket=bucket, Prefix="share-logs/")
            content = ""

            # Read all log files
            if "Contents" in response:
                for obj in response["Contents"]:
                    log_response = client.get_object(Bucket=bucket, Key=obj["Key"])
                    content += log_response["Body"].read().decode("utf-8")
            for line in content.strip().split("\n"):
                if line:
                    try:
                        entry = json.loads(line)
                        # Check if share file exists (not deleted)
                        try:
                            client.head_object(Bucket=bucket, Key=f"shares/{entry['id']}.json")
                            # File exists, include it
                            entry["share_url"] = f"{base_url}/share/{entry['id']}"
                            all_shares.append(entry)
                        except client.exceptions.NoSuchKey:
                            # File doesn't exist - it's deleted, skip it
                            pass
                    except Exception:
                        continue

            # Sort by creation date, newest first
            all_shares.sort(key=lambda x: x.get("created_at", ""), reverse=True)
        except client.exceptions.NoSuchKey:
            # No log file yet
            pass
        except Exception as e:
            print(f"Error loading shares log from R2: {e}")

    return {"shares": all_shares}
