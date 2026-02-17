import asyncio
import concurrent.futures
import os
import threading
import re
import traceback
from math import floor
import time
from typing import Any, Dict, Optional, Tuple

from dotenv import load_dotenv
from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, Field

from executor.position_guard import PositionGuard
from executor.telegram_notifier import (
    TelegramNotifier,
    TradeResult,
    UnwindInfo,
    leg_info_from_dict,
)

try:
    from eth_account import Account
    from eth_utils import is_address, to_checksum_address
    from py_clob_client.client import ClobClient
    from py_clob_client.clob_types import ApiCreds, MarketOrderArgs, OrderArgs, OrderType
    from py_clob_client.order_builder.constants import BUY, SELL
except Exception as exc:
    raise RuntimeError(
        "py-clob-client import failed. Install dependencies with: pip install -r executor/requirements.txt"
    ) from exc


load_dotenv()

HOST = os.getenv("POLY_CLOB_URL", "https://clob.polymarket.com")
CHAIN_ID = int(os.getenv("CHAIN_ID", "137"))
PRIVATE_KEY = os.getenv("PRIVATE_KEY", "")
PROXY_WALLET = os.getenv("PROXY_WALLET", "")
POLY_API_KEY = os.getenv("POLY_API_KEY", "")
POLY_API_SECRET = os.getenv("POLY_API_SECRET", "")
POLY_API_PASSPHRASE = os.getenv("POLY_API_PASSPHRASE", "")
SIGNATURE_TYPE_RAW = os.getenv("POLY_SIGNATURE_TYPE", "")
FEE_BPS_OVERRIDE = os.getenv("EXECUTOR_FEE_BPS", "").strip()
MIN_SHARES = float(os.getenv("MIN_SHARES", "5"))
MAX_SHARES_RAW = os.getenv("MAX_SHARES", "").strip()
MAX_SHARES = float(MAX_SHARES_RAW) if MAX_SHARES_RAW else None
STRICT_SHARE_BOUNDS = os.getenv("STRICT_SHARE_BOUNDS", "true").strip().lower() == "true"
CREATE_ORDER_RETRY_ATTEMPTS = max(1, int(os.getenv("CREATE_ORDER_RETRY_ATTEMPTS", "2")))
CREATE_ORDER_RETRY_DELAY_MS = max(0, int(os.getenv("CREATE_ORDER_RETRY_DELAY_MS", "250")))

app = FastAPI(title="polymarket-executor", version="1.0.4")


class ExecuteOrderRequest(BaseModel):
    token_id: str
    side: str = Field(pattern="^(BUY|SELL)$")
    price: float = Field(gt=0.0, lt=1.0)
    size_usdc: float = Field(gt=0.0)
    fok: bool = False


class ExecuteOrderResponse(BaseModel):
    ok: bool
    order_id: Optional[str] = None
    error: Optional[str] = None


class CashoutRequest(BaseModel):
    token_id: str
    shares: Optional[float] = Field(default=None, gt=0.0)


class CashoutResponse(BaseModel):
    ok: bool
    token_id: str
    requested_shares: Optional[float] = None
    order_id: Optional[str] = None
    error: Optional[str] = None


class TelegramNotificationRequest(BaseModel):
    type: str
    data: Dict[str, Any]


class TelegramNotificationResponse(BaseModel):
    ok: bool
    error: Optional[str] = None
    message_id: Optional[int] = None


def clamp_order_size(raw_size: float, side: int, price: Optional[float] = None) -> float:
    """Clamp buy size with strict env bounds; keep sell size flexible for emergency exits."""
    size = float(raw_size)

    if side == SELL:
        if size <= 0.0:
            raise ValueError("sell size must be positive")
        return size

    size = float(floor(size))

    if side == BUY and price is not None and price > 0 and MIN_MARKETABLE_BUY_USDC > 0:
        min_notional_shares = float(ceil(MIN_MARKETABLE_BUY_USDC / price))
        if min_notional_shares > size:
            size = min_notional_shares

    if MAX_SHARES is not None:
        if STRICT_SHARE_BOUNDS and abs(MAX_SHARES - MIN_SHARES) < 1e-9:
            if abs(size - MAX_SHARES) > 1e-9:
                raise ValueError(
                    "strict fixed shares enabled for BUY: "
                    f"required exactly={MAX_SHARES:.2f}, "
                    f"but {size:.2f} shares are needed to satisfy minimum notional ${MIN_MARKETABLE_BUY_USDC:.2f}"
                )
            return MAX_SHARES

        size = min(size, MAX_SHARES)

    if size < MIN_SHARES:
        raise ValueError(f"buy size below MIN_SHARES: {size:.2f} < {MIN_SHARES:.2f}")

    return size


