from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional

from py_clob_client.clob_types import (
    AssetType,
    BalanceAllowanceParams,
    MarketOrderArgs,
    OpenOrderParams,
    OrderType,
)
from py_clob_client.order_builder.constants import SELL


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
                market_args = MarketOrderArgs(
                    token_id=token_id,
                    amount=adjusted_qty,
                    side=SELL,
                    order_type=OrderType.GTC,
                )
                signed = self.client.create_market_order(market_args)
                posted = self.client.post_order(signed, market_args.order_type)
                order_id = posted.get("orderID") if isinstance(posted, dict) else None
                return CashoutResult(
                    token_id=token_id,
                    requested_shares=adjusted_qty,
                    order_id=order_id,
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
