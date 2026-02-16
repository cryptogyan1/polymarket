#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ ! -f ".venv-executor/bin/activate" ]]; then
  echo "❌ Missing virtualenv activation file: .venv-executor/bin/activate"
  exit 1
fi

source .venv-executor/bin/activate

# Run exactly as requested:
# source .venv-executor/bin/activate
# python -m executor.telegram_control_bot
exec python -m executor.telegram_control_bot
