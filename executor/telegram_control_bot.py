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
CLAIM_WALLET = os.getenv("CLAIM_WALLET", "")
CLAIM_PRIVATE_KEY = os.getenv("CLAIM_PRIVATE_KEY", "")
TELEGRAM_CONTROL_RPC_URL = os.getenv("TELEGRAM_CONTROL_RPC_URL", os.getenv("RPC_URL", ""))


def _clean_env(name: str) -> str:
    raw = os.getenv(name, "")
    return raw.strip().strip('"').strip("'")


def _resolve_rpc_url() -> str:
    for key in ["TELEGRAM_CONTROL_RPC_URL", "POLYGON_RPC_URL"]:
        value = _clean_env(key)
        if value:
            return value
    return ""


def _safe_rpc_label(url: str) -> str:
    if not url:
        return "<empty>"
    no_query = url.split("?", 1)[0]
    if "://" not in no_query:
        return no_query
    scheme, rest = no_query.split("://", 1)
    if "/" in rest:
        host, _ = rest.split("/", 1)
    else:
        host = rest
    return f"{scheme}://{host}"

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




def _fetch_positions_rows(wallet: str, extra_params: Optional[Dict[str, str]] = None) -> List[Dict[str, Any]]:
    url = f"{DATA_API_URL.rstrip('/')}/positions"
    params: Dict[str, str] = {"user": wallet, "sizeThreshold": "0"}
    if extra_params:
        params.update(extra_params)

    resp = requests.get(url, params=params, timeout=20)
    resp.raise_for_status()
    data = resp.json()
    if not isinstance(data, list):
        return []
    return [row for row in data if isinstance(row, dict)]


def _extract_claimable_condition_ids(wallet: str) -> List[str]:
    # The data API shape can vary by market state/provider. Query a few safe variants
    # and aggregate claimable/resolved condition IDs.
    variants = [
        {},
        {"limit": "500", "offset": "0"},
        {"redeemable": "true"},
        {"claimable": "true"},
        {"closed": "true", "limit": "500", "offset": "0"},
    ]

    condition_ids: set[str] = set()
    for variant in variants:
        try:
            rows = _fetch_positions_rows(wallet, variant)
        except Exception:
            continue

        for row in rows:
            condition_id = row.get("conditionId") or row.get("condition_id")
            if not condition_id:
                continue

            resolved_or_claimable = bool(
                row.get("resolved")
                or row.get("isResolved")
                or row.get("redeemable")
                or row.get("claimable")
            )
            if not resolved_or_claimable:
                continue

            # Some payloads include explicit "already redeemed" markers.
            already_redeemed = bool(row.get("redeemed") or row.get("isRedeemed"))
            if already_redeemed:
                continue

            shares = _normalize_share(row)
            has_size_signal = shares > 0 or row.get("claimable") or row.get("redeemable")
            if not has_size_signal:
                continue

            condition_ids.add(str(condition_id))

    return sorted(condition_ids)


def fetch_open_positions(wallet: str) -> List[PositionRow]:
    data = _fetch_positions_rows(wallet)

    out: List[PositionRow] = []
    for row in data:
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
    rpc_url = rpc_url.strip().strip('"').strip("'")
    private_key = private_key.strip().strip('"').strip("'")

    if not rpc_url:
        raise RuntimeError(
            "CLAIM missing RPC URL. Set TELEGRAM_CONTROL_RPC_URL (preferred) or POLYGON_RPC_URL."
        )
    if not private_key:
        raise RuntimeError("PRIVATE_KEY is required for CLAIM")

    try:
        signer_addr = Web3().eth.account.from_key(private_key).address
    except Exception as exc:
        raise RuntimeError(f"invalid PRIVATE_KEY for CLAIM: {exc}") from exc

    if wallet and signer_addr.lower() != wallet.lower():
        raise RuntimeError(
            "CLAIM signer does not match wallet holding positions. "
            f"signer={signer_addr} wallet={wallet}. "
            "redeemPositions must be sent by the same address that owns the conditional tokens. "
            "Set CLAIM_PRIVATE_KEY for that wallet (or make CLAIM_WALLET/PROXY_WALLET match signer)."
        )

    positions = fetch_open_positions(wallet)
    settled_conditions = sorted(
        {p.condition_id for p in positions if p.condition_id and p.resolved}
    )

    if not settled_conditions:
        return []

    w3 = Web3(Web3.HTTPProvider(rpc_url, request_kwargs={"timeout": 20}))
    try:
        connected = w3.is_connected()
        chain_id = w3.eth.chain_id if connected else None
    except Exception as exc:
        raise RuntimeError(
            f"failed connecting to RPC ({_safe_rpc_label(rpc_url)}): {exc}"
        ) from exc

    if not connected:
        raise RuntimeError(
            f"failed connecting to RPC ({_safe_rpc_label(rpc_url)}). "
            "Check protocol (https://), API key, and outbound network access."
        )

    if chain_id != 137:
        raise RuntimeError(
            f"RPC chain mismatch for CLAIM: expected Polygon mainnet (137), got {chain_id}."
        )

    acct = w3.eth.account.from_key(private_key)

    # redeemPositions redeems only balances owned by the signing address.
    lookup_wallet = wallet
    if wallet and wallet.lower() != acct.address.lower():
        lookup_wallet = acct.address

    settled_conditions = _extract_claimable_condition_ids(lookup_wallet)
    if not settled_conditions:
        return []

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

    wallet = (_clean_env("CLAIM_WALLET") or CLAIM_WALLET) or (_clean_env("PROXY_WALLET") or PROXY_WALLET) or CLIENT.get_address()
    rpc_url = _resolve_rpc_url() or TELEGRAM_CONTROL_RPC_URL
    private_key = (_clean_env("CLAIM_PRIVATE_KEY") or CLAIM_PRIVATE_KEY) or (_clean_env("PRIVATE_KEY") or PRIVATE_KEY)

    try:
        tx_hashes = await asyncio.to_thread(
            claim_settled_positions,
            wallet,
            rpc_url,
            private_key,
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
