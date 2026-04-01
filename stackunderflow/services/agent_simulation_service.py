"""
Agent simulation service for StackUnderflow social layer.

Implements the perceive-decide-act loop where AI agents analyze Q&A pairs
and post threaded commentary. Each agent has a persona and makes decisions
about whether to POST, LIKE, or DO_NOTHING in each round.

Supports multiple LLM providers (Groq, OpenRouter) via OpenAI-compatible APIs.
"""

import asyncio
import json
import logging
import os
import random
import re
from datetime import UTC, datetime

import httpx

logger = logging.getLogger(__name__)

# ── Provider configs ───────────────────────────────────────────────────────

PROVIDERS = {
    "groq": {
        "url": "https://api.groq.com/openai/v1/chat/completions",
        "env_key": "GROQ_API_KEY",
        "headers": {},
        "supports_json_mode": True,
    },
    "openrouter": {
        "url": "https://openrouter.ai/api/v1/chat/completions",
        "env_key": "OPENROUTER_API_KEY",
        "headers": {
            "HTTP-Referer": "http://localhost:8081",
            "X-Title": "StackUnderflow",
        },
        "supports_json_mode": False,  # free models often don't support it
    },
}

# Each agent gets a provider + model pairing for diverse perspectives
AGENT_LLM_CONFIG = {
    "security-reviewer": {
        "provider": "groq",
        "model": "llama-3.3-70b-versatile",
    },
    "architecture-expert": {
        "provider": "openrouter",
        "model": "qwen/qwen3-next-80b-a3b-instruct:free",
    },
    "performance-optimizer": {
        "provider": "groq",
        "model": "qwen/qwen3-32b",
    },
    "code-mentor": {
        "provider": "openrouter",
        "model": "deepseek/deepseek-r1-0528:free",
    },
    "devils-advocate": {
        "provider": "openrouter",
        "model": "sourceful/riverflow-v2-pro",
    },
}

FALLBACK_PROVIDER = "groq"
FALLBACK_MODEL = "llama-3.3-70b-versatile"


