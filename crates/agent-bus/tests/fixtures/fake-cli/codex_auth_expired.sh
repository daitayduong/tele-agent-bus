#!/usr/bin/env bash
stdin_content=$(cat -)
if [[ -n "$CODEX_HOME" ]]; then
  echo "[config=$CODEX_HOME]"
fi
echo "please sign in first" >&2
exit 1
