"""
Tag service for auto-detecting and managing session tags.

Detects languages, frameworks, topics, and tools from session content.
Stores tags in ~/.stackunderflow/tags.json as a simple JSON file.
"""

import json
import logging
import re
from collections import defaultdict
from pathlib import Path

logger = logging.getLogger(__name__)

# Well-known GitHub language colors
LANGUAGE_COLORS = {
    "python": "#3572A5",
    "javascript": "#f1e05a",
    "typescript": "#2b7489",
    "go": "#00ADD8",
    "rust": "#dea584",
    "java": "#b07219",
    "c": "#555555",
    "cpp": "#f34b7d",
    "csharp": "#178600",
    "ruby": "#701516",
    "php": "#4F5D95",
    "swift": "#F05138",
    "kotlin": "#A97BFF",
    "scala": "#c22d40",
    "html": "#e34c26",
    "css": "#563d7c",
    "scss": "#c6538c",
    "shell": "#89e051",
    "bash": "#89e051",
    "lua": "#000080",
    "r": "#198CE7",
    "dart": "#00B4AB",
    "elixir": "#6e4a7e",
    "haskell": "#5e5086",
    "sql": "#e38c00",
    "yaml": "#cb171e",
    "json": "#292929",
    "toml": "#9c4221",
    "markdown": "#083fa1",
    "vue": "#41b883",
    "svelte": "#ff3e00",
    "zig": "#ec915c",
    "nix": "#7e7eff",
    "proto": "#4a6f8a",
    "graphql": "#e10098",
    "terraform": "#5C4EE5",
}

FRAMEWORK_COLORS = {
    "fastapi": "#009688",
    "flask": "#000000",
    "django": "#092E20",
    "express": "#000000",
    "react": "#61dafb",
    "nextjs": "#000000",
    "vue": "#41b883",
    "angular": "#dd0031",
    "svelte": "#ff3e00",
    "tailwind": "#06b6d4",
    "pytorch": "#ee4c2c",
    "tensorflow": "#ff6f00",
    "sqlalchemy": "#d71f00",
    "prisma": "#2D3748",
    "rails": "#CC0000",
    "spring": "#6DB33F",
    "nestjs": "#E0234E",
    "nuxt": "#00DC82",
    "remix": "#000000",
    "astro": "#FF5D01",
    "vite": "#646CFF",
    "webpack": "#8DD6F9",
    "docker": "#2496ED",
    "kubernetes": "#326CE5",
    "terraform": "#5C4EE5",
    "ansible": "#EE0000",
    "pytest": "#009fe3",
    "jest": "#C21325",
    "storybook": "#FF4785",
    "graphql": "#e10098",
    "redis": "#DC382D",
    "postgres": "#4169E1",
    "mongodb": "#47A248",
    "supabase": "#3ECF8E",
    "firebase": "#FFCA28",
    "aws": "#FF9900",
    "gcp": "#4285F4",
    "azure": "#0078D4",
    "pydantic": "#E92063",
    "celery": "#37814A",
    "htmx": "#3366CC",
}

TOPIC_COLORS = {
    "debugging": "#e53e3e",
    "testing": "#38a169",
    "refactoring": "#805ad5",
    "devops": "#2b6cb0",
    "authentication": "#d69e2e",
    "api-development": "#3182ce",
    "frontend-styling": "#ed64a6",
    "database": "#dd6b20",
    "performance": "#e53e3e",
    "security": "#c53030",
    "documentation": "#4a5568",
    "deployment": "#2b6cb0",
    "configuration": "#718096",
    "data-processing": "#2d3748",
    "migration": "#9b2c2c",
    "ci-cd": "#2c5282",
}

