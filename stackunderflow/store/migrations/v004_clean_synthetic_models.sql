-- v004: clear the literal ``"<synthetic>"`` model id from existing rows.
--
-- Background: Claude Code stamps ``message.model = "<synthetic>"`` on
-- locally generated placeholder records — API errors (rate-limit hits,
-- ECONNRESET, auth failures), invalid-request stubs, and the
-- "No response requested." marker. Versions of the Claude adapter
-- before this migration passed that string through verbatim, so on
-- real stores it surfaced as a distinct ``<synthetic>`` row in
-- ``stackunderflow compare`` (zero tokens, zero cost — pure noise).
--
-- The adapter is now fixed (``_model_from`` in adapters/claude.py drops
-- the sentinel), but historical rows still carry the bogus id. Rewrite
-- them to ``NULL`` so cost/compare paths skip them the same way they
-- skip any other ``model IS NULL`` record. The error message itself
-- stays in ``content_text`` and ``raw_json`` — only the bogus model id
-- is cleared.

BEGIN;

UPDATE messages SET model = NULL WHERE model = '<synthetic>';

PRAGMA user_version = 4;

COMMIT;