def _sanitize(value: str) -> str:
    return value.strip().strip('"').strip("'")


def normalize_private_key(private_key: str) -> str:
    key = _sanitize(private_key)
    if not key:
        raise RuntimeError("PRIVATE_KEY is required")

    if re.fullmatch(r"[0-9a-fA-F]{64}", key):
        return f"0x{key}"

    if re.fullmatch(r"0x[0-9a-fA-F]{64}", key):
        return key

    raise RuntimeError("PRIVATE_KEY must be a 32-byte hex string (with or without 0x)")


def resolve_funder_address(private_key: str, proxy_wallet: str) -> Tuple[str, str]:
    normalized_key = normalize_private_key(private_key)
    proxy_wallet_clean = _sanitize(proxy_wallet)

    if proxy_wallet_clean and is_address(proxy_wallet_clean):
        return normalized_key, to_checksum_address(proxy_wallet_clean)

    derived_address = Account.from_key(normalized_key).address

    if proxy_wallet_clean:
        print(
            f"[executor] warning: PROXY_WALLET '{proxy_wallet_clean}' is invalid. "
            f"It must be an EVM address, not a private key. "
            f"Using derived address {derived_address} from PRIVATE_KEY"
        )
    else:
        print(
            f"[executor] PROXY_WALLET missing; using derived address {derived_address} from PRIVATE_KEY"
        )

    return normalized_key, derived_address


def resolve_signature_type(signer: str, funder: str, raw_override: str) -> int:
    override = _sanitize(raw_override)
    if override:
        if override not in {"0", "1", "2"}:
            raise RuntimeError("POLY_SIGNATURE_TYPE must be one of: 0, 1, 2")
        return int(override)

    return 2 if signer.lower() != funder.lower() else 0


def build_order_args(
    token_id: str,
    side: int,
    price: float,
    size_usdc: float,
    fok: bool = False,
    fee_rate_bps: Optional[int] = None,
) -> OrderArgs:
    payload = {
        "token_id": token_id,
        "price": price,
        "size": size_usdc,
        "side": side,
    }

    if fee_rate_bps is not None:
        payload["fee_rate_bps"] = fee_rate_bps

    return OrderArgs(**payload)


def extract_market_fee_bps(exc: Exception) -> Optional[int]:
    # Example upstream errors:
    # PolyApiException[status_code=400, error_message={'error':
    # "invalid fee rate (0), current market's taker fee: 1000"}]
    # PolyApiException[status_code=400, error_message={'error':
    # "invalid fee rate (0), current market's maker fee: 1000"}]
    message = str(exc)
    match = re.search(r"current market'?s\s+(maker|taker)\s+fee:\s*(\d+)", message, re.IGNORECASE)
    if match:
        return int(match.group(2))
    return None


def resolve_fee_rate_bps(market_fee_bps: Optional[int] = None) -> Optional[int]:
    if FEE_BPS_OVERRIDE:
        try:
            configured = int(FEE_BPS_OVERRIDE)
            if configured < 0:
                return 0
            if market_fee_bps is not None:
                return max(configured, market_fee_bps)
            return configured
        except ValueError:
            print(f"[executor] invalid EXECUTOR_FEE_BPS='{FEE_BPS_OVERRIDE}', ignoring override")

    return market_fee_bps


def is_transient_clob_error(exc: Exception) -> bool:
    message = str(exc).lower()
    return (
        "server disconnected" in message
        or "request exception" in message
        or "remoteprotocolerror" in message
        or "timed out" in message
    )


