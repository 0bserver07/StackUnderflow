"""
Curriculum generation service for StackUnderflow.

Bridges local error/Q&A data with Modal-powered curriculum generation.
"""

import json
import logging
from datetime import datetime
from pathlib import Path

logger = logging.getLogger(__name__)


class CurriculumService:
    """Service for generating personalized learning curriculum from Claude Code usage."""

    def __init__(self, cache_dir: Path | None = None):
        self.cache_dir = cache_dir or Path.home() / ".stackunderflow" / "curriculum"
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self._modal_available = None

    def _check_modal_available(self) -> bool:
        """Check if Modal is available and configured."""
        if self._modal_available is not None:
            return self._modal_available

        try:
            import modal
            # Check if we can look up the function
            modal.Function.from_name("stackunderflow-curriculum", "generate_curriculum")
            self._modal_available = True
        except Exception as e:
            logger.warning(f"Modal not available: {e}")
            self._modal_available = False

        return self._modal_available

    def extract_error_patterns(self, stats: dict) -> list[dict]:
        """Extract error patterns from StackUnderflow statistics."""
        errors = stats.get("errors", {})
        error_list = []

        # Handle different error data structures
        if isinstance(errors, dict):
            by_category = errors.get("by_category", errors)
            for category, data in by_category.items():
                if isinstance(data, dict):
                    count = data.get("count", 0)
                    examples = data.get("examples", [])
                elif isinstance(data, int):
                    count = data
                    examples = []
                else:
                    continue

                if count > 0:
                    error_list.append({
                        "category": category,
                        "count": count,
                        "examples": examples[:5] if examples else []
                    })

        return sorted(error_list, key=lambda x: x["count"], reverse=True)

    def extract_tool_usage(self, stats: dict) -> dict:
        """Extract tool usage with error rates from statistics."""
        tools = stats.get("tools", {})
        tool_data = {}

        for tool_name, data in tools.items():
            if isinstance(data, dict):
                tool_data[tool_name] = {
                    "count": data.get("count", 0),
                    "error_rate": data.get("error_rate", 0),
                    "avg_duration": data.get("avg_duration", 0),
                }
            elif isinstance(data, int):
                tool_data[tool_name] = {
                    "count": data,
                    "error_rate": 0,
                    "avg_duration": 0,
                }

        return tool_data

    def generate_curriculum(
        self,
        stats: dict,
        qa_pairs: list[dict],
        focus_area: str | None = None,
        difficulty: str | None = None,
        use_cache: bool = True,
    ) -> dict:
        """
        Generate personalized curriculum from user's Claude Code usage.

        Args:
            stats: Statistics dict from StackUnderflow
            qa_pairs: Q&A pairs from qa_service
            focus_area: Optional focus (e.g., "file operations", "bash")
            difficulty: "beginner", "intermediate", "advanced"
            use_cache: Whether to use cached curriculum

        Returns:
            Curriculum dict with lessons, exercises, recommendations
        """
        # Create cache key
        cache_key = self._create_cache_key(stats, focus_area, difficulty)
        cache_file = self.cache_dir / f"{cache_key}.json"

        # Check cache
        if use_cache and cache_file.exists():
            try:
                with open(cache_file) as f:
                    cached = json.load(f)
                    # Cache valid for 24 hours
                    if cached.get("generated_at"):
                        cached_time = datetime.fromisoformat(cached["generated_at"])
                        if (datetime.now() - cached_time).total_seconds() < 86400:
                            logger.info(f"Using cached curriculum: {cache_key}")
                            return cached
            except Exception as e:
                logger.warning(f"Cache read error: {e}")

        # Extract data for Modal
        error_patterns = self.extract_error_patterns(stats)
        tool_usage = self.extract_tool_usage(stats)

        if not error_patterns and not qa_pairs:
            return {
                "error": "No error patterns or Q&A history found",
                "suggestion": "Use Claude Code more to generate learning data"
            }

        # Check if Modal is available
        if not self._check_modal_available():
            return self._generate_local_curriculum(
                error_patterns, qa_pairs, tool_usage, focus_area, difficulty
            )

        # Call Modal function
        try:
            import modal
            generate_fn = modal.Function.from_name(
                "stackunderflow-curriculum",
                "generate_curriculum"
            )

            curriculum = generate_fn.remote(
                error_patterns=error_patterns,
                qa_pairs=qa_pairs,
                tool_usage=tool_usage,
                focus_area=focus_area,
                difficulty_preference=difficulty,
            )

            # Add metadata
            curriculum["generated_at"] = datetime.now().isoformat()
            curriculum["source"] = "modal"

            # Cache result
            try:
                with open(cache_file, "w") as f:
                    json.dump(curriculum, f, indent=2)
            except Exception as e:
                logger.warning(f"Cache write error: {e}")

            return curriculum

        except Exception as e:
            logger.error(f"Modal curriculum generation failed: {e}")
            return self._generate_local_curriculum(
                error_patterns, qa_pairs, tool_usage, focus_area, difficulty
            )

    def _generate_local_curriculum(
        self,
        error_patterns: list[dict],
        qa_pairs: list[dict],
        tool_usage: dict,
        focus_area: str | None,
        difficulty: str | None,
    ) -> dict:
        """
        Generate a basic curriculum locally (fallback when Modal unavailable).

        This is a simpler rule-based approach without AI.
        """
        lessons = []

        # Group errors by severity
        high_frequency = [e for e in error_patterns if e["count"] >= 10]


        # Generate lessons from high-frequency errors
        for i, error in enumerate(high_frequency[:3]):
            lessons.append({
                "id": f"lesson-{i+1}",
                "title": f"Avoiding {error['category']} Errors",
                "topic": self._categorize_error(error["category"]),
                "difficulty": "beginner" if i == 0 else "intermediate",
                "why_needed": f"You've encountered this error {error['count']} times",
                "concepts": self._get_concepts_for_error(error["category"]),
                "exercises": [{
                    "type": "practice",
                    "prompt": f"Practice handling {error['category']} scenarios",
                    "hints": self._get_hints_for_error(error["category"])
                }],
                "estimated_minutes": 15
            })

        # Add tool-based lessons for high error rates
        high_error_tools = [
            (tool, data) for tool, data in tool_usage.items()
            if data.get("error_rate", 0) > 0.1 and data.get("count", 0) > 5
        ]

        for tool, data in high_error_tools[:2]:
            lessons.append({
                "id": f"lesson-tool-{tool.lower()}",
                "title": f"Mastering the {tool} Tool",
                "topic": "Tool Usage",
                "difficulty": "intermediate",
                "why_needed": f"{data['error_rate']:.0%} error rate with {data['count']} uses",
                "concepts": [f"{tool} best practices", "Error handling", "Common pitfalls"],
                "exercises": [{
                    "type": "practice",
                    "prompt": f"Practice using {tool} effectively",
                    "hints": [f"Check {tool} documentation", "Handle edge cases"]
                }],
                "estimated_minutes": 20
            })

        return {
            "summary": f"Based on {len(error_patterns)} error types and {len(qa_pairs)} Q&A pairs",
            "skill_gaps": [e["category"] for e in high_frequency[:3]],
            "lessons": lessons,
            "recommended_order": [lesson["id"] for lesson in lessons],
            "quick_wins": [
                f"Review {error_patterns[0]['category']} handling" if error_patterns else "Keep coding!",
                "Check file paths before operations",
                "Use try-except for external commands"
            ],
            "generated_at": datetime.now().isoformat(),
            "source": "local",
            "note": "AI-powered curriculum generation requires Modal deployment (see docs)"
        }

    def _create_cache_key(self, stats: dict, focus: str | None, difficulty: str | None) -> str:
        """Create a cache key from inputs."""
        import hashlib
        # Use error counts as part of key (changes when new errors occur)
        error_sig = str(sorted([
            (k, v.get("count", v) if isinstance(v, dict) else v)
            for k, v in stats.get("errors", {}).get("by_category", stats.get("errors", {})).items()
        ][:5]))
        key_parts = [error_sig, str(focus), str(difficulty)]
        return hashlib.md5("|".join(key_parts).encode()).hexdigest()[:12]

    def _categorize_error(self, error_category: str) -> str:
        """Map error category to topic."""
        file_errors = ["File Not Found", "File Not Read", "File Too Large", "Permission Error"]
        bash_errors = ["Command Timeout", "Tool Not Found", "Code Runtime Error"]
        syntax_errors = ["Syntax Error"]

        if error_category in file_errors:
            return "File Operations"
        elif error_category in bash_errors:
            return "Command Execution"
        elif error_category in syntax_errors:
            return "Code Quality"
        else:
            return "General"

    def _get_concepts_for_error(self, error_category: str) -> list[str]:
        """Get relevant concepts for an error type."""
        concepts = {
            "File Not Found": ["Path validation", "Working directories", "Relative vs absolute paths"],
            "File Not Read": ["File encoding", "Binary vs text", "File permissions"],
            "Permission Error": ["Unix permissions", "sudo usage", "File ownership"],
            "Syntax Error": ["Python syntax", "Indentation", "String formatting"],
            "Command Timeout": ["Process management", "Async execution", "Timeout handling"],
            "Code Runtime Error": ["Exception handling", "Debugging", "Stack traces"],
        }
        return concepts.get(error_category, ["Error handling", "Best practices"])

    def _get_hints_for_error(self, error_category: str) -> list[str]:
        """Get hints for avoiding an error type."""
        hints = {
            "File Not Found": ["Use os.path.exists() to check first", "Print the full path for debugging"],
            "Permission Error": ["Check file permissions with ls -la", "Consider if sudo is needed"],
            "Syntax Error": ["Check indentation consistency", "Look for missing colons or brackets"],
            "Command Timeout": ["Set appropriate timeout values", "Use background execution for long tasks"],
        }
        return hints.get(error_category, ["Read the error message carefully", "Check the documentation"])

    def generate_exercise_for_error(
        self,
        error_category: str,
        error_examples: list[str],
    ) -> dict:
        """Generate a focused exercise for a specific error type."""
        if not self._check_modal_available():
            return {
                "error_type": error_category,
                "explanation": f"Common causes of {error_category}",
                "exercise": {
                    "scenario": f"You encounter a {error_category}",
                    "task": "Debug and fix the issue",
                    "expected_approach": "Check the error message and trace",
                    "common_mistakes": ["Ignoring the error", "Not reading the full message"]
                },
                "quick_reference": "Read error messages carefully",
                "source": "local"
            }

        try:
            import modal
            generate_fn = modal.Function.from_name(
                "stackunderflow-curriculum",
                "generate_exercise_for_error"
            )

            return generate_fn.remote(
                error_category=error_category,
                error_examples=error_examples,
            )
        except Exception as e:
            logger.error(f"Modal exercise generation failed: {e}")
            return {
                "error_type": error_category,
                "error": str(e),
                "source": "local"
            }


# Singleton instance
_curriculum_service: CurriculumService | None = None


def get_curriculum_service() -> CurriculumService:
    """Get the curriculum service singleton."""
    global _curriculum_service
    if _curriculum_service is None:
        _curriculum_service = CurriculumService()
    return _curriculum_service
