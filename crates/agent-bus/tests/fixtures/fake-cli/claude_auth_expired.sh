#!/usr/bin/env bash
stdin_content=$(cat -)
while [[ $# -gt 0 ]]; do
  if [[ "$1" == "--resume" ]]; then
    echo "[resumed uuid=$2]"
    shift 2
  else
    shift
  fi
done
if [[ -n "$CLAUDE_CONFIG_DIR" ]]; then
  echo "[config=$CLAUDE_CONFIG_DIR]"
fi
echo "Please log in to continue." >&2
exit 1