def create_order_with_retry(order_args: OrderArgs):
    last_exc: Optional[Exception] = None
    for attempt in range(1, CREATE_ORDER_RETRY_ATTEMPTS + 1):
        try:
            return CLIENT.create_order(order_args)
        except Exception as exc:
            last_exc = exc
            if not is_transient_clob_error(exc) or attempt >= CREATE_ORDER_RETRY_ATTEMPTS:
                raise
            wait_s = CREATE_ORDER_RETRY_DELAY_MS / 1000.0
            print(
                f"[executor] transient create_order failure (attempt {attempt}/{CREATE_ORDER_RETRY_ATTEMPTS}): {exc}. "
                f"retrying in {wait_s:.3f}s"
            )
            time.sleep(wait_s)
    if last_exc is not None:
        raise last_exc
    raise RuntimeError("create_order_with_retry exhausted without error or result")

def init_client() -> ClobClient:
    private_key, funder = resolve_funder_address(PRIVATE_KEY, PROXY_WALLET)
    signer = Account.from_key(private_key).address
    signature_type = resolve_signature_type(signer, funder, SIGNATURE_TYPE_RAW)

    client = ClobClient(
        host=HOST,
        chain_id=CHAIN_ID,
        key=private_key,
        funder=funder,
        signature_type=signature_type,
    )
    print(
        f"[executor] configured signer={signer} funder={funder} "
        f"signature_type={signature_type} chain_id={CHAIN_ID}"
    )

    # If explicit API creds are present, use them; otherwise attempt to derive/create.
    if POLY_API_KEY and POLY_API_SECRET and POLY_API_PASSPHRASE:
        client.set_api_creds(
            ApiCreds(
                api_key=POLY_API_KEY,
                api_secret=POLY_API_SECRET,
                api_passphrase=POLY_API_PASSPHRASE,
            )
        )
    else:
        try:
            creds = client.derive_api_key()
            client.set_api_creds(creds)
            print("[executor] derived API credentials from wallet successfully")
        except Exception:
            try:
                creds = client.create_api_key()
                client.set_api_creds(creds)
                print("[executor] created new API credentials from wallet successfully")
            except Exception as exc:
                raise RuntimeError(
                    "Could not derive or create API credentials from wallet. "
                    "Generate API creds once from Polymarket UI or set POLY_API_* vars."
                ) from exc

    return client


class TelegramDispatchRunner:
    def __init__(self) -> None:
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return

        ready = threading.Event()

        def _run_loop() -> None:
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            self._loop = loop
            ready.set()
            loop.run_forever()

            pending = asyncio.all_tasks(loop)
            for task in pending:
                task.cancel()
            if pending:
                loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
            loop.close()

        self._thread = threading.Thread(target=_run_loop, daemon=True)
        self._thread.start()
        ready.wait(timeout=2)

    def submit(self, coro) -> None:
        if self._loop is None:
            self.start()

        if self._loop is None:
            print('[executor] telegram notification failed: dispatcher loop unavailable')
            return

        future = asyncio.run_coroutine_threadsafe(coro, self._loop)

        def _done_callback(done: concurrent.futures.Future) -> None:
            try:
                done.result()
            except Exception as exc:
                print(f"[executor] telegram notification failed: {exc}")

        future.add_done_callback(_done_callback)

    def stop(self) -> None:
        loop = self._loop
        if loop and loop.is_running():
            loop.call_soon_threadsafe(loop.stop)

        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2)

        self._thread = None
        self._loop = None


CLIENT = init_client()
GUARD = PositionGuard(CLIENT, MIN_SHARES, MAX_SHARES)
TELEGRAM_ENABLED = os.getenv("TELEGRAM_ENABLED", "false").strip().lower() == "true"

if TELEGRAM_ENABLED:
    try:
        NOTIFIER = TelegramNotifier()
        TELEGRAM_DISPATCHER = TelegramDispatchRunner()
        print("[executor] Telegram notifications enabled")
    except Exception as exc:
        print(f"[executor] failed to initialize Telegram notifier: {exc}")
        NOTIFIER = None
        TELEGRAM_DISPATCHER = None
else:
    NOTIFIER = None
    TELEGRAM_DISPATCHER = None


def _env_bool_any(keys, default: Optional[bool] = None) -> Optional[bool]:
    for key in keys:
        raw = os.getenv(key)
        if raw is not None:
            return raw.strip().lower() in {"1", "true", "yes", "on"}
    return default


