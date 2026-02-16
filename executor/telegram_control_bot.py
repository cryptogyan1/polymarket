"""Telegram control bot for manual portfolio actions.

Buttons (always visible):
- TRACK: fetch current open positions with share count
- KILL: cashout all open positions immediately
- CLAIM: redeem settled positions onchain

This bot uses a dedicated RPC URL (TELEGRAM_CONTROL_RPC_URL) for CLAIM transactions.
"""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

import requests
from dotenv import load_dotenv
from telegram import ReplyKeyboardMarkup, Update
from telegram.constants import ParseMode
from telegram.ext import (
    Application,
    ApplicationBuilder,
    CommandHandler,
    ContextTypes,
    MessageHandler,
    filters,
)
from web3 import Web3
from web3.exceptions import TimeExhausted

from executor.app import CLIENT, MAX_SHARES, MIN_SHARES
from executor.position_guard import PositionGuard

load_dotenv()

KEYBOARD = [["TRACK", "KILL", "CLAIM"]]
MARKUP = ReplyKeyboardMarkup(KEYBOARD, resize_keyboard=True, one_time_keyboard=False)

DATA_API_URL = os.getenv("POLYMARKET_DATA_API_URL", "https://data-api.polymarket.com")
TELEGRAM_BOT_TOKEN = os.getenv("TELEGRAM_BOT_TOKEN", "")
TELEGRAM_CHAT_ID = os.getenv("TELEGRAM_CHAT_ID", "")
PROXY_WALLET = os.getenv("PROXY_WALLET", "")
PRIVATE_KEY = os.getenv("PRIVATE_KEY", "")
TELEGRAM_CONTROL_RPC_URL = os.getenv("TELEGRAM_CONTROL_RPC_URL", os.getenv("RPC_URL", ""))

CTF_CONTRACT = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")
USDC_ADDRESS = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")

CTF_ABI = [
    {
        "inputs": [
            {"internalType": "address", "name": "collateralToken", "type": "address"},
            {"internalType": "bytes32", "name": "parentCollectionId", "type": "bytes32"},
            {"internalType": "bytes32", "name": "conditionId", "type": "bytes32"},
            {"internalType": "uint256[]", "name": "indexSets", "type": "uint256[]"},
        ],
        "name": "redeemPositions",
        "outputs": [],
        "stateMutability": "nonpayable",
        "type": "function",
    }
]


@dataclass
class PositionRow:
    token_id: str
    title: str
    shares: float
    condition_id: Optional[str] = None
    outcome: Optional[str] = None
    resolved: bool = False


def _normalize_share(value: Any) -> float:
    for key in ["size", "shares", "amount", "balance", "quantity", "position"]:
        if isinstance(value, dict) and key in value:
            try:
                return float(value[key])
            except Exception:
                continue
    try:
        return float(value)
    except Exception:
        return 0.0


def _extract_token_id(row: Dict[str, Any]) -> Optional[str]:
    for key in ["asset", "asset_id", "token_id", "tokenID", "tokenId"]:
        val = row.get(key)
        if val:
            return str(val)
    return None


def fetch_open_positions(wallet: str) -> List[PositionRow]:
    url = f"{DATA_API_URL.rstrip('/')}/positions"
    params = {"user": wallet, "sizeThreshold": "0"}
    resp = requests.get(url, params=params, timeout=20)
    resp.raise_for_status()
    data = resp.json()

    if not isinstance(data, list):
        return []

    out: List[PositionRow] = []
    for row in data:
        if not isinstance(row, dict):
            continue
        token_id = _extract_token_id(row)
        if not token_id:
            continue

        shares = _normalize_share(row)
        if shares <= 0:
            continue

        title = str(
            row.get("title")
            or row.get("market")
            or row.get("question")
            or row.get("slug")
            or token_id
        )
        condition_id = row.get("conditionId") or row.get("condition_id")
        outcome = row.get("outcome")
        resolved = bool(
            row.get("resolved")
            or row.get("isResolved")
            or row.get("redeemable")
            or row.get("claimable")
        )

        out.append(
            PositionRow(
                token_id=token_id,
                title=title,
                shares=shares,
                condition_id=str(condition_id) if condition_id else None,
                outcome=str(outcome) if outcome is not None else None,
                resolved=resolved,
            )
        )

    return out


