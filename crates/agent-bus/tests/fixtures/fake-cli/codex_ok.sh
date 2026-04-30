#!/usr/bin/env bash
stdin_content=$(cat -)
echo "[args=$*]"
if [[ -n "$CODEX_HOME" ]]; then
  echo "[config=$CODEX_HOME]"
fi
echo "codex-ok: $stdin_content"
exit 0