def configured_pair_name() -> str:
    btc_eth_raw = _env_bool_any(["PAIR_BTC_ETH", "BTC_ETH", "BTC-ETH"], None)
    btc_sol_raw = _env_bool_any(["PAIR_BTC_SOL", "BTC_SOL", "BTC-SOL"], None)
    btc_xrp_raw = _env_bool_any(["PAIR_BTC_XRP", "BTC_XRP", "BTC-XRP"], None)

    any_explicit = any(v is not None for v in [btc_eth_raw, btc_sol_raw, btc_xrp_raw])
    btc_eth = btc_eth_raw if btc_eth_raw is not None else (not any_explicit)
    btc_sol = btc_sol_raw or False
    btc_xrp = btc_xrp_raw or False

    if btc_sol and not btc_eth and not btc_xrp:
        return "BTC-SOL"
    if btc_xrp and not btc_eth and not btc_sol:
        return "BTC-XRP"
    return "BTC-ETH"


def send_telegram_notification(coro) -> None:
    if NOTIFIER is None or TELEGRAM_DISPATCHER is None:
        return

    TELEGRAM_DISPATCHER.submit(coro)


@app.on_event("startup")
async def startup_event():
    if NOTIFIER is None:
        return

    if TELEGRAM_DISPATCHER is not None:
        TELEGRAM_DISPATCHER.start()

    try:
        send_telegram_notification(NOTIFIER.send_startup_notification("polymarket-executor"))
    except Exception as exc:
        print(f"[executor] failed to send startup telegram notification: {exc}")


@app.on_event("shutdown")
async def shutdown_event():
    if TELEGRAM_DISPATCHER is not None:
        TELEGRAM_DISPATCHER.stop()


@app.get("/health")
def health():
    return {"ok": True, "mode": "execution-only"}


@app.post("/execute", response_model=ExecuteOrderResponse)
def execute(req: ExecuteOrderRequest):
    try:
        side = BUY if req.side == "BUY" else SELL
        clamped_size = clamp_order_size(req.size_usdc, side, req.price)
        order_args = build_order_args(
            token_id=req.token_id,
            side=side,
            price=req.price,
            size_usdc=clamped_size,
            fok=req.fok,
            fee_rate_bps=resolve_fee_rate_bps(),
        )

        try:
            signed = create_order_with_retry(order_args)
            result = CLIENT.post_order(signed, OrderType.FOK if req.fok else OrderType.GTC)
        except Exception as exc:
            market_fee_bps = extract_market_fee_bps(exc)
            if market_fee_bps is None:
                raise

            effective_fee_bps = resolve_fee_rate_bps(market_fee_bps)
            print(
                f"[executor] detected market fee {market_fee_bps} bps for token {req.token_id}; "
                f"retrying order with fee_rate_bps={effective_fee_bps}"
            )
            retry_args = build_order_args(
                token_id=req.token_id,
                side=side,
                price=req.price,
                size_usdc=clamped_size,
                fok=req.fok,
                fee_rate_bps=effective_fee_bps,
            )
            signed = create_order_with_retry(retry_args)
            result = CLIENT.post_order(signed, OrderType.FOK if req.fok else OrderType.GTC)

        order_id = None
        if isinstance(result, dict):
            order_id = result.get("orderID") or result.get("order_id")

        return ExecuteOrderResponse(ok=True, order_id=order_id)
    except Exception as exc:
        traceback.print_exc()
        raise HTTPException(status_code=400, detail=str(exc))


