#!/bin/sh
set -eu

INPUT=$(cat)
ENV_FILE="$HOME/.evotai/evot.env"
WEBHOOK="${EVOT_NOTIFY_FEISHU_WEBHOOK:-}"
if [ -z "$WEBHOOK" ] && [ -f "$ENV_FILE" ]; then
  WEBHOOK=$(awk -F= '
    $1 == "EVOT_NOTIFY_FEISHU_WEBHOOK" {
      value = substr($0, index($0, "=") + 1)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
      gsub(/^"|"$/, "", value)
      print value
      exit
    }
  ' "$ENV_FILE")
fi
if [ -z "$WEBHOOK" ]; then
  echo "EVOT_NOTIFY_FEISHU_WEBHOOK is not set" >&2
  exit 1
fi

PAYLOAD=$(EVOT_HOOK_INPUT="$INPUT" python3 - <<'PY'
import json
import os
import socket

raw = os.environ.get("EVOT_HOOK_INPUT", "{}")
data = json.loads(raw)
payload = data.get("payload", {}) or {}
text = (payload.get("text") or "").strip()
if len(text) > 500:
    text = text[:500] + "..."
usage = payload.get("usage", {}) or {}
turn_count = payload.get("turn_count")
duration_ms = payload.get("duration_ms") or 0
seconds = round(duration_ms / 1000, 1)
run_id = data.get("run_id", "")
session_id = data.get("session_id", "")
cwd = data.get("cwd", "")

lines = ["Evot task finished", f"Host: {socket.gethostname()}"]
if cwd:
    lines.append(f"CWD: {cwd}")
if turn_count is not None:
    lines.append(f"Turns: {turn_count}")
lines.append(f"Duration: {seconds}s")
if usage:
    lines.append(
        "Usage: input={input}, output={output}, cache_read={cache_read}, cache_write={cache_write}".format(
            input=usage.get("input", 0),
            output=usage.get("output", 0),
            cache_read=usage.get("cache_read", 0),
            cache_write=usage.get("cache_write", 0),
        )
    )
if run_id:
    lines.append(f"Run: {run_id}")
if session_id:
    lines.append(f"Session: {session_id}")
if text:
    lines.append("")
    lines.append(text)

print(json.dumps({"msg_type": "text", "content": {"text": "\n".join(lines)}}, ensure_ascii=False))
PY
)

curl -fsS -X POST \
  -H 'Content-Type: application/json' \
  -d "$PAYLOAD" \
  "$WEBHOOK" >/dev/null
