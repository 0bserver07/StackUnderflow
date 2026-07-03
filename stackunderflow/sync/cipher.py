"""``age`` encryption for sync shards — no rolled-own crypto.

Every blob is encrypted with ``age`` (an audited file-encryption format) via the
``pyrage`` binding: ephemeral X25519 → HKDF-SHA256 → ChaCha20-Poly1305 over a
chunked STREAM construction. We only ever call ``encrypt(recipient, plaintext)``
/ ``decrypt(identity, bytes)``; nothing lower-level (nonces, AEAD, key schedule)
is our responsibility.

``pyrage`` is imported on demand so a core install (no ``[sync]`` extra) can
still import this module.
"""

from __future__ import annotations

from .keys import SyncDependencyError, _pyrage


class DecryptError(RuntimeError):
    """A blob could not be decrypted (wrong key, or corrupt/tampered ciphertext).

    ``age``'s per-frame AEAD authenticates every shard, so a truncated, swapped
    or tampered blob fails to decrypt rather than returning a silent partial
    read. A wrong key raises this too — cleanly, with no local mutation.
    """


def encrypt(plaintext: bytes, recipient: str) -> bytes:
    """Encrypt *plaintext* to the age *recipient* (``age1…``). Returns ciphertext."""
    pyrage = _pyrage()
    recip = pyrage.x25519.Recipient.from_str(recipient.strip())
    return pyrage.encrypt(plaintext, [recip])


def decrypt(ciphertext: bytes, secret: str) -> bytes:
    """Decrypt *ciphertext* with the age *secret* (``AGE-SECRET-KEY-1…``).

    Raises :class:`DecryptError` on a wrong key or a corrupt/tampered blob.
    """
    pyrage = _pyrage()
    ident = pyrage.x25519.Identity.from_str(secret.strip())
    try:
        return pyrage.decrypt(ciphertext, [ident])
    except SyncDependencyError:
        raise
    except Exception as exc:
        raise DecryptError(
            "could not decrypt — the blob is not encrypted for your key, "
            "or it is corrupt/tampered"
        ) from exc