@app.post("/cashout", response_model=CashoutResponse)
def cashout(req: CashoutRequest):
    """Unwind position using GTC market order (no $1 minimum)."""
    unwind_info = UnwindInfo(
        timestamp=TelegramNotifier.format_timestamp(),
        token=req.token_id,
        shares_to_sell=float(req.shares or 0.0),
        original_cost=0.0,
        original_direction=configured_pair_name(),
    )
    if NOTIFIER:
        send_telegram_notification(NOTIFIER.send_unwind_initiated(unwind_info))

    try:
        result = GUARD.cashout_market(
            token_id=req.token_id,
            shares=req.shares,
            max_retries=3,
            retry_delay_ms=300,
        )

        if result.ok and NOTIFIER:
            unwind_info.order_id = result.order_id
            unwind_info.shares_to_sell = float(result.requested_shares or 0.0)
            unwind_info.status = "completed"
            send_telegram_notification(NOTIFIER.update_unwind_complete(unwind_info))
        elif NOTIFIER and not result.ok:
            unwind_info.shares_to_sell = float(result.requested_shares or 0.0)
            unwind_info.status = "failed"
            send_telegram_notification(
                NOTIFIER.send_unwind_failed(unwind_info, result.error or "unknown unwind error")
            )

        return CashoutResponse(
            ok=result.ok,
            token_id=result.token_id,
            requested_shares=result.requested_shares,
            order_id=result.order_id,
            error=result.error,
        )
    except Exception as exc:
        traceback.print_exc()
        if NOTIFIER:
            send_telegram_notification(NOTIFIER.send_unwind_failed(unwind_info, str(exc)))
        raise HTTPException(status_code=400, detail=str(exc))


@app.post("/notify", response_model=TelegramNotificationResponse)
def notify(req: TelegramNotificationRequest):
    """Forward structured notifications (from Rust or other services) to Telegram."""
    if NOTIFIER is None:
        return TelegramNotificationResponse(ok=False, error="telegram_notifier_disabled")

    try:
        if req.type == "success":
            trade = TradeResult(
                timestamp=str(req.data.get("timestamp", TelegramNotifier.format_timestamp())),
                direction=str(req.data.get("direction", "")),
                leg1=leg_info_from_dict(req.data.get("leg1", {})),
                leg2=leg_info_from_dict(req.data.get("leg2", {})),
                total_cost=float(req.data.get("total_cost", 0.0)),
                combined_price=float(req.data.get("combined_price", 0.0)),
                target_price=float(req.data.get("target_price", 0.0)),
                profit_potential=req.data.get("profit_potential"),
                execution_time_seconds=req.data.get("execution_time_seconds"),
            )
            send_telegram_notification(NOTIFIER.send_both_legs_filled(trade))
            return TelegramNotificationResponse(ok=True)

        if req.type == "partial":
            trade = TradeResult(
                timestamp=str(req.data.get("timestamp", TelegramNotifier.format_timestamp())),
                direction=str(req.data.get("direction", "")),
                leg1=leg_info_from_dict(req.data.get("leg1", {})),
                leg2=leg_info_from_dict(req.data.get("leg2", {})),
                total_cost=float(req.data.get("total_cost", 0.0)),
                combined_price=float(req.data.get("combined_price", 0.0)),
                target_price=float(req.data.get("target_price", 0.0)),
                profit_potential=req.data.get("profit_potential"),
                execution_time_seconds=req.data.get("execution_time_seconds"),
            )
            send_telegram_notification(NOTIFIER.send_one_leg_alert(trade))
            return TelegramNotificationResponse(ok=True)

        if req.type == "unwind_start":
            unwind = UnwindInfo(
                timestamp=str(req.data.get("timestamp", TelegramNotifier.format_timestamp())),
                token=str(req.data.get("token", "")),
                shares_to_sell=float(req.data.get("shares_to_sell", 0.0)),
                original_cost=float(req.data.get("original_cost", 0.0)),
                original_direction=str(req.data.get("original_direction", "")),
                failed_leg=str(req.data.get("failed_leg", "")),
            )
            send_telegram_notification(NOTIFIER.send_unwind_initiated(unwind))
            return TelegramNotificationResponse(ok=True)

        if req.type == "unwind_complete":
            unwind = UnwindInfo(
                timestamp=str(req.data.get("timestamp", TelegramNotifier.format_timestamp())),
                token=str(req.data.get("token", "")),
                shares_to_sell=float(req.data.get("shares_to_sell", 0.0)),
                original_cost=float(req.data.get("original_cost", 0.0)),
                market_price=req.data.get("market_price"),
                received_usdc=req.data.get("received_usdc"),
                order_id=req.data.get("order_id"),
                status="completed",
            )
            send_telegram_notification(NOTIFIER.update_unwind_complete(unwind))
            return TelegramNotificationResponse(ok=True)

        return TelegramNotificationResponse(ok=False, error=f"unknown_notification_type:{req.type}")
    except Exception as exc:
        traceback.print_exc()
        return TelegramNotificationResponse(ok=False, error=str(exc))
