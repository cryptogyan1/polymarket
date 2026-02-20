from __future__ import annotations

import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional


@dataclass
class JournalEntry:
    event_type: str
    token_id: str
    side: str
    price: float
    size_usdc: float
    order_id: Optional[str]
    status: str
    fill_confirmed: bool
    fill_reason: Optional[str] = None
    metadata_json: Optional[str] = None


class TradeJournal:
    def __init__(self, db_path: str) -> None:
        self.db_path = db_path
        self._lock = threading.Lock()
        Path(db_path).parent.mkdir(parents=True, exist_ok=True)
        self._init_db()

    def _conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.db_path, timeout=5, isolation_level=None)
        conn.row_factory = sqlite3.Row
        return conn

    def _init_db(self) -> None:
        with self._lock, self._conn() as conn:
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS trade_journal (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    token_id TEXT NOT NULL,
                    side TEXT NOT NULL,
                    price REAL NOT NULL,
                    size_usdc REAL NOT NULL,
                    order_id TEXT,
                    status TEXT NOT NULL,
                    fill_confirmed INTEGER NOT NULL,
                    fill_reason TEXT,
                    metadata_json TEXT
                )
                """
            )
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_trade_journal_ts ON trade_journal(ts_ms DESC)"
            )
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_trade_journal_status ON trade_journal(status)"
            )

    def insert(self, entry: JournalEntry) -> int:
        with self._lock, self._conn() as conn:
            cur = conn.execute(
                """
                INSERT INTO trade_journal (
                    ts_ms, event_type, token_id, side, price, size_usdc, order_id,
                    status, fill_confirmed, fill_reason, metadata_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    int(time.time() * 1000),
                    entry.event_type,
                    entry.token_id,
                    entry.side,
                    float(entry.price),
                    float(entry.size_usdc),
                    entry.order_id,
                    entry.status,
                    1 if entry.fill_confirmed else 0,
                    entry.fill_reason,
                    entry.metadata_json,
                ),
            )
            return int(cur.lastrowid)

    def recent(self, limit: int = 50) -> List[Dict[str, Any]]:
        limit = max(1, min(int(limit), 500))
        with self._lock, self._conn() as conn:
            rows = conn.execute(
                "SELECT * FROM trade_journal ORDER BY id DESC LIMIT ?", (limit,)
            ).fetchall()
        return [dict(r) for r in rows]

    def summary(self) -> Dict[str, Any]:
        with self._lock, self._conn() as conn:
            total = conn.execute("SELECT COUNT(*) AS c FROM trade_journal").fetchone()["c"]
            confirmed = conn.execute(
                "SELECT COUNT(*) AS c FROM trade_journal WHERE fill_confirmed = 1"
            ).fetchone()["c"]
            rejected = conn.execute(
                "SELECT COUNT(*) AS c FROM trade_journal WHERE status IN ('rejected','error','timeout')"
            ).fetchone()["c"]
            by_status_rows = conn.execute(
                "SELECT status, COUNT(*) AS c FROM trade_journal GROUP BY status ORDER BY c DESC"
            ).fetchall()

        return {
            "total_orders": int(total),
            "fill_confirmed_orders": int(confirmed),
            "fill_confirmation_rate": (float(confirmed) / float(total)) if total else 0.0,
            "failed_or_timed_out": int(rejected),
            "by_status": {r["status"]: int(r["c"]) for r in by_status_rows},
        }
