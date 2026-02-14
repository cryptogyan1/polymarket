#!/usr/bin/env python3
"""Submit a tiny real-money Polymarket BUY order for connectivity testing.

The script:
1) initializes py-clob-client from .env,
2) discovers an active token from Gamma,
3) places a small marketable LIMIT BUY targeting about $1 notional,
4) auto-retries with detected taker fee bps when required.
"""

import json
import os
import re
from math import ceil
from typing import Any, Dict, Optional, Tuple
from urllib.request import urlopen

from dotenv import load_dotenv

load_dotenv()

from eth_account import Account
from eth_utils import is_address, to_checksum_address
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import ApiCreds, OrderArgs
from py_clob_client.order_builder.constants import BUY


HOST = os.getenv("POLY_CLOB_URL", "https://clob.polymarket.com")
GAMMA_URL = os.getenv("POLY_GAMMA_URL", "https://gamma-api.polymarket.com")
CHAIN_ID = int(os.getenv("CHAIN_ID", "137"))
TARGET_NOTIONAL_USD = float(os.getenv("TEST_TRADE_NOTIONAL_USD", "1.0"))


def sanitize(value: str) -> str:
    return (value or "").strip().strip('"').strip("'")


def normalize_private_key(private_key: str) -> str:
    key = sanitize(private_key)
    if re.fullmatch(r"[0-9a-fA-F]{64}", key):
        return f"0x{key}"
    if re.fullmatch(r"0x[0-9a-fA-F]{64}", key):
        return key
    raise SystemExit("PRIVATE_KEY must be a 32-byte hex string (with or without 0x)")


def resolve_funder(private_key: str, proxy_wallet: str) -> Tuple[str, str]:
    normalized_key = normalize_private_key(private_key)
    proxy_wallet_clean = sanitize(proxy_wallet)
    if proxy_wallet_clean and is_address(proxy_wallet_clean):
        return normalized_key, to_checksum_address(proxy_wallet_clean)
    return normalized_key, Account.from_key(normalized_key).address


def resolve_signature_type(signer: str, funder: str) -> int:
    override = sanitize(os.getenv("POLY_SIGNATURE_TYPE", ""))
    if override:
        if override not in {"0", "1", "2"}:
            raise SystemExit("POLY_SIGNATURE_TYPE must be one of 0,1,2")
        return int(override)
    return 2 if signer.lower() != funder.lower() else 0


def extract_taker_fee_bps(exc: Exception) -> Optional[int]:
    match = re.search(r"current market'?s taker fee:\s*(\d+)", str(exc), re.IGNORECASE)
    return int(match.group(1)) if match else None


def fetch_candidate_token() -> Tuple[str, float, str]:
    url = f"{GAMMA_URL.rstrip('/')}/markets?active=true&closed=false&limit=200"
    with urlopen(url, timeout=15) as resp:
        markets = json.loads(resp.read().decode("utf-8"))

    for market in markets:
        question = market.get("question") or "unknown"
        for token in market.get("tokens", []):
            token_id = token.get("token_id") or token.get("id")
            if not token_id:
                continue

            raw_price = token.get("price") or token.get("lastPrice") or token.get("outcomePrice")
            try:
                px = float(raw_price)
            except Exception:
                continue

            if 0.05 <= px < 0.95:
                # Make order marketable while staying in valid [0,1) range.
                limit_px = min(0.99, max(px + 0.02, 0.1))
                return str(token_id), limit_px, question

    raise SystemExit("No suitable active token found from Gamma")


def build_client() -> ClobClient:
    private_key, funder = resolve_funder(os.getenv("PRIVATE_KEY", ""), os.getenv("PROXY_WALLET", ""))
    signer = Account.from_key(private_key).address
    signature_type = resolve_signature_type(signer, funder)

    client = ClobClient(
        host=HOST,
        chain_id=CHAIN_ID,
        key=private_key,
        funder=funder,
        signature_type=signature_type,
    )

    api_key = sanitize(os.getenv("POLY_API_KEY", ""))
    api_secret = sanitize(os.getenv("POLY_API_SECRET", ""))
    api_passphrase = sanitize(os.getenv("POLY_API_PASSPHRASE", ""))
    if api_key and api_secret and api_passphrase:
        client.set_api_creds(
            ApiCreds(api_key=api_key, api_secret=api_secret, api_passphrase=api_passphrase)
        )
    else:
        creds = client.derive_api_key()
        client.set_api_creds(creds)

    print(
        f"[info] signer={signer} funder={funder} signature_type={signature_type} chain_id={CHAIN_ID}"
    )
    return client


def place_test_order(client: ClobClient) -> Dict[str, Any]:
    token_id, price, question = fetch_candidate_token()
    shares = max(5, ceil(TARGET_NOTIONAL_USD / price))

    print(f"[info] market={question}")
    print(f"[info] token_id={token_id}")
    print(f"[info] price={price:.4f} shares={shares} estimated_notional=${shares*price:.2f}")

    order_args = OrderArgs(token_id=token_id, side=BUY, price=price, size=float(shares))

    try:
        signed = client.create_order(order_args)
        return client.post_order(signed)
    except Exception as exc:
        fee_bps = extract_taker_fee_bps(exc)
        if fee_bps is None:
            raise
        print(f"[warn] detected taker fee {fee_bps} bps; retrying with fee_rate_bps")
        retry_args = OrderArgs(
            token_id=token_id,
            side=BUY,
            price=price,
            size=float(shares),
            fee_rate_bps=fee_bps,
        )
        signed = client.create_order(retry_args)
        return client.post_order(signed)


if __name__ == "__main__":
    clob = build_client()
    result = place_test_order(clob)
    print("[ok] order response:")
    print(json.dumps(result, indent=2, default=str))
