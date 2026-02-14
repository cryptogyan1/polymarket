#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

echo "[1/5] Building Rust bot"
cargo build

echo "[2/5] Setting default execution mode to executor in .env"
if [[ -f .env ]]; then
  grep -q '^EXECUTION_MODE=' .env && sed -i 's/^EXECUTION_MODE=.*/EXECUTION_MODE=executor/' .env || echo 'EXECUTION_MODE=executor' >> .env
  grep -q '^EXECUTOR_URL=' .env && sed -i 's#^EXECUTOR_URL=.*#EXECUTOR_URL=http://127.0.0.1:8787#' .env || echo 'EXECUTOR_URL=http://127.0.0.1:8787' >> .env
else
  cat > .env <<ENV
EXECUTION_MODE=executor
EXECUTOR_URL=http://127.0.0.1:8787
ENV
fi

echo "[3/5] Creating Python venv for executor"
python3 -m venv .venv-executor
source .venv-executor/bin/activate
pip install --upgrade pip
pip install -r executor/requirements.txt

echo "[4/5] Attempting API credential generation from wallet"
set +e
python scripts/generate_poly_api_creds.py
GEN_RC=$?
set -e
if [[ $GEN_RC -ne 0 ]]; then
  echo "[warn] auto generation failed; set POLY_API_KEY/POLY_API_SECRET/POLY_API_PASSPHRASE manually in .env"
fi

echo "[5/5] Done"
echo "Run executor: source .venv-executor/bin/activate && uvicorn executor.app:app --host 127.0.0.1 --port 8787"
echo "Run bot: cargo run"
