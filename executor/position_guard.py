from __future__ import annotations

import os
import time
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional, Tuple

import requests

from py_clob_client.clob_types import (
    AssetType,
    BalanceAllowanceParams,
    MarketOrderArgs,
    OpenOrderParams,
    OrderArgs,
    OrderType,
)
from py_clob_client.order_builder.constants import SELL


CLOB_URL = os.getenv("POLY_CLOB_URL", "https://clob.polymarket.com").rstrip("/")
UNWIND_LADDER_ENABLED = os.getenv("UNWIND_LADDER_ENABLED", "true").strip().lower() == "true"
UNWIND_LADDER_MAX_LEVELS = max(1, int(os.getenv("UNWIND_LADDER_MAX_LEVELS", "4")))
UNWIND_LADDER_LIQUIDITY_FACTOR = max(0.1, min(1.0, float(os.getenv("UNWIND_LADDER_LIQUIDITY_FACTOR", "0.9"))))
UNWIND_MIN_BID_PRICE = max(0.001, float(os.getenv("UNWIND_MIN_BID_PRICE", "0.02")))
UNWIND_MIN_REMAINING_SHARES = max(0.0, float(os.getenv("UNWIND_MIN_REMAINING_SHARES", "0.5")))


@dataclass
class CashoutResult:
    token_id: str
    requested_shares: float
    order_id: Optional[str]
    ok: bool
    error: Optional[str] = None


