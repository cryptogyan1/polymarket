#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ ! -f ".venv-executor/bin/activate" ]]; then
  echo "❌ Missing virtualenv activation file: .venv-executor/bin/activate"
  exit 1
fi

source .venv-executor/bin/activate

pids=()

cleanup() {
  local exit_code=$?

  echo
  echo "🛑 Stopping services..."

  for pid in "${pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done

  for pid in "${pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done

  exit "$exit_code"
}

trap cleanup INT TERM EXIT

echo "🚀 Starting executor API..."
uvicorn executor.app:app --host 127.0.0.1 --port 8787 &
pids+=("$!")

echo "🤖 Starting telegram control bot..."
python -m executor.telegram_control_bot &
pids+=("$!")

echo "🦀 Starting Rust bot..."
cargo run &
pids+=("$!")

echo "✅ All three processes started. Press Ctrl+C to stop all."

wait -n "${pids[@]}"
