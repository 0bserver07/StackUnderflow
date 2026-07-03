"""Key resolution / storage / fingerprint. The resolution + file legs need no
optional dependency; identity generation is gated on pyrage."""

from __future__ import annotations

import os
import stat

import pytest

from stackunderflow.sync import keys


def test_fingerprint_is_stable_short_hex():
    fp = keys.fingerprint("age1exampledummyrecipient")
    assert fp == keys.fingerprint("age1exampledummyrecipient")
    assert len(fp) == 16
    assert all(c in "0123456789abcdef" for c in fp)
    assert keys.fingerprint("age1other") != fp


def test_resolve_secret_env_wins(tmp_path):
    keys.store_secret_file("FILE-KEY", tmp_path)
    got = keys.resolve_secret(
        tmp_path,
        env={keys.ENV_KEY: "ENV-KEY"},
        keychain_reader=lambda: None,
    )
    assert got == "ENV-KEY"


def test_resolve_secret_keychain_before_file(tmp_path):
    keys.store_secret_file("FILE-KEY", tmp_path)
    got = keys.resolve_secret(
        tmp_path,
        env={},
        keychain_reader=lambda: "KEYCHAIN-KEY",
    )
    assert got == "KEYCHAIN-KEY"


def test_resolve_secret_falls_back_to_file(tmp_path):
    keys.store_secret_file("FILE-KEY", tmp_path)
    got = keys.resolve_secret(tmp_path, env={}, keychain_reader=lambda: None)
    assert got == "FILE-KEY"


def test_resolve_secret_none_when_absent(tmp_path):
    got = keys.resolve_secret(tmp_path, env={}, keychain_reader=lambda: None)
    assert got is None


def test_store_secret_file_is_0600(tmp_path):
    path = keys.store_secret_file("AGE-SECRET-KEY-1-EXAMPLE", tmp_path)
    assert path == keys.identity_path(tmp_path)
    mode = stat.S_IMODE(os.stat(path).st_mode)
    assert mode == 0o600
    assert path.read_text().strip() == "AGE-SECRET-KEY-1-EXAMPLE"


def test_generate_identity_and_recipient_for():
    pytest.importorskip("pyrage")
    identity = keys.generate_identity()
    assert identity.secret.startswith("AGE-SECRET-KEY-1")
    assert identity.recipient.startswith("age1")
    assert identity.fingerprint == keys.fingerprint(identity.recipient)
    # The public recipient is recoverable from the secret alone.
    assert keys.recipient_for(identity.secret) == identity.recipient
