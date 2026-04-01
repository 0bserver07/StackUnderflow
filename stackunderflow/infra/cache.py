"""Project data cache with hot (memory) and cold (disk) tiers.

Eviction uses a weighted-score approach rather than simple LRU: each entry
gets a score based on access frequency and recency, and the lowest-scoring
entry is evicted first.  This avoids the identical 5-minute-protection +
float(inf) pattern used in simpler caches.
"""

from __future__ import annotations

import hashlib
import json
import math
import shutil
import time
from collections import OrderedDict
from datetime import datetime
from pathlib import Path
from typing import Any

_DECAY_HALF_LIFE = 180.0  # seconds — halves score every 3 minutes


class TieredCache:
    def __init__(
        self,
        max_slots: int = 5,
        max_mb: int = 500,
        disk_root: Path | None = None,
    ) -> None:
        self._hot: OrderedDict[str, _Entry] = OrderedDict()
        self._cap = max_slots
        self._budget = max_mb << 20  # MB → bytes via bit-shift

        self._cold = disk_root or (Path.home() / ".stackunderflow" / "cache")
        self._cold.mkdir(parents=True, exist_ok=True)

        self._n_hit = self._n_miss = self._n_evict = self._n_reject = 0

    # ── hot tier ─────────────────────────────────────────────────────────

    def fetch(self, key: str) -> tuple[list[dict], dict] | None:
        entry = self._hot.get(key)
        if entry is None:
            self._n_miss += 1
            return None
        entry.touch()
        self._hot.move_to_end(key)
        self._n_hit += 1
        return entry.messages, entry.stats

    def store(self, key: str, messages: list[dict], stats: dict, force: bool = False) -> bool:
        size = _byte_estimate(messages, stats)
        if size > self._budget:
            self._n_reject += 1
            return False

        while len(self._hot) >= self._cap:
            if not self._shed(force):
                return False

        # Enforce aggregate memory budget
        current_total = sum(e.size for e in self._hot.values())
        while current_total + size > self._budget and self._hot:
            if not self._shed(force):
                break
            current_total = sum(e.size for e in self._hot.values())
        if current_total + size > self._budget:
            self._n_reject += 1
            return False

        self._hot[key] = _Entry(messages, stats, size)
        return True

    def drop(self, key: str) -> bool:
        return self._hot.pop(key, None) is not None

    def wipe(self) -> None:
        self._hot.clear()

    def metrics(self) -> dict[str, Any]:
        total = self._n_hit + self._n_miss
        return {
            "projects_cached": len(self._hot),
            "max_projects": self._cap,
            "total_size_mb": sum(e.size for e in self._hot.values()) / (1 << 20),
            "hits": self._n_hit,
            "misses": self._n_miss,
            "hit_rate": (self._n_hit / total * 100) if total else 0.0,
            "evictions": self._n_evict,
            "size_rejections": self._n_reject,
            "cache_keys": list(self._hot.keys()),
        }

    def slot_info(self, key: str) -> dict[str, Any] | None:
        e = self._hot.get(key)
        if e is None:
            return None
        now = time.time()
        return {
            "path": key,
            "message_count": len(e.messages),
            "size_mb": e.size / (1 << 20),
            "cached_at": e.born,
            "age_seconds": now - e.born,
            "last_accessed": e.last_ts,
            "last_access_age_seconds": now - e.last_ts,
        }

    # ── cold tier ────────────────────────────────────────────────────────

    def has_disk_changes(self, key: str) -> bool:
        mp = self._meta_file(key)
        if not mp.exists():
            return True
        try:
            old = json.loads(mp.read_text()).get("fingerprint", {})
            return old != self._fingerprint(key)
        except Exception:
            return True

    def load_stats(self, key: str) -> dict | None:
        if self.has_disk_changes(key):
            return None
        return _slurp_json(self._data_file(key, "stats.json"))

    def load_messages(self, key: str) -> list[dict] | None:
        if self.has_disk_changes(key):
            return None
        return _slurp_json(self._data_file(key, "messages.json"))

    def persist_stats(self, key: str, stats: dict) -> None:
        _dump_json(self._data_file(key, "stats.json"), stats)
        self._write_meta(key)

    def persist_messages(self, key: str, messages: list[dict]) -> None:
        _dump_json(self._data_file(key, "messages.json"), messages)
        self._write_meta(key)

    def invalidate_disk(self, key: str) -> None:
        d = self._cold / self._slug(key)
        if d.exists():
            shutil.rmtree(d)

    def disk_info(self, key: str) -> dict | None:
        mp = self._meta_file(key)
        if not mp.exists():
            return None
        try:
            meta = json.loads(mp.read_text())
            return {
                "cached_at": meta.get("when"),
                "has_stats": self._data_file(key, "stats.json").exists(),
                "has_messages": self._data_file(key, "messages.json").exists(),
                "is_valid": not self.has_disk_changes(key),
            }
        except Exception:
            return None

    def clear_disk(self) -> None:
        if self._cold.exists():
            shutil.rmtree(self._cold)
            self._cold.mkdir(parents=True, exist_ok=True)

    # ── eviction (weighted score) ────────────────────────────────────────

    def _shed(self, force: bool) -> bool:
        """Remove the entry with the lowest retention score."""
        if not self._hot:
            return False

        now = time.time()
        worst_key: str | None = None
        worst_score = math.inf

        for k, e in self._hot.items():
            sc = e.retention_score(now)
            if sc < worst_score:
                worst_score = sc
                worst_key = k

        if worst_key is None:
            return False

        # only refuse to evict if non-forced AND score is very high (recently/frequently used)
        if not force and worst_score > 10.0:
            return False

        del self._hot[worst_key]
        self._n_evict += 1
        return True

    # ── disk helpers ─────────────────────────────────────────────────────

    @staticmethod
    def _slug(key: str) -> str:
        return hashlib.sha1(key.encode(), usedforsecurity=False).hexdigest()[:16]

    def _data_file(self, key: str, name: str) -> Path:
        d = self._cold / self._slug(key)
        d.mkdir(exist_ok=True)
        return d / name

    def _meta_file(self, key: str) -> Path:
        return self._data_file(key, "meta.json")

    def _write_meta(self, key: str) -> None:
        _dump_json(self._meta_file(key), {
            "source": key,
            "when": datetime.now().isoformat(),
            "fingerprint": self._fingerprint(key),
        })

    @staticmethod
    def _fingerprint(key: str) -> dict[str, str]:
        d = Path(key)
        if not d.is_dir():
            return {}
        return {
            f.name: f"{f.stat().st_size}:{int(f.stat().st_mtime)}"
            for f in sorted(d.glob("*.jsonl"))
        }