TOOL_COLORS = {
    "Read": "#718096",
    "Write": "#718096",
    "Edit": "#718096",
    "MultiEdit": "#718096",
    "Bash": "#718096",
    "Grep": "#718096",
    "Glob": "#718096",
    "Task": "#718096",
    "WebFetch": "#718096",
    "WebSearch": "#718096",
    "NotebookEdit": "#718096",
    "TodoRead": "#718096",
    "TodoWrite": "#718096",
}

# File extension -> language mapping
EXTENSION_TO_LANGUAGE = {
    ".py": "python",
    ".pyw": "python",
    ".pyi": "python",
    ".js": "javascript",
    ".jsx": "javascript",
    ".mjs": "javascript",
    ".cjs": "javascript",
    ".ts": "typescript",
    ".tsx": "typescript",
    ".mts": "typescript",
    ".go": "go",
    ".rs": "rust",
    ".java": "java",
    ".c": "c",
    ".h": "c",
    ".cpp": "cpp",
    ".cc": "cpp",
    ".cxx": "cpp",
    ".hpp": "cpp",
    ".cs": "csharp",
    ".rb": "ruby",
    ".php": "php",
    ".swift": "swift",
    ".kt": "kotlin",
    ".kts": "kotlin",
    ".scala": "scala",
    ".html": "html",
    ".htm": "html",
    ".css": "css",
    ".scss": "scss",
    ".sass": "scss",
    ".less": "css",
    ".sh": "shell",
    ".bash": "bash",
    ".zsh": "shell",
    ".lua": "lua",
    ".r": "r",
    ".R": "r",
    ".dart": "dart",
    ".ex": "elixir",
    ".exs": "elixir",
    ".hs": "haskell",
    ".sql": "sql",
    ".yaml": "yaml",
    ".yml": "yaml",
    ".json": "json",
    ".toml": "toml",
    ".md": "markdown",
    ".vue": "vue",
    ".svelte": "svelte",
    ".zig": "zig",
    ".nix": "nix",
    ".proto": "proto",
    ".graphql": "graphql",
    ".gql": "graphql",
    ".tf": "terraform",
}

