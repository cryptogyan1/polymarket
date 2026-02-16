"""Telegram notification helpers for trade, partial-fill, and unwind events."""

from __future__ import annotations

import os
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Dict, Optional

from dotenv import load_dotenv
from telegram import Bot
from telegram.constants import ParseMode

load_dotenv()


@dataclass
class LegInfo:
    token: str
    shares: float
    price: float
    cost_usdc: float
    order_id: Optional[str] = None
    status: str = "pending"
    error: Optional[str] = None


@dataclass
class TradeResult:
    timestamp: str
    direction: str
    leg1: LegInfo
    leg2: LegInfo
    total_cost: float
    combined_price: float
    target_price: float
    profit_potential: Optional[float] = None
    execution_time_seconds: Optional[float] = None


@dataclass
class UnwindInfo:
    timestamp: str
    token: str
    shares_to_sell: float
    original_cost: float
    market_price: Optional[float] = None
    received_usdc: Optional[float] = None
    order_id: Optional[str] = None
    status: str = "pending"
    original_direction: str = ""
    failed_leg: str = ""


class TelegramNotifier:
    def __init__(self, bot_token: Optional[str] = None, chat_id: Optional[str] = None):
        self.bot_token = bot_token or os.getenv("TELEGRAM_BOT_TOKEN")
        self.chat_id = chat_id or os.getenv("TELEGRAM_CHAT_ID")

        if not self.bot_token or not self.chat_id:
            raise ValueError("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be set")

        self.bot = Bot(token=self.bot_token)
        self._unwind_message_ids: Dict[str, int] = {}

    async def send_both_legs_filled(self, trade: TradeResult) -> None:
        profit_value = trade.profit_potential or 0.0
        profit_pct = (profit_value / trade.total_cost * 100.0) if trade.total_cost > 0 else 0.0

        message = f"""🎯 <b>ARBITRAGE FILLED</b>

📊 Market Window: {trade.timestamp}
Direction: {trade.direction}

<b>LEG 1: {trade.leg1.token}</b>
✅ Filled: {trade.leg1.shares:.0f} shares @ ${trade.leg1.price:.3f}
💰 Cost: ${trade.leg1.cost_usdc:.2f} USDC
📋 Order ID: <code>{self._shorten_id(trade.leg1.order_id)}</code>

<b>LEG 2: {trade.leg2.token}</b>
✅ Filled: {trade.leg2.shares:.0f} shares @ ${trade.leg2.price:.3f}
💰 Cost: ${trade.leg2.cost_usdc:.2f} USDC
📋 Order ID: <code>{self._shorten_id(trade.leg2.order_id)}</code>

━━━━━━━━━━━━━━━━━━━━━
💵 Total Cost: ${trade.total_cost:.2f} USDC
📈 Combined Price: {trade.combined_price:.3f}
🎯 Target: < {trade.target_price:.3f}
✨ Profit Potential: ~${profit_value:.2f} ({profit_pct:.2f}%)
⏱️ Execution Time: {(trade.execution_time_seconds or 0.0):.1f}s
"""

        await self.bot.send_message(
            chat_id=self.chat_id,
            text=message,
            parse_mode=ParseMode.HTML,
        )

    async def send_one_leg_alert(self, trade: TradeResult) -> None:
        filled_leg = trade.leg1 if trade.leg1.status == "filled" else trade.leg2
        failed_leg = trade.leg2 if trade.leg1.status == "filled" else trade.leg1

        message = f"""⚠️ <b>PARTIAL FILL ALERT</b>

🚨 <b>ONLY ONE LEG EXECUTED</b>
⏰ {trade.timestamp}

<b>FILLED LEG:</b>
✅ {filled_leg.token}
   • Shares: {filled_leg.shares:.0f} @ ${filled_leg.price:.3f}
   • Cost: ${filled_leg.cost_usdc:.2f} USDC
   • Order ID: <code>{self._shorten_id(filled_leg.order_id)}</code>

<b>FAILED LEG:</b>
❌ {failed_leg.token}
   • Target: {failed_leg.shares:.0f} shares @ ${failed_leg.price:.3f}
   • Status: Order rejected
   • Error: \"{failed_leg.error or 'Unknown error'}\"

━━━━━━━━━━━━━━━━━━━━━
⚠️ <b>EXPOSURE RISK:</b>
• Holding {filled_leg.shares:.0f} {filled_leg.token} shares
• No hedge position
• Market risk: ACTIVE

🔔 <b>ACTION REQUIRED:</b>
Bot will attempt automatic unwind.
"""

        await self.bot.send_message(
            chat_id=self.chat_id,
            text=message,
            parse_mode=ParseMode.HTML,
            disable_notification=False,
        )

    async def send_unwind_initiated(self, unwind: UnwindInfo) -> int:
        message = f"""🔄 <b>UNWINDING POSITION</b>

⚠️ Emergency exit initiated
⏰ {unwind.timestamp}

<b>UNWINDING:</b>
📉 Selling: <b>{unwind.shares_to_sell:.0f} {unwind.token} shares</b>
💵 Original Cost: ${unwind.original_cost:.2f} USDC
📊 Market Sell Order (GTC)

<b>STATUS: 🟡 PENDING...</b>
⏳ Attempting to exit at market price
"""

        result = await self.bot.send_message(
            chat_id=self.chat_id,
            text=message,
            parse_mode=ParseMode.HTML,
        )
        self._unwind_message_ids[unwind.token] = result.message_id
        return result.message_id

    async def update_unwind_complete(self, unwind: UnwindInfo) -> None:
        message_id = self._unwind_message_ids.get(unwind.token)
        received = unwind.received_usdc if unwind.received_usdc is not None else 0.0
        market_price = unwind.market_price if unwind.market_price is not None else 0.0
        net_result = received - unwind.original_cost
        net_pct = (net_result / unwind.original_cost * 100.0) if unwind.original_cost > 0 else 0.0

        message = f"""✅ <b>UNWIND COMPLETE</b>

🔄 Position closed
⏰ {unwind.timestamp}

<b>SOLD:</b>
📉 {unwind.shares_to_sell:.0f} {unwind.token} shares @ ${market_price:.3f}
💰 Received: ${received:.2f} USDC
📋 Order ID: <code>{self._shorten_id(unwind.order_id)}</code>

━━━━━━━━━━━━━━━━━━━━━
💸 <b>NET RESULT:</b> {'Loss' if net_result < 0 else 'Gain'}: <b>${net_result:.2f} ({net_pct:+.2f}%)</b>
"""

        if message_id is None:
            await self.bot.send_message(chat_id=self.chat_id, text=message, parse_mode=ParseMode.HTML)
        else:
            try:
                await self.bot.edit_message_text(
                    chat_id=self.chat_id,
                    message_id=message_id,
                    text=message,
                    parse_mode=ParseMode.HTML,
                )
            except Exception:
                await self.bot.send_message(chat_id=self.chat_id, text=message, parse_mode=ParseMode.HTML)
            finally:
                self._unwind_message_ids.pop(unwind.token, None)

    async def send_unwind_failed(self, unwind: UnwindInfo, error: str) -> None:
        message = f"""❌ <b>UNWIND FAILED</b>

⏰ {unwind.timestamp}
Token: {unwind.token}
Requested shares: {unwind.shares_to_sell:.4f}
Error: {error}
"""
        await self.bot.send_message(chat_id=self.chat_id, text=message, parse_mode=ParseMode.HTML)

    @staticmethod
    def format_timestamp(dt: Optional[datetime] = None) -> str:
        if dt is None:
            dt = datetime.utcnow()
        return dt.strftime("%Y-%m-%d %H:%M:%S UTC")

    def _shorten_id(self, order_id: Optional[str]) -> str:
        if not order_id:
            return "N/A"
        if len(order_id) > 14:
            return f"{order_id[:6]}...{order_id[-4:]}"
        return order_id


def leg_info_from_dict(data: Dict[str, Any]) -> LegInfo:
    return LegInfo(
        token=str(data.get("token", "")),
        shares=float(data.get("shares", 0.0)),
        price=float(data.get("price", 0.0)),
        cost_usdc=float(data.get("cost_usdc", 0.0)),
        order_id=data.get("order_id"),
        status=str(data.get("status", "pending")),
        error=data.get("error"),
    )
