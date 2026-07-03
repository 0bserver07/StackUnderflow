"""Egress leak-scan — proof that structured outbound request bodies are
shape-bounded and don't smuggle un-reviewed content across the (now
cloud-capable) Ollama network boundary.

Since commit ``afb07b5`` the embeddings backend, the watcher, and the
``meta_agent`` chat can POST to a REMOTE endpoint. These tests drive the two
outbound body builders — ``embeddings._embed_one`` and
``meta_agent.build_chat_request`` — with a corpus of RFC-reserved SYNTHETIC
secrets (``tests/fixtures/egress-corpus/``) and assert:

* the serialized body carries ONLY allowlisted top-level keys
  (``egress.guard_json_body`` fails closed on anything else — the property
  allowlist test);
* a hosted-endpoint bearer credential never lands in the body (it is a header);
* text that legitimately MUST cross (the prompt you asked to embed; the
  conversation you asked the agent to read) is present — an EXPLICIT, reviewed
  allowance, asserted here so it can never become a silent leak;
* a negative control proves the leak-scan is NOT vacuous: a planted secret in a
  disallowed slot IS detected, so the green ``== []`` assertions mean something.

Offline: no network. The one HTTP call site (``httpx.post``) is monkeypatched
to capture the outbound body. Every secret-shaped string comes from the corpus
fixtures (never a literal in this file) so the credential-scanning linter has
nothing to flag.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from stackunderflow.infra import egress
from stackunderflow.services import embeddings as emb
from stackunderflow.services import meta_agent

# ── synthetic-secret corpus ───────────────────────────────────────────────────

_CORPUS_ROOT = Path(__file__).resolve().parents[2] / "fixtures" / "egress-corpus"


def _load(name: str) -> list[str]:
    """Read one corpus file → its non-comment, non-blank lines."""
    out: list[str] = []
    for raw in (_CORPUS_ROOT / name).read_text(encoding="utf-8").splitlines():
        s = raw.strip()
        if s and not s.startswith("#"):
            out.append(s)
    return out


@pytest.fixture(scope="module")
def corpus() -> dict[str, list[str]]:
    c = {
        "api_keys": _load("api_keys.txt"),
        "emails": _load("emails.txt"),
        "ips": _load("ips.txt"),
        "paths": _load("paths.txt"),
    }
    # Guard against a missing/empty fixture silently turning every leak-scan
    # below into a vacuous pass.
    for kind, vals in c.items():
        assert vals, f"egress corpus '{kind}' is empty — fixture missing?"
    return c


def _all_secrets(corpus: dict[str, list[str]]) -> list[str]:
    return [s for vals in corpus.values() for s in vals]


# ── httpx capture (no network) ────────────────────────────────────────────────


class _FakeResponse:
    status_code = 200

    def json(self) -> dict[str, Any]:
        return {"embedding": [0.1, 0.2, 0.3]}


class _FakePost:
    """Stand-in for ``httpx.post`` that records the outbound body + headers."""

    def __init__(self) -> None:
        self.body: dict[str, Any] | None = None
        self.headers: dict[str, str] | None = None

    def __call__(self, url: str, *, json: dict[str, Any], headers: dict[str, str], timeout: float) -> _FakeResponse:
        self.body = json
        self.headers = headers
        return _FakeResponse()


def _guard_spy(monkeypatch: pytest.MonkeyPatch, module: Any) -> list[tuple[set[str], str]]:
    """Patch the single chokepoint on ``module.egress`` and record every call.

    Proves the outbound builder actually routes through ``guard_json_body``
    (i.e. the chokepoint is on the path, not bypassed) and captures the
    allowlist + kind each call used. The real guard still runs.
    """
    seen: list[tuple[set[str], str]] = []
    real_guard = egress.guard_json_body

    def spy(body: Any, *, allow: Any, kind: str) -> dict[str, Any]:
        seen.append((set(allow), kind))
        return real_guard(body, allow=allow, kind=kind)

    monkeypatch.setattr(module.egress, "guard_json_body", spy)
    return seen


# ── property allowlist: a stray key is rejected ───────────────────────────────


class TestAllowlistProperty:
    """Inject a stray top-level key into a structured payload → rejected."""

    def test_stray_key_in_embed_body_is_rejected(self, corpus: dict[str, list[str]]) -> None:
        secret = corpus["api_keys"][0]
        with pytest.raises(egress.EgressViolation) as ei:
            egress.guard_json_body(
                {"model": "nomic-embed-text", "prompt": "hello", "x_context": secret},
                allow=egress.OLLAMA_EMBED_KEYS,
                kind="ollama/embeddings",
            )
        msg = str(ei.value)
        assert "x_context" in msg  # names the offending key
        assert secret not in msg  # but never echoes the secret value into logs

    def test_stray_key_in_chat_body_is_rejected(self, corpus: dict[str, list[str]]) -> None:
        secret = corpus["paths"][0]
        with pytest.raises(egress.EgressViolation) as ei:
            egress.guard_json_body(
                {"model": "qwen2.5-coder", "messages": [], "env": secret},
                allow=egress.OLLAMA_CHAT_KEYS,
                kind="ollama/chat",
            )
        assert "env" in str(ei.value)
        assert secret not in str(ei.value)

    def test_valid_body_passes_through_as_a_copy(self) -> None:
        body = {"model": "m", "prompt": "hi"}
        out = egress.guard_json_body(body, allow=egress.OLLAMA_EMBED_KEYS, kind="ollama/embeddings")
        assert out == body
        assert out is not body  # returns a copy, never aliases the caller's dict


# ── embeddings: the hot outbound path ─────────────────────────────────────────


class TestEmbeddingsOutboundBody:
    def test_body_is_shape_bounded_and_routes_through_the_chokepoint(
        self, monkeypatch: pytest.MonkeyPatch, corpus: dict[str, list[str]]
    ) -> None:
        fake = _FakePost()
        monkeypatch.setattr(emb.httpx, "post", fake)
        seen = _guard_spy(monkeypatch, emb)

        bearer = corpus["api_keys"][1]  # a hosted-endpoint bearer credential
        vec = emb._embed_one(
            "benign prompt text",
            model="nomic-embed-text",
            base="https://ollama.example.invalid",
            api_key=bearer,
        )
        assert vec == [0.1, 0.2, 0.3]  # fake response flowed back → path exercised

        # (1) shape-bounded: exactly the allowlisted keys, nothing else.
        assert set(fake.body or {}) == {"model", "prompt"}
        # (2) it went through the single egress chokepoint, with the embed allowlist.
        assert seen == [({"model", "prompt"}, "ollama/embeddings")]
        # (3) the bearer credential is header-only — it must NOT be in the body.
        assert egress.scan(egress.serialize(fake.body), [bearer]) == []
        assert bearer in (fake.headers or {}).get("Authorization", "")

    def test_no_corpus_credential_ever_reaches_the_body(
        self, monkeypatch: pytest.MonkeyPatch, corpus: dict[str, list[str]]
    ) -> None:
        # Sweep every credential-shaped fixture through the api_key arg and
        # confirm none of them land in the serialized body — the credential's
        # only sanctioned exit is the Authorization header.
        for bearer in corpus["api_keys"]:
            fake = _FakePost()
            monkeypatch.setattr(emb.httpx, "post", fake)
            emb._embed_one("some text", model="m", base="https://x.example.invalid", api_key=bearer)
            assert egress.scan(egress.serialize(fake.body), [bearer]) == []

    def test_prompt_text_crossing_is_an_explicit_reviewed_allowance(
        self, monkeypatch: pytest.MonkeyPatch, corpus: dict[str, list[str]]
    ) -> None:
        # EXPLICIT ALLOWANCE — NOT A LEAK. Embeddings cannot embed text without
        # sending it, so the prompt is the one payload we knowingly let cross
        # the boundary. We assert it IS present (and that it is the ONLY corpus
        # string in the body) so this crossing is documented and reviewed here,
        # never silent. If embeddings ever sent MORE than the prompt, the shape
        # test above and the `found == [secret_text]` check below would catch it.
        fake = _FakePost()
        monkeypatch.setattr(emb.httpx, "post", fake)
        secret_text = corpus["paths"][0]  # transcript-like content w/ a fake path
        emb._embed_one(secret_text, model="m", base="https://x.example.invalid")
        assert (fake.body or {})["prompt"] == secret_text
        found = egress.scan(egress.serialize(fake.body), _all_secrets(corpus))
        assert found == [secret_text]


# ── meta-agent chat: the largest egress surface ───────────────────────────────


class TestChatOutboundBody:
    def test_build_chat_request_is_shape_bounded_and_routes_through_the_chokepoint(
        self, monkeypatch: pytest.MonkeyPatch, corpus: dict[str, list[str]]
    ) -> None:
        seen = _guard_spy(monkeypatch, meta_agent)

        # messages carry the user's own conversation + a tool result (store data
        # they asked the agent to read) — content that legitimately crosses.
        # Lace them with corpus secrets to model a real transcript.
        messages = [
            {"role": "system", "content": "You are the StackUnderflow meta-agent."},
            {"role": "user", "content": f"what happened in {corpus['paths'][0]}?"},
            {
                "role": "tool",
                "name": "get_file_risk",
                "content": f'{{"path": "{corpus["paths"][1]}", "contact": "{corpus["emails"][0]}"}}',
            },
        ]
        req = meta_agent.build_chat_request(model="qwen2.5-coder", messages=messages, tools_enabled=True)

        # shape-bounded to the chat allowlist; tools attached; nothing extra.
        assert set(req).issubset(egress.OLLAMA_CHAT_KEYS)
        assert set(req) == {"model", "messages", "stream", "tools"}
        assert req["tools"] is meta_agent.TOOL_CATALOG
        # routed through the SAME single chokepoint, with the chat allowlist.
        assert seen == [(set(egress.OLLAMA_CHAT_KEYS), "ollama/chat")]

    def test_builder_introduces_no_credential_only_the_passed_content(
        self, corpus: dict[str, list[str]]
    ) -> None:
        # ALLOWANCE — a chat turn necessarily sends the conversation to the LLM.
        # The bearer credential is NOT part of the body (it's a header in the
        # route); assert the builder never introduces one and never adds a field
        # beyond what we handed it. Only the message content we passed appears.
        path = corpus["paths"][0]
        messages = [{"role": "user", "content": f"look at {path}"}]
        req = meta_agent.build_chat_request(model="m", messages=messages, tools_enabled=False)
        assert set(req) == {"model", "messages", "stream"}  # no tools, no extras
        body_str = egress.serialize(req)
        assert egress.scan(body_str, corpus["api_keys"]) == []  # no credential smuggled in
        assert path in body_str  # the content we chose to send is present (allowed)


# ── negative control: the scan is not vacuous ─────────────────────────────────


class TestNegativeControl:
    """If a secret DID cross in a disallowed slot, the guard + scan must catch
    it. If these fail, every green ``== []`` above is meaningless."""

    def test_scan_detects_a_planted_secret(self, corpus: dict[str, list[str]]) -> None:
        secret = corpus["api_keys"][0]
        leaky_body = {"model": "m", "prompt": "hi", "smuggled_credential": secret}
        assert egress.scan(egress.serialize(leaky_body), [secret]) == [secret]

    def test_guard_rejects_the_planted_body_before_the_wire(self, corpus: dict[str, list[str]]) -> None:
        secret = corpus["api_keys"][0]
        with pytest.raises(egress.EgressViolation):
            egress.guard_json_body(
                {"model": "m", "prompt": "hi", "smuggled_credential": secret},
                allow=egress.OLLAMA_EMBED_KEYS,
                kind="ollama/embeddings",
            )

    def test_a_regressed_embed_body_would_leak_the_bearer(self, corpus: dict[str, list[str]]) -> None:
        # Models the exact regression the shape test guards against: a future
        # edit that put the bearer into the body (`json={..., "api_key": key}`).
        # Prove both that the scan flags it AND the guard refuses it, so this
        # failure mode cannot slip past as a silent green.
        bearer = corpus["api_keys"][1]
        regressed_body = {"model": "m", "prompt": "hi", "api_key": bearer}
        assert egress.scan(egress.serialize(regressed_body), [bearer]) == [bearer]
        with pytest.raises(egress.EgressViolation):
            egress.guard_json_body(regressed_body, allow=egress.OLLAMA_EMBED_KEYS, kind="ollama/embeddings")