# Framework detection patterns: (pattern_in_content, framework_name)
FRAMEWORK_PATTERNS = [
    (r"\bfrom\s+fastapi\b|\bimport\s+fastapi\b|\bFastAPI\b", "fastapi"),
    (r"\bfrom\s+flask\b|\bimport\s+flask\b|\bFlask\b", "flask"),
    (r"\bfrom\s+django\b|\bimport\s+django\b|\bDjango\b", "django"),
    (r"\brequire\s*\(\s*['\"]express['\"]\s*\)|\bfrom\s+['\"]express['\"]", "express"),
    (r"\bimport\s+React\b|\bfrom\s+['\"]react['\"]|\buseState\b|\buseEffect\b", "react"),
    (r"\bfrom\s+['\"]next['\"/]|\bnext\.config\b|\bgetServerSideProps\b|\bgetStaticProps\b", "nextjs"),
    (r"\bfrom\s+['\"]vue['\"]|\bcreateApp\b|\bdefineComponent\b|\.vue\b", "vue"),
    (r"\b@angular\b|\bfrom\s+['\"]@angular\b|\bNgModule\b", "angular"),
    (r"\bfrom\s+['\"]svelte['\"]|\b\.svelte\b", "svelte"),
    (r"\btailwindcss\b|\btailwind\.config\b|class=\"[^\"]*\b(?:flex|grid|text-|bg-|p-|m-)\b", "tailwind"),
    (r"\bimport\s+torch\b|\bfrom\s+torch\b", "pytorch"),
    (r"\bimport\s+tensorflow\b|\bfrom\s+tensorflow\b", "tensorflow"),
    (r"\bfrom\s+sqlalchemy\b|\bimport\s+sqlalchemy\b", "sqlalchemy"),
    (r"\bfrom\s+['\"]@prisma\b|\bprisma\.schema\b|\bPrismaClient\b", "prisma"),
    (r"\bRails\b|\bActiveRecord\b|\bActionController\b", "rails"),
    (r"\b@SpringBoot\b|\bSpringApplication\b", "spring"),
    (r"\b@nestjs\b|\bfrom\s+['\"]@nestjs\b", "nestjs"),
    (r"\bnuxt\.config\b|\bfrom\s+['\"]nuxt['\"]", "nuxt"),
    (r"\bremix\.config\b|\bfrom\s+['\"]@remix-run\b", "remix"),
    (r"\bastro\.config\b|\bfrom\s+['\"]astro\b", "astro"),
    (r"\bvite\.config\b|\bfrom\s+['\"]vite\b", "vite"),
    (r"\bwebpack\.config\b|\bfrom\s+['\"]webpack\b", "webpack"),
    (r"\bDockerfile\b|\bdocker-compose\b|\bdocker\s+build\b", "docker"),
    (r"\bkubectl\b|\bkubernetes\b|\bk8s\b|\.kube\b", "kubernetes"),
    (r"\bterraform\b|\b\.tf\b|\bterraform\s+(?:init|plan|apply)\b", "terraform"),
    (r"\bansible\b|\bplaybook\b|\b\.ansible\b", "ansible"),
    (r"\bimport\s+pytest\b|\bfrom\s+pytest\b|\b@pytest\b|\.pytest\b", "pytest"),
    (r"\bjest\.config\b|\bdescribe\s*\(\s*['\"]|\bit\s*\(\s*['\"]", "jest"),
    (r"\bstorybook\b|\b\.stories\.", "storybook"),
    (r"\bGraphQL\b|\bgql\`|\btype\s+Query\b|\btype\s+Mutation\b", "graphql"),
    (r"\bredis\b|\bRedis\b|\bREDIS_URL\b", "redis"),
    (r"\bpostgres\b|\bPostgreSQL\b|\bpg_\b|\bCREATE\s+TABLE\b", "postgres"),
    (r"\bmongodb\b|\bMongoClient\b|\bmongoose\b", "mongodb"),
    (r"\bsupabase\b|\bfrom\s+['\"]@supabase\b", "supabase"),
    (r"\bfirebase\b|\bfrom\s+['\"]firebase\b", "firebase"),
    (r"\baws\b|\bboto3\b|\bs3\b|\blambda\b.*\baws\b", "aws"),
    (r"\bgcloud\b|\bgcp\b|\bgoogle\.cloud\b", "gcp"),
    (r"\bazure\b|\bAzure\b|\baz\s+", "azure"),
    (r"\bfrom\s+pydantic\b|\bimport\s+pydantic\b|\bBaseModel\b", "pydantic"),
    (r"\bfrom\s+celery\b|\bimport\s+celery\b|\bcelery\b", "celery"),
    (r"\bhtmx\b|\bhx-get\b|\bhx-post\b|\bhx-trigger\b", "htmx"),
]

