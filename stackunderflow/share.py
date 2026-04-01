"""
Share functionality for stackunderflow dashboards
"""

import json
import logging
import os
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

# Load .env.stackunderflow.dev for share configuration
from dotenv import load_dotenv

from .__version__ import __version__

env_file = Path(__file__).parent.parent / ".env.stackunderflow.dev"
if env_file.exists():
    load_dotenv(env_file)

logger = logging.getLogger(__name__)


class ShareManager:
    def __init__(self):
        from stackunderflow.settings import Settings
        config = Settings()
        
        # Check environment
        # For PyPI users, we should default to production mode
        env = os.getenv("ENV", "PROD" if not env_file.exists() else "DEV")
        logger.info(f"ShareManager: ENV={env}, env_file={env_file}, exists={env_file.exists()}")

        if env == "DEV":
            # Development mode
            # Use config system with fallback to env var for backwards compatibility
            self.base_url = config.get("share_base_url", os.getenv("SHARE_BASE_URL", "http://localhost:4001"))
            self.r2_endpoint = os.getenv("SHARE_STORAGE_PATH", "/tmp/stackunderflow-fake-r2")
            self.is_production = False
            logger.info(f"ShareManager: Development mode, base_url={self.base_url}")
        else:
            self.base_url = config.get("share_base_url", "https://stackunderflow.dev")
            # For PyPI users, they can't directly access R2
            # Check if we have R2 credentials
            if os.getenv("R2_ACCESS_KEY_ID"):
                # Internal use with direct R2 access
                self.r2_endpoint = os.getenv("R2_ENDPOINT", "https://r2.stackunderflow.dev")
            else:
                # PyPI users use public API
                self.r2_endpoint = config.get("share_api_url", "https://stackunderflow.dev")
            self.is_production = True
            logger.info(f"ShareManager: Production mode, base_url={self.base_url}, r2_endpoint={self.r2_endpoint}")


    async def create_share_link(
        self,
        statistics: dict[str, Any],
        charts_data: dict[str, Any],
        make_public: bool = False,
        include_commands: bool = False,
        user_commands: list = None,
        project_name: str = None,
        request_info: dict = None,
    ) -> dict[str, Any]:
        """Create a shareable link for the current dashboard"""
        share_id = str(uuid.uuid4())[:24]  # Use 24 characters for better uniqueness

        # Prepare static dashboard data
        dashboard_data = {
            "id": share_id,
            "created_at": datetime.now(UTC).isoformat(),
            "statistics": self._sanitize_statistics(statistics),
            "charts": charts_data,  # Now expects chart configurations, not images
            "user_commands": user_commands if include_commands else [],
            "version": __version__,
            "is_public": make_public,
            "title": (project_name or self._generate_title(statistics)) if make_public else None,
            "description": self._generate_description(statistics) if make_public else None,
            "project_name": project_name or self._get_project_name(statistics),
        }

        # Save to fake-r2 folder for testing
        await self._upload_to_storage(share_id, dashboard_data)

        # If public, add to gallery index
        if make_public:
            await self._add_to_public_gallery(share_id, dashboard_data)

        # Log share creation for analytics
        await self._log_share_creation(share_id, dashboard_data, request_info)

        return {
            "url": f"{self.base_url}/share/{share_id}",
            "is_public": make_public,
        }

    def _sanitize_statistics(self, stats: dict[str, Any]) -> dict[str, Any]:
        """Remove any sensitive information from statistics"""
        # Deep copy to avoid modifying original
        import copy

        sanitized = copy.deepcopy(stats)

        # Remove file paths but keep project name for display
        if "overview" in sanitized:
            sanitized["overview"].pop("log_directory", None)
            # Keep log_dir_name as it's just the folder name, not full path

        return sanitized

    def _get_project_name(self, stats: dict[str, Any]) -> str:
        """Extract project name from statistics"""
        return stats.get("overview", {}).get("project_name", "Unknown Project")

    def _generate_title(self, stats: dict[str, Any]) -> str:
        """Generate a descriptive title for public gallery"""
        # Use the project name as the title
        return stats.get("overview", {}).get("log_dir_name", "Unknown Project")

    def _generate_description(self, stats: dict[str, Any]) -> str:
        """Generate a summary for public gallery"""
        # For now, return empty string since we'll show metrics as icons
        return ""

    def _format_number(self, num: int) -> str:
        """Format large numbers with K/M suffix"""
        if num >= 1_000_000:
            return f"{num / 1_000_000:.1f}M"
        elif num >= 1_000:
            return f"{num / 1_000:.1f}K"
        else:
            return str(num)

    async def _upload_to_storage(self, share_id: str, data: dict[str, Any]):
        """Upload dashboard data to storage"""
        if self.is_production:
            # Check if we have R2 credentials (internal use)
            if os.getenv("R2_ACCESS_KEY_ID"):
                # Direct R2 upload (for internal/development use)
                await self._upload_to_r2(share_id, data)
            else:
                # Use public API endpoint (for PyPI users)
                await self._upload_via_api(share_id, data)
        else:
            # Development: Save to local fake-r2 folder
            storage_dir = Path(self.r2_endpoint)
            storage_dir.mkdir(exist_ok=True)

            file_path = storage_dir / f"{share_id}.json"
            with open(file_path, "w") as f:
                json.dump(data, f, indent=2)

            logger.info(f"Saved share data to {file_path}")

    async def _upload_via_api(self, share_id: str, data: dict[str, Any]):
        """Upload share data via public API endpoint (for PyPI users)"""
        import httpx
        
        # API endpoint for share uploads
        api_url = f"{self.r2_endpoint}/api/shares"
        
        try:
            async with httpx.AsyncClient(timeout=30.0) as client:
                # Prepare the share data with metadata
                payload = {
                    "share_id": share_id,
                    "data": data,
                    "is_public": data.get("is_public", False),
                }
                
                # POST to the API endpoint
                response = await client.post(api_url, json=payload)
                response.raise_for_status()
                
                logger.info(f"Uploaded share via API: {share_id}")
                
                # Gallery update is handled by the API endpoint itself
                    
        except httpx.HTTPError as e:
            logger.error(f"Failed to upload share via API: {e}")
            raise Exception(f"Failed to upload share: {str(e)}")

    async def _add_to_public_gallery(self, share_id: str, data: dict[str, Any]):
        """Add project to public gallery index"""
        if self.is_production:
            # Check if we have R2 credentials (internal use)
            if os.getenv("R2_ACCESS_KEY_ID"):
                # Direct R2 update
                await self._update_r2_gallery(share_id, data)
            # For API users, gallery update is handled by the API endpoint itself
        else:
            # Development: Update local gallery file
            gallery_file = Path(self.r2_endpoint) / "gallery-index.json"

            # Load existing gallery or create new one
            if gallery_file.exists():
                with open(gallery_file) as f:
                    gallery = json.load(f)
            else:
                gallery = {"projects": []}

            # Add new project
            stats = data["statistics"]
            total_tokens = stats.get("overview", {}).get("total_tokens", {})
            total_token_count = total_tokens.get("input", 0) + total_tokens.get("output", 0)

            # Calculate duration from date range (inclusive, like in the dashboard)
            date_range = stats.get("overview", {}).get("date_range", {})
            duration_days = 0
            if date_range.get("start") and date_range.get("end"):
                from datetime import datetime

                start = datetime.fromisoformat(date_range["start"].replace("Z", "+00:00"))
                end = datetime.fromisoformat(date_range["end"].replace("Z", "+00:00"))
                # Add 1 to make it inclusive (same as calculateDaysInclusive in stats.js)
                duration_days = (end - start).days + 1

            gallery["projects"].insert(
                0,
                {
                    "id": share_id,
                    "title": data["title"],
                    "description": data["description"],
                    "project_name": data.get("project_name", "Unknown Project"),
                    "created_at": data["created_at"],
                    "includes_commands": len(data.get("user_commands", [])) > 0,
                    "stats": {
                        "total_commands": stats.get("user_interactions", {}).get("user_commands_analyzed", 0),
                        "total_tokens": total_token_count,
                        "duration_days": duration_days,
                        "total_cost": stats.get("overview", {}).get("total_cost", 0),
                        "interruption_rate": stats.get("user_interactions", {}).get("interruption_rate", 0),
                        "avg_steps_per_command": stats.get("user_interactions", {}).get("avg_steps_per_command", 0),
                    },
                },
            )

            # Keep all projects (no limit)

            # Save gallery index
            with open(gallery_file, "w") as f:
                json.dump(gallery, f, indent=2)

            logger.info(f"Added to public gallery: {share_id}")

    async def _upload_to_r2(self, share_id: str, data: dict[str, Any]):
        """Upload to Cloudflare R2 in production"""
        import boto3
        from botocore.exceptions import ClientError

        # Get R2 credentials from environment
        r2_endpoint = os.getenv("R2_ENDPOINT")
        r2_access_key = os.getenv("R2_ACCESS_KEY_ID")
        r2_secret_key = os.getenv("R2_SECRET_ACCESS_KEY")
        r2_bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        if not all([r2_endpoint, r2_access_key, r2_secret_key]):
            raise ValueError(
                "R2 credentials not configured. Please set R2_ENDPOINT, "
                "R2_ACCESS_KEY_ID, and R2_SECRET_ACCESS_KEY environment variables."
            )

        # Create S3 client for R2 (R2 is S3-compatible)
        client = boto3.client(
            "s3",
            endpoint_url=r2_endpoint,
            aws_access_key_id=r2_access_key,
            aws_secret_access_key=r2_secret_key,
            region_name="auto",  # R2 uses 'auto' region
        )

        try:
            # Upload share data as JSON
            json_data = json.dumps(data, indent=2)
            client.put_object(
                Bucket=r2_bucket,
                Key=f"shares/{share_id}.json",
                Body=json_data,
                ContentType="application/json",
            )
            logger.info(f"Uploaded share to R2: {share_id}")
        except ClientError as e:
            logger.error(f"Failed to upload share to R2: {e}")
            raise Exception(f"Failed to upload share data: {str(e)}")

    async def _update_r2_gallery(self, share_id: str, data: dict[str, Any]):
        """Update gallery index in R2 in production"""
        import boto3
        from botocore.exceptions import ClientError

        # Get R2 credentials from environment
        r2_endpoint = os.getenv("R2_ENDPOINT")
        r2_access_key = os.getenv("R2_ACCESS_KEY_ID")
        r2_secret_key = os.getenv("R2_SECRET_ACCESS_KEY")
        r2_bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        if not all([r2_endpoint, r2_access_key, r2_secret_key]):
            raise ValueError(
                "R2 credentials not configured. Please set R2_ENDPOINT, "
                "R2_ACCESS_KEY_ID, and R2_SECRET_ACCESS_KEY environment variables."
            )

        # Create S3 client for R2
        client = boto3.client(
            "s3",
            endpoint_url=r2_endpoint,
            aws_access_key_id=r2_access_key,
            aws_secret_access_key=r2_secret_key,
            region_name="auto",
        )

        try:
            # Download existing gallery index
            gallery = {"projects": []}
            try:
                response = client.get_object(Bucket=r2_bucket, Key="gallery-index.json")
                gallery = json.loads(response["Body"].read().decode("utf-8"))
            except client.exceptions.NoSuchKey:
                # Gallery doesn't exist yet, start with empty
                logger.info("Gallery index not found, creating new one")

            # Add new project to gallery
            stats = data["statistics"]
            total_tokens = stats.get("overview", {}).get("total_tokens", {})
            total_token_count = total_tokens.get("input", 0) + total_tokens.get("output", 0)

            # Calculate duration from date range (inclusive)
            date_range = stats.get("overview", {}).get("date_range", {})
            duration_days = 0
            if date_range.get("start") and date_range.get("end"):
                from datetime import datetime

                start = datetime.fromisoformat(date_range["start"].replace("Z", "+00:00"))
                end = datetime.fromisoformat(date_range["end"].replace("Z", "+00:00"))
                duration_days = (end - start).days + 1

            gallery["projects"].insert(
                0,
                {
                    "id": share_id,
                    "title": data["title"],
                    "description": data["description"],
                    "project_name": data.get("project_name", "Unknown Project"),
                    "created_at": data["created_at"],
                    "includes_commands": len(data.get("user_commands", [])) > 0,
                    "stats": {
                        "total_commands": stats.get("user_interactions", {}).get("user_commands_analyzed", 0),
                        "total_tokens": total_token_count,
                        "duration_days": duration_days,
                        "total_cost": stats.get("overview", {}).get("total_cost", 0),
                        "interruption_rate": stats.get("user_interactions", {}).get("interruption_rate", 0),
                        "avg_steps_per_command": stats.get("user_interactions", {}).get("avg_steps_per_command", 0),
                    },
                },
            )

            # Upload updated gallery index
            gallery_json = json.dumps(gallery, indent=2)
            client.put_object(
                Bucket=r2_bucket,
                Key="gallery-index.json",
                Body=gallery_json,
                ContentType="application/json",
            )

            logger.info(f"Updated R2 gallery with share: {share_id}")

        except ClientError as e:
            logger.error(f"Failed to update gallery in R2: {e}")
            raise Exception(f"Failed to update gallery: {str(e)}")

    async def _log_share_creation(self, share_id: str, data: dict[str, Any], request_info: dict = None):
        """Log share creation for analytics"""
        import hashlib

        log_entry = {
            "id": share_id,
            "created_at": data["created_at"],
            "is_public": data.get("is_public", False),
            "project_name": data.get("project_name", "Unknown"),
            "include_commands": len(data.get("user_commands", [])) > 0,
        }

        # Add request info if provided
        if request_info:
            if request_info.get("ip"):
                # Hash IP for privacy
                ip_hash = hashlib.sha256(request_info["ip"].encode()).hexdigest()
                log_entry["ip_hash"] = ip_hash
            if request_info.get("user_agent"):
                log_entry["user_agent"] = request_info["user_agent"]

        if self.is_production:
            # Production: Append to R2 log file
            await self._append_to_r2_log(log_entry)
        else:
            # Development: Append to local JSONL file
            log_file = Path(self.r2_endpoint) / "shares-log.jsonl"
            try:
                with open(log_file, "a") as f:
                    f.write(json.dumps(log_entry) + "\n")
                logger.info(f"Share logged to {log_file}: {share_id}")
            except Exception as e:
                logger.error(f"Failed to log share creation: {e}")

    async def _append_to_r2_log(self, log_entry: dict[str, Any]):
        """Append share creation log to R2 in production"""
        from datetime import datetime

        import boto3
        from botocore.exceptions import ClientError

        # Get R2 credentials
        r2_endpoint = os.getenv("R2_ENDPOINT")
        r2_access_key = os.getenv("R2_ACCESS_KEY_ID")
        r2_secret_key = os.getenv("R2_SECRET_ACCESS_KEY")
        r2_bucket = os.getenv("R2_BUCKET_NAME", "stackunderflow-shares")

        if not all([r2_endpoint, r2_access_key, r2_secret_key]):
            logger.warning("R2 credentials not configured for logging")
            return

        client = boto3.client(
            "s3",
            endpoint_url=r2_endpoint,
            aws_access_key_id=r2_access_key,
            aws_secret_access_key=r2_secret_key,
            region_name="auto",
        )

        try:
            # Get current date for log file name (one file per day)
            log_date = datetime.now(UTC).strftime("%Y-%m-%d")
            log_key = f"logs/shares-{log_date}.jsonl"

            # Download existing log file or start new
            existing_log = ""
            try:
                response = client.get_object(Bucket=r2_bucket, Key=log_key)
                existing_log = response["Body"].read().decode("utf-8")
            except client.exceptions.NoSuchKey:
                # Log file doesn't exist yet for today
                pass

            # Append new entry
            new_log = existing_log + json.dumps(log_entry) + "\n"

            # Upload updated log
            client.put_object(Bucket=r2_bucket, Key=log_key, Body=new_log, ContentType="text/plain")

            logger.info(f"Share logged to R2: {log_entry['id']}")

        except ClientError as e:
            logger.error(f"Failed to log share to R2: {e}")