# ── entry container ──────────────────────────────────────────────────────────

class _Entry:
    __slots__ = ("messages", "stats", "size", "born", "last_ts", "hits")

    def __init__(self, messages: list[dict], stats: dict, size: int) -> None:
        self.messages = messages
        self.stats = stats
        self.size = size
        now = time.time()
        self.born = now
        self.last_ts = now
        self.hits = 1

    def touch(self) -> None:
        self.last_ts = time.time()
        self.hits += 1

    def retention_score(self, now: float) -> float:
        """Higher score = more worth keeping.

        score = hits * 2^(-age / half_life)
        A frequently-accessed entry keeps a high score; an entry that
        hasn't been touched decays exponentially.
        """
        age = now - self.last_ts
        return self.hits * (2.0 ** (-age / _DECAY_HALF_LIFE))


# ── I/O ──────────────────────────────────────────────────────────────────────

def _byte_estimate(messages: list[dict], stats: dict) -> int:
    try:
        payload = json.dumps(messages, separators=(",", ":"))
        payload += json.dumps(stats, separators=(",", ":"))
        return int(len(payload) * 1.4)  # overhead factor
    except Exception:
        return len(messages) * 800


def _slurp_json(p: Path) -> Any:
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except Exception:
        return None


def _dump_json(p: Path, obj: Any) -> None:
    p.write_text(json.dumps(obj, indent=2))
