#!/usr/bin/env python3
import os
import re

from dotenv import load_dotenv

load_dotenv()

try:
    from eth_account import Account
    from eth_utils import is_address, to_checksum_address
    from py_clob_client.client import ClobClient
except Exception as exc:
    raise SystemExit(f"py-clob-client is required: {exc}")


def sanitize(value: str) -> str:
    return (value or "").strip().strip('"').strip("'")


def normalize_private_key(private_key: str) -> str:
    key = sanitize(private_key)
    if re.fullmatch(r"[0-9a-fA-F]{64}", key):
        return f"0x{key}"
    if re.fullmatch(r"0x[0-9a-fA-F]{64}", key):
        return key
    raise SystemExit("PRIVATE_KEY must be a 32-byte hex string (with or without 0x)")


host = os.getenv("POLY_CLOB_URL", "https://clob.polymarket.com")
chain_id = int(os.getenv("CHAIN_ID", "137"))
private_key = normalize_private_key(os.getenv("PRIVATE_KEY", ""))
proxy_wallet = sanitize(os.getenv("PROXY_WALLET", ""))
signature_type_override = sanitize(os.getenv("POLY_SIGNATURE_TYPE", ""))

signer = Account.from_key(private_key).address
if proxy_wallet and is_address(proxy_wallet):
    funder = to_checksum_address(proxy_wallet)
else:
    funder = signer

if signature_type_override:
    if signature_type_override not in {"0", "1", "2"}:
        raise SystemExit("POLY_SIGNATURE_TYPE must be one of: 0, 1, 2")
    signature_type = int(signature_type_override)
else:
    signature_type = 2 if signer.lower() != funder.lower() else 0

print(
    f"[info] signer={signer} funder={funder} "
    f"signature_type={signature_type} chain_id={chain_id}"
)

client = ClobClient(
    host=host,
    chain_id=chain_id,
    key=private_key,
    funder=funder,
    signature_type=signature_type,
)

creds = None
for fn_name in ("derive_api_key", "create_api_key"):
    fn = getattr(client, fn_name, None)
    if callable(fn):
        try:
            creds = fn()
            print(f"[ok] used {fn_name}")
            break
        except Exception as exc:
            print(f"[warn] {fn_name} failed: {exc}")

if creds is None:
    raise SystemExit("Could not auto-generate API credentials from wallet")

api_key = getattr(creds, "api_key", None) or creds.get("api_key")
api_secret = getattr(creds, "api_secret", None) or creds.get("api_secret")
api_passphrase = getattr(creds, "api_passphrase", None) or creds.get("api_passphrase")

print("\n# Add/update in your .env:")
print(f"POLY_API_KEY={api_key}")
print(f"POLY_API_SECRET={api_secret}")
print(f"POLY_API_PASSPHRASE={api_passphrase}")
print(f"POLY_SIGNATURE_TYPE={signature_type}")