def claim_settled_positions(wallet: str, rpc_url: str, private_key: str) -> List[str]:
    if not rpc_url:
        raise RuntimeError("TELEGRAM_CONTROL_RPC_URL (or RPC_URL) is required for CLAIM")
    if not private_key:
        raise RuntimeError("PRIVATE_KEY is required for CLAIM")

    positions = fetch_open_positions(wallet)
    settled_conditions = sorted(
        {p.condition_id for p in positions if p.condition_id and p.resolved}
    )

    if not settled_conditions:
        return []

    w3 = Web3(Web3.HTTPProvider(rpc_url))
    if not w3.is_connected():
        raise RuntimeError("failed connecting to TELEGRAM_CONTROL_RPC_URL")

    acct = w3.eth.account.from_key(private_key)
    ctf = w3.eth.contract(address=CTF_CONTRACT, abi=CTF_ABI)

    tx_hashes: List[str] = []
    nonce = w3.eth.get_transaction_count(acct.address)

    for condition_id in settled_conditions:
        cond_bytes = bytes.fromhex(condition_id[2:] if condition_id.startswith("0x") else condition_id)
        if len(cond_bytes) != 32:
            continue

        fee_params: Dict[str, int] = {}
        try:
            # Polygon gas can spike quickly; hardcoded fees often leave txs pending.
            latest_block = w3.eth.get_block("latest")
            base_fee = int(latest_block.get("baseFeePerGas", 0) or 0)
            priority_fee = int(w3.eth.max_priority_fee)
            if priority_fee <= 0:
                priority_fee = w3.to_wei("30", "gwei")
            fee_params = {
                "maxPriorityFeePerGas": priority_fee,
                # Use a multiplier so tx remains valid during short-lived fee spikes.
                "maxFeePerGas": max(base_fee * 2 + priority_fee, priority_fee),
            }
        except Exception:
            # Fallback for RPCs without EIP-1559 helpers.
            gas_price = int(w3.eth.gas_price)
            fee_params = {"gasPrice": max(gas_price, w3.to_wei("50", "gwei"))}

        txn = ctf.functions.redeemPositions(
            USDC_ADDRESS,
            b"\x00" * 32,
            cond_bytes,
            [1, 2],
        ).build_transaction(
            {
                "from": acct.address,
                "nonce": nonce,
                "gas": 500_000,
                "chainId": w3.eth.chain_id,
                **fee_params,
            }
        )

        signed = acct.sign_transaction(txn)
        txh = w3.eth.send_raw_transaction(signed.raw_transaction)
        tx_hashes.append(txh.hex())

        # Best-effort confirmation check to surface stuck txs early in chat output.
        try:
            w3.eth.wait_for_transaction_receipt(txh, timeout=120)
        except TimeExhausted:
            pass

        nonce += 1

    return tx_hashes


def _ensure_authorized(update: Update) -> bool:
    if not TELEGRAM_CHAT_ID:
        return True
    chat_id = str(update.effective_chat.id) if update.effective_chat else ""
    return chat_id == str(TELEGRAM_CHAT_ID)


async def _send(update: Update, text: str) -> None:
    if update.effective_message:
        await update.effective_message.reply_text(
            text,
            parse_mode=ParseMode.HTML,
            reply_markup=MARKUP,
            disable_web_page_preview=True,
        )


