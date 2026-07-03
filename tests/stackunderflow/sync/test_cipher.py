"""age encrypt/decrypt roundtrip + failure modes. Gated on pyrage."""

from __future__ import annotations

import pytest

pytest.importorskip("pyrage")

from stackunderflow.sync import cipher, keys  # noqa: E402 - after importorskip


def test_encrypt_decrypt_roundtrip():
    identity = keys.generate_identity()
    ct = cipher.encrypt(b"the quick brown fox", identity.recipient)
    assert ct != b"the quick brown fox"  # actually encrypted
    assert cipher.decrypt(ct, identity.secret) == b"the quick brown fox"


def test_wrong_key_raises_clean_decrypt_error():
    owner = keys.generate_identity()
    other = keys.generate_identity()
    ct = cipher.encrypt(b"secret aggregate", owner.recipient)
    with pytest.raises(cipher.DecryptError):
        cipher.decrypt(ct, other.secret)


def test_tampered_ciphertext_raises():
    identity = keys.generate_identity()
    ct = bytearray(cipher.encrypt(b"payload bytes here", identity.recipient))
    ct[-1] ^= 0xFF  # flip the last byte
    with pytest.raises(cipher.DecryptError):
        cipher.decrypt(bytes(ct), identity.secret)
