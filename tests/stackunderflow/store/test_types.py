from stackunderflow.store.types import DayTotals, MessageRow, ProjectRow, SessionRow


def test_project_row_fields() -> None:
    p = ProjectRow(
        id=1, provider="claude", slug="-a", path="/a",
        display_name="a", first_seen=0.0, last_modified=0.0,
    )
    assert p.provider == "claude"


def test_session_row_fields() -> None:
    s = SessionRow(
        id=1, project_id=1, session_id="abc",
        first_ts="2026-01-01T00:00:00+00:00",
        last_ts="2026-01-01T01:00:00+00:00",
        message_count=5,
    )
    assert s.message_count == 5


def test_message_row_fields() -> None:
    m = MessageRow(
        id=1, session_fk=1, seq=0,
        timestamp="2026-01-01T00:00:00+00:00",
        role="user", model=None,
        input_tokens=10, output_tokens=20,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="hello", tools_json="[]", raw_json="{}",
        is_sidechain=False, uuid="u", parent_uuid=None,
    )
    assert m.input_tokens == 10


def test_day_totals_fields() -> None:
    d = DayTotals(
        date="2026-01-01", input_tokens=1, output_tokens=2,
        cache_create_tokens=0, cache_read_tokens=0, message_count=3,
    )
    assert d.date == "2026-01-01"