class AgentSimulationService:
    """Service for running agent discussion simulations on Q&A pairs."""

    def __init__(self):
        self._running_tasks: dict[str, asyncio.Task] = {}

    async def trigger_discussion(
        self,
        qa_id: str,
        social_service,
        qa_service,
        agent_ids: list[str] | None = None,
        max_rounds: int = 3,
    ) -> dict:
        """Trigger an agent discussion on a Q&A pair."""
        agents = social_service.list_agents()
        if agent_ids:
            agents = [a for a in agents if a['id'] in agent_ids]
        agents = [a for a in agents if a['status'] == 'active']

        if not agents:
            raise ValueError("No active agents available for discussion")

        qa = qa_service.get_qa_by_id(qa_id)
        if not qa:
            raise ValueError(f"Q&A pair not found: {qa_id}")

        run = social_service.create_agent_run(
            qa_id=qa_id,
            agent_ids=[a['id'] for a in agents],
        )
        run_id = run['id']

        total_steps = len(agents) * max_rounds
        social_service.update_agent_run(run_id, total_steps=total_steps)

        task = asyncio.create_task(
            self._run_discussion(run_id, qa_id, qa, agents, social_service, max_rounds)
        )
        self._running_tasks[run_id] = task
        task.add_done_callback(lambda t: self._running_tasks.pop(run_id, None))

        return social_service.get_agent_run(run_id)

    def cancel_run(self, run_id: str, social_service) -> bool:
        """Cancel a running simulation."""
        task = self._running_tasks.get(run_id)
        if task and not task.done():
            task.cancel()
            social_service.update_agent_run(
                run_id,
                status='failed',
                error='Cancelled by user',
                completed_at=datetime.now(UTC).isoformat(),
            )
            return True
        return False

    async def _run_discussion(
        self,
        run_id: str,
        qa_id: str,
        qa: dict,
        agents: list[dict],
        social_service,
        max_rounds: int,
    ):
        """Main discussion loop — runs as background task."""
        try:
            social_service.update_agent_run(run_id, status='running')
            completed_steps = 0

            for round_num in range(max_rounds):
                all_did_nothing = True
                random.shuffle(agents)

                for agent in agents:
                    try:
                        tree = social_service.get_discussion_tree(qa_id)
                        decision = await self._get_agent_decision(agent, qa, tree)

                        if decision is None:
                            completed_steps += 1
                            social_service.update_agent_run(run_id, completed_steps=completed_steps)
                            continue

                        action = decision.get('action', 'DO_NOTHING')

                        if action == 'POST':
                            content = decision.get('content', '').strip()
                            reply_to = decision.get('reply_to')
                            if content:
                                social_service.create_discussion(
                                    qa_id=qa_id,
                                    author_type='agent',
                                    author_id=agent['id'],
                                    content=content,
                                    parent_id=reply_to,
                                )
                                all_did_nothing = False

                        elif action == 'LIKE':
                            like_post_id = decision.get('like_post_id', '').strip()
                            if like_post_id:
                                social_service.toggle_vote(
                                    target_type='discussion',
                                    target_id=like_post_id,
                                    voter_type='agent',
                                    voter_id=agent['id'],
                                )
                                all_did_nothing = False

                        updated_memory = decision.get('updated_memory', '')
                        if updated_memory:
                            social_service.update_agent(agent['id'], {'memory': updated_memory})
                            agent['memory'] = updated_memory

                    except asyncio.CancelledError:
                        raise
                    except Exception as e:
                        logger.error(f"Agent {agent['id']} error in round {round_num}: {e}")

                    completed_steps += 1
                    social_service.update_agent_run(run_id, completed_steps=completed_steps)

                if all_did_nothing:
                    logger.info(f"All agents chose DO_NOTHING in round {round_num}, stopping early")
                    break

            social_service.update_agent_run(
                run_id,
                status='completed',
                completed_steps=completed_steps,
                completed_at=datetime.now(UTC).isoformat(),
            )

        except asyncio.CancelledError:
            logger.info(f"Simulation {run_id} cancelled")
        except Exception as e:
            logger.error(f"Simulation {run_id} failed: {e}")
            social_service.update_agent_run(
                run_id,
                status='failed',
                error=str(e),
                completed_at=datetime.now(UTC).isoformat(),
            )

    async def _get_agent_decision(self, agent: dict, qa: dict, tree: dict) -> dict | None:
        """Get an agent's decision by calling the LLM."""
        thread_text = self._format_thread_for_prompt(tree.get('posts', []))

        code_snippets = qa.get('code_snippets', [])
        code_text = '\n\n'.join(code_snippets[:3]) if code_snippets else '(no code snippets)'

        system_message = (
            f"You are {agent['name']}, a {agent['role']}. {agent['system_prompt']}\n\n"
            "Your task is to review a Q&A pair from a coding session and participate "
            "in a discussion about it.\n\n"
            "RULES:\n"
            "- Be concise but insightful (2-4 paragraphs max for posts)\n"
            "- Use markdown formatting for code references\n"
            "- If you have nothing meaningful to add, choose DO_NOTHING\n"
            "- Only LIKE posts you genuinely find valuable\n"
            "- When replying, reference the specific post you're responding to\n"
            "- Don't repeat points already made by others\n"
            "- Your personality should come through in your writing style"
        )

        user_message = (
            f"Your observations so far: {agent.get('memory', 'None yet')}\n\n"
            "--- Q&A Under Review ---\n"
            f"Question: {qa.get('question_text', '')}\n\n"
            f"Answer: {qa.get('answer_text', '')[:3000]}\n\n"
            "Code Snippets:\n"
            f"{code_text}\n\n"
            "--- Discussion So Far ---\n"
            f"{thread_text if thread_text else '(No discussion yet - you can be the first to comment!)'}\n\n"
            "---\n"
            "Based on your expertise, decide what to do. Respond with ONLY a JSON object, no other text:\n\n"
            "If posting a new comment or reply:\n"
            '{"action": "POST", "content": "your comment in markdown", '
            '"reply_to": null, "updated_memory": "brief notes about what you observed"}\n\n'
            "If liking someone's post:\n"
            '{"action": "LIKE", "like_post_id": "id_of_post_to_like", '
            '"updated_memory": "brief notes"}\n\n'
            "If you have nothing to add:\n"
            '{"action": "DO_NOTHING", "updated_memory": "brief notes"}\n\n'
            "JSON ONLY — no markdown fences, no explanation, just the raw JSON object."
        )

        messages = [
            {"role": "system", "content": system_message},
            {"role": "user", "content": user_message},
        ]

        # Resolve provider + model for this agent
        llm_config = AGENT_LLM_CONFIG.get(agent['id'], {})
        provider_name = llm_config.get('provider', FALLBACK_PROVIDER)
        model = agent.get('ollama_model') or llm_config.get('model', FALLBACK_MODEL)

        try:
            response = await self._call_llm(provider_name, model, messages)
            if response:
                return response
        except Exception as e:
            logger.error(f"LLM call failed for agent {agent['id']} ({provider_name}/{model}): {e}")
            # Try fallback if not already on it
            if provider_name != FALLBACK_PROVIDER or model != FALLBACK_MODEL:
                logger.info(f"Retrying agent {agent['id']} with fallback {FALLBACK_PROVIDER}/{FALLBACK_MODEL}")
                try:
                    response = await self._call_llm(FALLBACK_PROVIDER, FALLBACK_MODEL, messages)
                    if response:
                        return response
                except Exception as e2:
                    logger.error(f"Fallback also failed for agent {agent['id']}: {e2}")

        return None

    async def _call_llm(
        self, provider_name: str, model: str, messages: list[dict]
    ) -> dict | None:
        """Call an OpenAI-compatible LLM API and parse JSON response."""
        provider = PROVIDERS.get(provider_name)
        if not provider:
            raise ValueError(f"Unknown provider: {provider_name}")

        url = provider["url"]
        api_key = os.getenv(provider["env_key"], "")
        extra_headers = provider.get("headers", {})
        supports_json_mode = provider.get("supports_json_mode", False)

        headers = {
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            **extra_headers,
        }

        body: dict = {
            "model": model,
            "messages": messages,
            "temperature": 0.7,
            "max_tokens": 1024,
        }
        if supports_json_mode:
            body["response_format"] = {"type": "json_object"}

        try:
            async with httpx.AsyncClient(timeout=120.0) as client:
                response = await client.post(url, headers=headers, json=body)
                response.raise_for_status()

                result = response.json()
                content = result["choices"][0]["message"]["content"]

                # Strip <think>...</think> blocks (deepseek-r1 reasoning traces)
                content = re.sub(r'<think>.*?</think>', '', content, flags=re.DOTALL).strip()

                # Strip markdown code fences if model wrapped JSON in them
                content = re.sub(r'^```(?:json)?\s*', '', content)
                content = re.sub(r'\s*```$', '', content)
                content = content.strip()

                try:
                    return json.loads(content)
                except json.JSONDecodeError:
                    # Try to find a JSON object in the response
                    json_match = re.search(r'\{[^{}]*"action"\s*:\s*"[^"]+?".*?\}', content, re.DOTALL)
                    if json_match:
                        try:
                            return json.loads(json_match.group())
                        except json.JSONDecodeError:
                            pass
                    # Broader attempt
                    json_match = re.search(r'\{.*\}', content, re.DOTALL)
                    if json_match:
                        try:
                            return json.loads(json_match.group())
                        except json.JSONDecodeError:
                            pass
                    logger.warning(
                        f"Failed to parse JSON from {provider_name}/{model}: {content[:300]}"
                    )
                    return None

        except httpx.ConnectError:
            logger.error(f"Cannot connect to {provider_name} at {url}")
            raise ValueError(f"Cannot reach {provider_name} API.")
        except httpx.HTTPStatusError as e:
            body_text = e.response.text[:500] if e.response else ""
            logger.error(f"{provider_name} API error {e.response.status_code}: {body_text}")
            raise
        except Exception as e:
            logger.error(f"{provider_name} call error: {e}")
            raise

    def _format_thread_for_prompt(self, posts: list[dict], indent: int = 0) -> str:
        """Format discussion thread for inclusion in agent prompt."""
        if not posts:
            return ""

        lines = []
        for post in posts:
            prefix = "  " * indent
            author_name = post.get('author_name', post.get('author_id', 'Unknown'))
            author_role = post.get('author_role', '')
            role_str = f" ({author_role})" if author_role else ""
            votes = post.get('vote_count', 0)
            post_id = post.get('id', '')

            lines.append(f"{prefix}[{author_name}{role_str}] (id: {post_id}, votes: {votes}):")
            for content_line in post.get('content', '').split('\n'):
                lines.append(f"{prefix}  {content_line}")
            lines.append("")

            children = post.get('children', [])
            if children:
                lines.append(self._format_thread_for_prompt(children, indent + 1))

        return "\n".join(lines)