class PositionGuard:
    """Tracks account positions + open orders and force-unwinds risky partial exposure."""

    def __init__(self, client, min_shares: float, max_shares: Optional[float]) -> None:
        self.client = client
        self.min_shares = float(min_shares)
        self.max_shares = float(max_shares) if max_shares is not None else None

    @staticmethod
    def _parse_float(value, default: float = 0.0) -> float:
        try:
            return float(value)
        except Exception:
            return default

    def get_token_shares(self, token_id: str) -> float:
        """Get current position size for a token."""
        try:
            resp = self.client.get_balance_allowance(
                BalanceAllowanceParams(asset_type=AssetType.CONDITIONAL, token_id=token_id)
            )
            if isinstance(resp, dict):
                for key in ("balance", "available", "value"):
                    if key in resp:
                        return self._parse_float(resp.get(key), 0.0)
        except Exception as exc:
            print(f"[position_guard] Error fetching balance for {token_id}: {exc}")
        return 0.0

    def get_positions(self, token_ids: Iterable[str]) -> Dict[str, float]:
        return {token_id: self.get_token_shares(token_id) for token_id in token_ids}

    def can_add_shares(self, token_id: str, requested_shares: float) -> bool:
        if self.max_shares is None:
            return True
        current = self.get_token_shares(token_id)
        return (current + float(requested_shares)) <= self.max_shares + 1e-9

    def cancel_open_orders_for_tokens(self, token_ids: Iterable[str]) -> List[str]:
        """Cancel all open orders for given tokens."""
        cancelled: List[str] = []
        for token_id in token_ids:
            try:
                orders = self.client.get_orders(OpenOrderParams(asset_id=token_id))
                order_ids = [
                    o.get("id") or o.get("orderID") for o in orders if isinstance(o, dict)
                ]
                order_ids = [oid for oid in order_ids if oid]
                if not order_ids:
                    continue
                self.client.cancel_orders(order_ids)
                cancelled.extend(order_ids)
            except Exception as exc:
                print(f"[position_guard] Error cancelling orders for {token_id}: {exc}")
        return cancelled

    def _fetch_orderbook_bids(self, token_id: str) -> List[Tuple[float, float]]:
        try:
            resp = requests.get(f"{CLOB_URL}/book", params={"token_id": token_id}, timeout=1.5)
            resp.raise_for_status()
            payload = resp.json()
        except Exception as exc:
            print(f"[position_guard] orderbook fetch failed for {token_id}: {exc}")
            return []

        raw_bids = payload.get("bids") if isinstance(payload, dict) else None
        if not isinstance(raw_bids, list):
            return []

        parsed: List[Tuple[float, float]] = []
        for lvl in raw_bids:
            if not isinstance(lvl, dict):
                continue
            try:
                p = float(lvl.get("price", 0.0))
                s = float(lvl.get("size", 0.0))
            except Exception:
                continue
            if p > 0.0 and s > 0.0:
                parsed.append((p, s))

        parsed.sort(key=lambda x: x[0], reverse=True)
        return parsed

    def _place_limit_sell(self, token_id: str, shares: float, price: float, fok: bool = True) -> Optional[str]:
        if shares <= 0.0 or price <= 0.0:
            return None
        args = OrderArgs(token_id=token_id, price=float(price), size=float(shares), side=SELL)
        signed = self.client.create_order(args)
        posted = self.client.post_order(signed, OrderType.FOK if fok else OrderType.GTC)
        if isinstance(posted, dict):
            return posted.get("orderID") or posted.get("order_id") or posted.get("id")
        return None

    def _laddered_unwind(self, token_id: str, target_qty: float) -> Tuple[float, Optional[str], List[str]]:
        bids = self._fetch_orderbook_bids(token_id)
        if not bids:
            return 0.0, None, ["no_bids"]

        sold = 0.0
        last_order_id: Optional[str] = None
        errors: List[str] = []
        levels = 0

        for price, size in bids:
            if levels >= UNWIND_LADDER_MAX_LEVELS:
                break
            if price < UNWIND_MIN_BID_PRICE:
                continue

            remaining = max(0.0, target_qty - sold)
            if remaining <= UNWIND_MIN_REMAINING_SHARES:
                break

            qty = min(remaining, size * UNWIND_LADDER_LIQUIDITY_FACTOR)
            if qty <= 0.0:
                continue

            try:
                oid = self._place_limit_sell(token_id, qty, price, fok=True)
                last_order_id = oid or last_order_id
                sold += qty
                levels += 1
                print(
                    f"[position_guard] ladder unwind success token={token_id[:10]}.. qty={qty:.4f} price={price:.4f}"
                )
            except Exception as exc:
                errors.append(str(exc))
                levels += 1

        return sold, last_order_id, errors

    def cashout_market(
        self,
        token_id: str,
        shares: Optional[float] = None,
        max_retries: int = 3,
        retry_delay_ms: int = 300,
    ) -> CashoutResult:
        """Enhanced cashout with retries, fresh-balance checks, and safety margins."""
        cancelled_orders = self.cancel_open_orders_for_tokens([token_id])
        if cancelled_orders:
            time.sleep(max(0, retry_delay_ms) / 1000.0)

        target_qty = self.get_token_shares(token_id) if shares is None else float(shares)
        target_qty = max(0.0, target_qty)
        if target_qty <= 0.0:
            return CashoutResult(
                token_id=token_id,
                requested_shares=0.0,
                order_id=None,
                ok=True,
                error="No balance",
            )

        attempts = max(1, int(max_retries))
        last_error: Optional[str] = None
        for attempt in range(attempts):
            available = max(0.0, self.get_token_shares(token_id))
            base_qty = min(target_qty, available) if shares is not None else available

            # 99% first attempt, progressively down to 96% on later attempts.
            safety_factor = max(0.96, 0.99 - (0.01 * attempt))
            adjusted_qty = max(0.0, base_qty * safety_factor)

            if adjusted_qty <= 0.0:
                return CashoutResult(
                    token_id=token_id,
                    requested_shares=0.0,
                    order_id=None,
                    ok=True,
                    error="No balance after refresh",
                )

            try:
                ladder_order_id: Optional[str] = None
                remaining_qty = adjusted_qty

                if UNWIND_LADDER_ENABLED:
                    sold_qty, ladder_order_id, ladder_errors = self._laddered_unwind(
                        token_id, adjusted_qty
                    )
                    remaining_qty = max(0.0, adjusted_qty - sold_qty)
                    if ladder_errors:
                        print(
                            f"[position_guard] ladder unwind had {len(ladder_errors)} errors for {token_id}: "
                            f"{ladder_errors[0]}"
                        )

                if remaining_qty <= UNWIND_MIN_REMAINING_SHARES:
                    return CashoutResult(
                        token_id=token_id,
                        requested_shares=adjusted_qty,
                        order_id=ladder_order_id,
                        ok=True,
                    )

                market_args = MarketOrderArgs(
                    token_id=token_id,
                    amount=remaining_qty,
                    side=SELL,
                    order_type=OrderType.GTC,
                )
                signed = self.client.create_market_order(market_args)
                posted = self.client.post_order(signed, market_args.order_type)
                market_order_id = posted.get("orderID") if isinstance(posted, dict) else None
                return CashoutResult(
                    token_id=token_id,
                    requested_shares=adjusted_qty,
                    order_id=market_order_id or ladder_order_id,
                    ok=True,
                )
            except Exception as exc:
                last_error = str(exc)
                error_msg = last_error.lower()

                if "balance" in error_msg or "allowance" in error_msg:
                    target_qty = max(0.0, self.get_token_shares(token_id))
                    time.sleep(max(0, retry_delay_ms) / 1000.0)
                    continue

                if "not found" in error_msg or "invalid token" in error_msg:
                    return CashoutResult(
                        token_id=token_id,
                        requested_shares=adjusted_qty,
                        order_id=None,
                        ok=False,
                        error=f"Invalid token: {exc}",
                    )

                if attempt < attempts - 1:
                    wait_ms = max(0, retry_delay_ms) * (attempt + 1)
                    time.sleep(wait_ms / 1000.0)
                    continue

                return CashoutResult(
                    token_id=token_id,
                    requested_shares=adjusted_qty,
                    order_id=None,
                    ok=False,
                    error=last_error,
                )

        return CashoutResult(
            token_id=token_id,
            requested_shares=target_qty,
            order_id=None,
            ok=False,
            error=f"Failed after {attempts} attempts: {last_error or 'unknown error'}",
        )

    def preflight_before_new_pair(
        self,
        token_ids: Iterable[str],
        requested_shares: Optional[Dict[str, float]] = None,
    ) -> dict:
        token_ids = [t for t in token_ids if t]
        requested_shares = requested_shares or {}

        cancelled_order_ids = self.cancel_open_orders_for_tokens(token_ids)
        if cancelled_order_ids:
            time.sleep(0.3)

        positions = self.get_positions(token_ids)

        forced_cashouts: List[CashoutResult] = []
        blocked_tokens: List[str] = []

        for token_id, shares in positions.items():
            if self.max_shares is None:
                continue

            req = float(requested_shares.get(token_id, self.min_shares))
            projected = shares + req
            if projected > self.max_shares + 1e-9:
                blocked_tokens.append(token_id)
                forced_cashouts.append(self.cashout_market(token_id, shares))

        return {
            "cancelled_order_ids": cancelled_order_ids,
            "positions": positions,
            "blocked_tokens": blocked_tokens,
            "forced_cashouts": [c.__dict__ for c in forced_cashouts],
            "can_open_new_pair": len(blocked_tokens) == 0,
        }