# Topic detection patterns: (pattern_in_content, topic_name)
TOPIC_PATTERNS = [
    (r"\berror\b|\bbug\b|\bfix\b|\bfixing\b|\bdebug\b|\bbreaking\b|\bbroken\b|\btraceback\b|\bexception\b|\bcrash\b", "debugging"),
    (r"\btest\b|\btesting\b|\bunit\s*test\b|\btest_\b|\b_test\.|\bspec\b|\bassert\b|\bmock\b", "testing"),
    (r"\brefactor\b|\brefactoring\b|\bcleanup\b|\brestructure\b|\breorganize\b|\bsimplify\b", "refactoring"),
    (r"\bdeploy\b|\bdeployment\b|\bdocker\b|\bci/cd\b|\bpipeline\b|\bgithub\s*actions?\b|\bjenkins\b", "devops"),
    (r"\bauth\b|\bauthoriz\b|\bauthenticat\b|\blogin\b|\bsignup\b|\bsign.?in\b|\bpassword\b|\bjwt\b|\boauth\b|\btoken\b", "authentication"),
    (r"\bapi\b|\bendpoint\b|\broute\b|\brequest\b|\bresponse\b|\brest\b|\bhttp\b|\bwebhook\b", "api-development"),
    (r"\bcss\b|\bstyle\b|\bstyling\b|\blayout\b|\bresponsive\b|\banimation\b|\btheme\b|\btailwind\b", "frontend-styling"),
    (r"\bdatabase\b|\bsql\b|\bquery\b|\bmigration\b|\bschema\b|\bindex\b|\bjoin\b|\borm\b|\btable\b", "database"),
    (r"\bperformance\b|\boptimiz\b|\blatency\b|\bbenchmark\b|\bcaching\b|\bprofile\b|\bslow\b", "performance"),
    (r"\bsecurity\b|\bvulnerability\b|\bsanitize\b|\bencrypt\b|\bxss\b|\bcsrf\b|\binjection\b", "security"),
    (r"\bdocumentation\b|\bdocstring\b|\breadme\b|\bcomment\b|\bjsdoc\b|\btypedoc\b", "documentation"),
    (r"\bconfig\b|\bconfiguration\b|\bsettings\b|\benv\b|\benvironment\b|\.env\b", "configuration"),
    (r"\bdata\s*process\b|\betl\b|\bpipeline\b|\btransform\b|\bpandas\b|\bcsv\b|\bparquet\b", "data-processing"),
    (r"\bmigrat\b|\bupgrade\b|\bdowngrade\b|\balembic\b|\bknex\b", "migration"),
    (r"\bci\b|\bcd\b|\bgithub.?actions?\b|\bworkflow\b|\bpipeline\b|\bbuild\b", "ci-cd"),
]


