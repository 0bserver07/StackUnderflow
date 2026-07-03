"""Unit tests for the canonical ``services.task_classifier`` (spec 26 Move 0).

Covers the single-vs-multi intent split, the adopted 6th label (``ops``), size
banding, language detection, and — the point of Move 0 — that the tag service
and the mode recommender now agree with the canonical classifier (no drift).
"""

from __future__ import annotations

from stackunderflow.services import task_classifier as tc


class TestIntentSingle:
    def test_fix_beats_test_on_fix_the_test(self):
        # The historical mode_recommender tiebreak: "fix" outranks "test".
        assert tc.classify_intent("fix the failing test in tests/foo.py") == "fix"

    def test_build(self):
        assert tc.classify_intent("implement a new component") == "build"

    def test_refactor(self):
        assert tc.classify_intent("refactor and rename the helper") == "refactor"

    def test_test(self):
        assert tc.classify_intent("write unit tests, target 90% coverage") == "test"

    def test_explore_default(self):
        assert tc.classify_intent("what does the cache module do") == "explore"
        assert tc.classify_intent("") == "explore"
        assert tc.classify_intent("hello world") == "explore"

    def test_ops_is_adopted(self):
        assert tc.classify_intent("deploy this to staging") == "ops"
        assert tc.classify_intent("why is the docker build hanging") == "ops"
        assert tc.classify_intent("update the .env and restart nginx") == "ops"

    def test_ops_present_in_multi_even_when_fix_wins_single(self):
        # "breaks" triggers fix (higher single-pick priority), but ops is still
        # in the multi-label set — the tag service surfaces both.
        text = "missing value in .env breaks auth"
        assert tc.classify_intent(text) == "fix"
        assert "ops" in tc.classify_intents(text)


class TestIntentMulti:
    def test_multi_intent_set(self):
        got = tc.classify_intents("Add a /users endpoint, then fix the crash")
        assert "build" in got and "fix" in got

    def test_empty(self):
        assert tc.classify_intents("") == set()
        assert tc.classify_intents("hello world") == set()

    def test_six_labels_available(self):
        assert set(tc.INTENT_LABELS) == {
            "build", "fix", "explore", "refactor", "test", "ops",
        }


class TestSizeBand:
    def test_band_for_token_count(self):
        assert tc.band_for_token_count(0) == "tiny"
        assert tc.band_for_token_count(199) == "tiny"
        assert tc.band_for_token_count(500) == "small"
        assert tc.band_for_token_count(1500) == "med"
        assert tc.band_for_token_count(5000) == "large"

    def test_token_band_from_text(self):
        assert tc.token_band("a" * 100) == "tiny"     # ~25 tok
        assert tc.token_band("a" * 5000) == "med"     # ~1250 tok


class TestLanguage:
    def test_dominant_language(self):
        assert tc.dominant_language("fix the bug in foo.py with pytest") == "python"
        assert tc.dominant_language("no language here") is None

    def test_detect_languages_sorted(self):
        langs = tc.detect_languages("convert foo.py to foo.ts")
        assert langs == sorted(langs)
        assert "python" in langs and "typescript" in langs


class TestClassifyTask:
    def test_shape(self):
        out = tc.classify_task("fix the failing test in foo.py")
        assert out == {"intent": "fix", "size_band": "tiny", "language": "python"}

    def test_deterministic(self):
        a = tc.classify_task("refactor the parser module")
        b = tc.classify_task("refactor the parser module")
        assert a == b


class TestReconciliation:
    """Move 0's contract: both callers now derive from the canonical classifier."""

    def test_mode_recommender_intent_delegates(self):
        from stackunderflow.services import mode_recommender as mr

        for prompt in [
            "fix the failing test in foo.py",
            "implement a new feature",
            "deploy the docker container",  # ops — the newly-unified label
            "what does this do",
        ]:
            assert mr._intent_of(prompt) == tc.classify_intent(prompt)

    def test_tag_service_intents_delegate_with_prefix(self):
        from stackunderflow.services.tag_service import TagService

        text = "Add a /users endpoint, then fix the crash, and deploy it"
        detected = TagService._detect_intents(text)
        expected = {f"intent:{label}" for label in tc.classify_intents(text)}
        assert detected == expected
        assert "intent:ops" in detected  # deploy → ops, shared taxonomy