async def start(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if not _ensure_authorized(update):
        await _send(update, "❌ Unauthorized chat")
        return

    msg = (
        "🤖 <b>Polymarket Control Ready</b>\n\n"
        "Use buttons below:\n"
        "• <b>TRACK</b> → list current open positions\n"
        "• <b>KILL</b> → instant cashout all open positions\n"
        "• <b>CLAIM</b> → claim/redeem settled positions (uses TELEGRAM_CONTROL_RPC_URL)"
    )
    await _send(update, msg)

    # attempt pin in private/group chats where bot has permission
    try:
        if update.effective_chat and update.effective_message:
            await context.bot.pin_chat_message(
                chat_id=update.effective_chat.id,
                message_id=update.effective_message.message_id,
                disable_notification=True,
            )
    except Exception:
        pass


async def track(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if not _ensure_authorized(update):
        await _send(update, "❌ Unauthorized chat")
        return

    wallet = PROXY_WALLET or CLIENT.get_address()

    try:
        positions = await asyncio.to_thread(fetch_open_positions, wallet)
        if not positions:
            await _send(update, "📭 No open positions found.")
            return

        lines = ["📊 <b>Open Positions</b>"]
        for i, p in enumerate(positions, 1):
            lines.append(
                f"{i}. <b>{p.title}</b>\n"
                f"   token: <code>{p.token_id}</code>\n"
                f"   shares: <b>{p.shares:.4f}</b>"
            )
        await _send(update, "\n".join(lines))
    except Exception as exc:
        await _send(update, f"❌ TRACK failed: <code>{exc}</code>")


async def kill(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if not _ensure_authorized(update):
        await _send(update, "❌ Unauthorized chat")
        return

    wallet = PROXY_WALLET or CLIENT.get_address()
    guard = PositionGuard(CLIENT, MIN_SHARES, MAX_SHARES)

    try:
        positions = await asyncio.to_thread(fetch_open_positions, wallet)
        if not positions:
            await _send(update, "📭 No open positions to cashout.")
            return

        lines = ["🧯 <b>KILL started</b> — cashing out all open positions..."]
        for p in positions:
            result = await asyncio.to_thread(
                guard.cashout_market,
                p.token_id,
                p.shares,
                3,
                300,
            )
            status = "✅" if result.ok else "❌"
            lines.append(
                f"{status} <code>{p.token_id}</code> | requested={result.requested_shares:.4f} "
                f"| order={result.order_id or 'n/a'} | error={result.error or '-'}"
            )

        await _send(update, "\n".join(lines))
    except Exception as exc:
        await _send(update, f"❌ KILL failed: <code>{exc}</code>")


async def claim(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if not _ensure_authorized(update):
        await _send(update, "❌ Unauthorized chat")
        return

    wallet = PROXY_WALLET or CLIENT.get_address()

    try:
        tx_hashes = await asyncio.to_thread(
            claim_settled_positions,
            wallet,
            TELEGRAM_CONTROL_RPC_URL,
            PRIVATE_KEY,
        )

        if not tx_hashes:
            await _send(update, "📭 No settled positions found to claim.")
            return

        lines = ["💰 <b>CLAIM submitted</b>"]
        for txh in tx_hashes:
            lines.append(f"✅ tx: <code>{txh}</code>")
        await _send(update, "\n".join(lines))
    except Exception as exc:
        await _send(update, f"❌ CLAIM failed: <code>{exc}</code>")


async def button_router(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    text = (update.effective_message.text or "").strip().upper() if update.effective_message else ""

    if text == "TRACK":
        await track(update, context)
    elif text == "KILL":
        await kill(update, context)
    elif text == "CLAIM":
        await claim(update, context)
    else:
        await _send(update, "Use TRACK, KILL, or CLAIM.")


def main() -> None:
    if not TELEGRAM_BOT_TOKEN:
        raise RuntimeError("TELEGRAM_BOT_TOKEN is required")

    app: Application = ApplicationBuilder().token(TELEGRAM_BOT_TOKEN).build()

    app.add_handler(CommandHandler("start", start))
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, button_router))

    app.run_polling(drop_pending_updates=True)


if __name__ == "__main__":
    main()
