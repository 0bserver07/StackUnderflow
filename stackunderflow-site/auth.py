"""Authentication module for admin access using Google OAuth."""

import json
import os
import secrets
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

import httpx
from fastapi import HTTPException, Request


# Helper functions
def is_dev_mode() -> bool:
    """Check if running in development mode."""
    return os.getenv("ENV", "DEV") == "DEV"


# Load .env.stackunderflow.dev file if it exists
# Try multiple possible locations for the env file
possible_paths = [
    Path(__file__).parent.parent / ".env.stackunderflow.dev",  # From stackunderflow/auth.py
    Path.cwd() / ".env.stackunderflow.dev",  # Current directory
    Path.cwd().parent / ".env.stackunderflow.dev",  # Parent directory
]
for env_file in possible_paths:
    if env_file.exists():
        from dotenv import load_dotenv

        load_dotenv(env_file)
        break


class GoogleOAuth:
    """Handle Google OAuth authentication flow."""

    def __init__(self):
        self.client_id = os.getenv("GOOGLE_CLIENT_ID", "")
        self.client_secret = os.getenv("GOOGLE_CLIENT_SECRET", "")
        # Use appropriate redirect URI based on environment
        if is_dev_mode():
            self.redirect_uri = os.getenv("GOOGLE_REDIRECT_URI_DEV", "http://localhost:8000/admin/callback")
        else:
            self.redirect_uri = os.getenv("GOOGLE_REDIRECT_URI_PROD", "https://stackunderflow.dev/admin/callback")
        self.authorized_emails = os.getenv("ADMIN_EMAILS", "").split(",")
        self.authorized_emails = [email.strip() for email in self.authorized_emails if email.strip()]

        # OAuth endpoints
        self.auth_url = "https://accounts.google.com/o/oauth2/v2/auth"
        self.token_url = "https://oauth2.googleapis.com/token"
        self.userinfo_url = "https://www.googleapis.com/oauth2/v2/userinfo"

        # Session storage (in production, use proper session store)
        self.sessions_file = Path.home() / ".stackunderflow" / "admin_sessions.json"
        self.sessions_file.parent.mkdir(exist_ok=True)
        self._load_sessions()

    def _load_sessions(self):
        """Load sessions from file."""
        if self.sessions_file.exists():
            with open(self.sessions_file) as f:
                self.sessions = json.load(f)
        else:
            self.sessions = {}

    def _save_sessions(self):
        """Save sessions to file."""
        with open(self.sessions_file, "w") as f:
            json.dump(self.sessions, f)

    def get_auth_url(self, state: str) -> str:
        """Generate Google OAuth authorization URL."""
        params = {
            "client_id": self.client_id,
            "redirect_uri": self.redirect_uri,
            "response_type": "code",
            "scope": "openid email profile",
            "state": state,
            "access_type": "online",
            "prompt": "select_account",
        }

        param_string = "&".join(f"{k}={v}" for k, v in params.items())
        return f"{self.auth_url}?{param_string}"

    async def exchange_code(self, code: str) -> dict[str, Any]:
        """Exchange authorization code for access token."""
        data = {
            "code": code,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
            "redirect_uri": self.redirect_uri,
            "grant_type": "authorization_code",
        }

        async with httpx.AsyncClient() as client:
            response = await client.post(self.token_url, data=data)

            if response.status_code != 200:
                raise HTTPException(status_code=400, detail="Failed to exchange code")

            return response.json()

    async def get_user_info(self, access_token: str) -> dict[str, Any]:
        """Get user information from Google."""
        headers = {"Authorization": f"Bearer {access_token}"}

        async with httpx.AsyncClient() as client:
            response = await client.get(self.userinfo_url, headers=headers)

            if response.status_code != 200:
                raise HTTPException(status_code=400, detail="Failed to get user info")

            return response.json()

    def is_authorized_admin(self, email: str) -> bool:
        """Check if email is in authorized admin list."""
        if is_dev_mode():
            # In dev mode, allow any email for testing
            return True
        return email in self.authorized_emails

    def create_session(self, user_info: dict[str, Any]) -> str:
        """Create admin session and return session ID."""
        session_id = secrets.token_urlsafe(32)

        # Handle temporary sessions for CSRF state
        if user_info.get("temp"):
            self.sessions[session_id] = {
                "temp": True,
                "created_at": datetime.now().isoformat(),
                "expires_at": (datetime.now() + timedelta(minutes=10)).isoformat(),  # Short expiry for temp sessions
            }
        else:
            # Regular user session
            self.sessions[session_id] = {
                "email": user_info["email"],
                "name": user_info.get("name", ""),
                "picture": user_info.get("picture", ""),
                "created_at": datetime.now().isoformat(),
                "expires_at": (datetime.now() + timedelta(days=7)).isoformat(),
            }

        self._save_sessions()
        return session_id

    def get_session(self, session_id: str) -> dict[str, Any] | None:
        """Get session by ID if valid."""
        session = self.sessions.get(session_id)
        if not session:
            return None

        # Check expiration
        expires_at = datetime.fromisoformat(session["expires_at"])
        if datetime.now() > expires_at:
            self.delete_session(session_id)
            return None

        return session

    def delete_session(self, session_id: str):
        """Delete a session."""
        if session_id in self.sessions:
            del self.sessions[session_id]
            self._save_sessions()


def require_admin(request: Request) -> dict[str, Any]:
    """Decorator/dependency to require admin authentication."""
    session_id = request.cookies.get("admin_session")
    if not session_id:
        raise HTTPException(status_code=401, detail="Not authenticated")

    oauth = GoogleOAuth()
    session = oauth.get_session(session_id)
    if not session:
        raise HTTPException(status_code=401, detail="Session expired")

    return session