class TagService:
    """Service for auto-detecting and managing session tags."""

    def __init__(self):
        self.storage_dir = Path.home() / ".stackunderflow"
        self.tags_file = self.storage_dir / "tags.json"

        # Ensure storage directory exists
        self.storage_dir.mkdir(parents=True, exist_ok=True)

    def _load_tags(self) -> dict:
        """Load all tag data from the JSON file."""
        if not self.tags_file.exists():
            return {
                "auto_tags": {},
                "manual_tags": {},
                "tag_metadata": {},
            }

        try:
            data = json.loads(self.tags_file.read_text())
            # Ensure all required keys exist
            if "auto_tags" not in data:
                data["auto_tags"] = {}
            if "manual_tags" not in data:
                data["manual_tags"] = {}
            if "tag_metadata" not in data:
                data["tag_metadata"] = {}
            return data
        except (OSError, json.JSONDecodeError) as e:
            logger.warning(f"Error loading tags: {e}")
            return {
                "auto_tags": {},
                "manual_tags": {},
                "tag_metadata": {},
            }

    def _save_tags(self, data: dict):
        """Save tag data to the JSON file."""
        try:
            self.tags_file.write_text(json.dumps(data, indent=2))
        except OSError as e:
            logger.error(f"Error saving tags: {e}")
            raise

    def _build_tag_metadata(self) -> dict:
        """Build the tag metadata dictionary with colors and categories."""
        metadata = {}

        for lang, color in LANGUAGE_COLORS.items():
            metadata[lang] = {"color": color, "category": "language"}

        for fw, color in FRAMEWORK_COLORS.items():
            metadata[fw] = {"color": color, "category": "framework"}

        for topic, color in TOPIC_COLORS.items():
            metadata[topic] = {"color": color, "category": "topic"}

        for tool, color in TOOL_COLORS.items():
            metadata[tool] = {"color": color, "category": "tool"}

        return metadata

    def auto_tag_session(self, session_id: str, messages: list[dict]) -> list[str]:
        """Auto-detect tags for a session from its messages.

        Analyzes message content, tool calls, and file extensions to detect:
        - Programming languages (from file extensions and code hints)
        - Frameworks (from import statements and patterns)
        - Topics (from content patterns)
        - Tools used (from tool call data)

        Args:
            session_id: The session identifier
            messages: List of message dicts for this session

        Returns:
            List of auto-detected tag names
        """
        tags = set()

        # Collect all text content and tool info from messages in this session
        all_content = []
        all_file_paths = []
        all_tool_names = set()

        for msg in messages:
            if msg.get("session_id") != session_id:
                continue

            # Collect text content
            content = msg.get("content", "")
            if content:
                all_content.append(content)

            # Collect tools used
            tools = msg.get("tools", [])
            for tool in tools:
                tool_name = tool.get("name", "")
                if tool_name:
                    all_tool_names.add(tool_name)

                # Extract file paths from tool inputs
                tool_input = tool.get("input", {})
                if isinstance(tool_input, dict):
                    file_path = tool_input.get("file_path", "")
                    if file_path:
                        all_file_paths.append(file_path)
                        # Also add file path to content for framework detection
                        # (e.g., tailwind.config.ts, vite.config.js)
                        all_content.append(file_path)

                    # Also check command for file references
                    command = tool_input.get("command", "")
                    if command:
                        all_content.append(command)

                    # Check pattern for Grep/Glob
                    pattern = tool_input.get("pattern", "")
                    if pattern:
                        all_content.append(pattern)

        # Combine all text for pattern matching
        combined_text = "\n".join(all_content)

        # 1. Detect languages from file extensions
        for file_path in all_file_paths:
            ext = Path(file_path).suffix.lower()
            if ext in EXTENSION_TO_LANGUAGE:
                tags.add(EXTENSION_TO_LANGUAGE[ext])

        # 2. Detect languages from code block hints in content
        code_block_langs = re.findall(r"```(\w+)", combined_text)
        for lang_hint in code_block_langs:
            lang_lower = lang_hint.lower()
            # Map common code block language hints
            code_hint_map = {
                "python": "python",
                "py": "python",
                "javascript": "javascript",
                "js": "javascript",
                "typescript": "typescript",
                "ts": "typescript",
                "tsx": "typescript",
                "jsx": "javascript",
                "go": "go",
                "golang": "go",
                "rust": "rust",
                "rs": "rust",
                "java": "java",
                "c": "c",
                "cpp": "cpp",
                "csharp": "csharp",
                "cs": "csharp",
                "ruby": "ruby",
                "rb": "ruby",
                "php": "php",
                "swift": "swift",
                "kotlin": "kotlin",
                "kt": "kotlin",
                "scala": "scala",
                "html": "html",
                "css": "css",
                "scss": "scss",
                "sass": "scss",
                "shell": "shell",
                "bash": "bash",
                "sh": "shell",
                "zsh": "shell",
                "lua": "lua",
                "r": "r",
                "dart": "dart",
                "elixir": "elixir",
                "haskell": "haskell",
                "sql": "sql",
                "yaml": "yaml",
                "yml": "yaml",
                "json": "json",
                "toml": "toml",
                "markdown": "markdown",
                "md": "markdown",
                "vue": "vue",
                "svelte": "svelte",
                "zig": "zig",
                "nix": "nix",
                "proto": "proto",
                "protobuf": "proto",
                "graphql": "graphql",
                "gql": "graphql",
                "terraform": "terraform",
                "tf": "terraform",
                "hcl": "terraform",
            }
            if lang_lower in code_hint_map:
                tags.add(code_hint_map[lang_lower])

        # 3. Detect frameworks from content patterns
        for pattern, framework in FRAMEWORK_PATTERNS:
            if re.search(pattern, combined_text, re.IGNORECASE):
                tags.add(framework)

        # 4. Detect topics from content patterns
        for pattern, topic in TOPIC_PATTERNS:
            if re.search(pattern, combined_text, re.IGNORECASE):
                tags.add(topic)

        # 5. Add tools used as tags
        for tool_name in all_tool_names:
            if tool_name in TOOL_COLORS:
                tags.add(tool_name)

        return sorted(tags)

    def auto_tag_all_sessions(self, messages: list[dict]) -> dict[str, list[str]]:
        """Auto-tag all sessions from a list of messages.

        Groups messages by session_id and runs auto_tag_session on each group.

        Args:
            messages: List of all message dicts

        Returns:
            Dict mapping session_id -> list of auto-detected tags
        """
        # Group messages by session
        sessions = defaultdict(list)
        for msg in messages:
            session_id = msg.get("session_id", "")
            if session_id:
                sessions[session_id].append(msg)

        # Auto-tag each session
        auto_tags = {}
        for session_id, session_messages in sessions.items():
            tags = self.auto_tag_session(session_id, session_messages)
            if tags:
                auto_tags[session_id] = tags

        return auto_tags

    def index_project(self, messages: list[dict]):
        """Index (auto-tag) all sessions in a project.

        Runs auto-detection on all messages, updates the stored tags,
        and rebuilds tag metadata.

        Args:
            messages: List of all message dicts for the project
        """
        auto_tags = self.auto_tag_all_sessions(messages)

        data = self._load_tags()
        # Update auto_tags (overwrite for re-indexed sessions)
        data["auto_tags"].update(auto_tags)

        # Rebuild metadata
        data["tag_metadata"] = self._build_tag_metadata()

        self._save_tags(data)

        total_tags = sum(len(tags) for tags in auto_tags.values())
        logger.info(
            f"Auto-tagged {len(auto_tags)} sessions with {total_tags} total tags"
        )

    def get_session_tags(self, session_id: str) -> dict:
        """Get all tags (auto + manual) for a session.

        Args:
            session_id: The session identifier

        Returns:
            Dict with 'auto', 'manual', and 'all' tag lists, plus metadata
        """
        data = self._load_tags()
        auto = data["auto_tags"].get(session_id, [])
        manual = data["manual_tags"].get(session_id, [])
        all_tags = sorted(set(auto + manual))
        metadata = data.get("tag_metadata", {})

        return {
            "session_id": session_id,
            "auto": auto,
            "manual": manual,
            "all": all_tags,
            "metadata": {t: metadata.get(t, {}) for t in all_tags},
        }

    def add_manual_tag(self, session_id: str, tag: str) -> dict:
        """Add a manual tag to a session.

        Args:
            session_id: The session identifier
            tag: The tag name to add

        Returns:
            Updated tags for the session
        """
        tag = tag.strip().lower()
        if not tag:
            return self.get_session_tags(session_id)

        data = self._load_tags()

        if session_id not in data["manual_tags"]:
            data["manual_tags"][session_id] = []

        if tag not in data["manual_tags"][session_id]:
            data["manual_tags"][session_id].append(tag)

        # Add metadata for custom tags if not present
        if tag not in data["tag_metadata"]:
            data["tag_metadata"][tag] = {
                "color": "#667eea",
                "category": "custom",
            }

        self._save_tags(data)
        return self.get_session_tags(session_id)

    def remove_manual_tag(self, session_id: str, tag: str) -> dict:
        """Remove a manual tag from a session.

        Args:
            session_id: The session identifier
            tag: The tag name to remove

        Returns:
            Updated tags for the session
        """
        tag = tag.strip().lower()
        data = self._load_tags()

        if session_id in data["manual_tags"]:
            data["manual_tags"][session_id] = [
                t for t in data["manual_tags"][session_id] if t != tag
            ]
            # Clean up empty lists
            if not data["manual_tags"][session_id]:
                del data["manual_tags"][session_id]

        self._save_tags(data)
        return self.get_session_tags(session_id)

    def get_sessions_by_tag(self, tag: str) -> list[dict]:
        """Get all sessions that have a specific tag.

        Args:
            tag: The tag name to search for

        Returns:
            List of dicts with session_id and tag source (auto/manual/both)
        """
        tag = tag.strip().lower()
        data = self._load_tags()
        results = []

        # Check auto tags
        auto_sessions = set()
        for session_id, tags in data["auto_tags"].items():
            if tag in tags:
                auto_sessions.add(session_id)

        # Check manual tags
        manual_sessions = set()
        for session_id, tags in data["manual_tags"].items():
            if tag in tags:
                manual_sessions.add(session_id)

        # Combine
        all_sessions = auto_sessions | manual_sessions
        for session_id in sorted(all_sessions):
            source = []
            if session_id in auto_sessions:
                source.append("auto")
            if session_id in manual_sessions:
                source.append("manual")
            results.append({
                "session_id": session_id,
                "source": source,
            })

        return results

    def get_tag_cloud(self) -> dict:
        """Get tag cloud data: all tags with counts and metadata.

        Returns:
            Dict with 'tags' list (tag name, count, category, color)
            and 'total_sessions' count
        """
        data = self._load_tags()
        metadata = data.get("tag_metadata", {})

        # Count occurrences across all sessions
        tag_counts = defaultdict(int)
        all_session_ids = set()

        for session_id, tags in data["auto_tags"].items():
            all_session_ids.add(session_id)
            for tag in tags:
                tag_counts[tag] += 1

        for session_id, tags in data["manual_tags"].items():
            all_session_ids.add(session_id)
            for tag in tags:
                tag_counts[tag] += 1

        # Build tag cloud entries
        tags_list = []
        for tag_name, count in sorted(tag_counts.items(), key=lambda x: -x[1]):
            meta = metadata.get(tag_name, {})
            tags_list.append({
                "name": tag_name,
                "count": count,
                "category": meta.get("category", "custom"),
                "color": meta.get("color", "#667eea"),
            })

        return {
            "tags": tags_list,
            "total_sessions": len(all_session_ids),
        }

    def reindex_all(self, memory_cache, cache_service) -> dict:
        """Rebuild auto-tags from all available project data.

        Args:
            memory_cache: The MemoryCache instance
            cache_service: The LocalCacheService instance

        Returns:
            Dict with reindex results
        """
        from ..infra.discovery import project_metadata as get_all_projects_with_metadata
        from ..pipeline import process as _run_pipeline

        projects = get_all_projects_with_metadata()
        total_sessions = 0
        total_tags = 0
        projects_indexed = 0
        errors = []

        # Clear existing auto tags before reindexing
        data = self._load_tags()
        data["auto_tags"] = {}
        data["tag_metadata"] = self._build_tag_metadata()

        for project in projects:
            project_name = project["dir_name"]
            log_path = project["log_path"]

            try:
                # Try memory cache first
                messages = None
                memory_result = (
                    memory_cache.fetch(log_path) if memory_cache else None
                )
                if memory_result:
                    messages, _ = memory_result
                else:
                    # Try file cache
                    cached_messages = (
                        cache_service.load_messages(log_path)
                        if cache_service
                        else None
                    )
                    if cached_messages:
                        messages = cached_messages
                    else:
                        # Process from disk
                        messages, _ = _run_pipeline(log_path)

                if messages:
                    auto_tags = self.auto_tag_all_sessions(messages)
                    data["auto_tags"].update(auto_tags)
                    total_sessions += len(auto_tags)
                    total_tags += sum(
                        len(tags) for tags in auto_tags.values()
                    )
                    projects_indexed += 1

            except Exception as e:
                logger.error(f"Error tagging project {project_name}: {e}")
                errors.append({"project": project_name, "error": str(e)})

        self._save_tags(data)

        return {
            "projects_indexed": projects_indexed,
            "total_sessions_tagged": total_sessions,
            "total_tags_assigned": total_tags,
            "errors": errors,
        }
