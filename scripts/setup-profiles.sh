#!/usr/bin/env bash
set -euo pipefail

# Welcome header
echo "----------------------------------------------------"
echo "  Agent Bus Profile Setup Wizard"
echo "----------------------------------------------------"
echo "This script will help you register and login to your"
echo "agent profiles (Claude or Codex)."
echo "----------------------------------------------------"

# Check if agent-bus binary is in PATH
if ! command -v agent-bus &> /dev/null; then
    echo "Error: agent-bus binary not found in PATH."
    echo "Please install it first: cargo install --path crates/agent-bus"
    exit 1
fi

while true; do
    # Prompt for agent
    read -p "Enter agent (claude/codex): " agent
    agent=$(echo "$agent" | tr '[:upper:]' '[:lower:]')

    if [[ "$agent" == "gemini" ]]; then
        echo "Error: gemini scope-out 4a"
        continue
    elif [[ "$agent" != "claude" && "$agent" != "codex" ]]; then
        echo "Error: Invalid agent. Please choose 'claude' or 'codex'."
        continue
    fi

    # Prompt for id
    read -p "Enter profile ID (e.g. personal, work): " id
    if [[ ! "$id" =~ ^[a-z][a-z0-9_-]{0,31}$ ]]; then
        echo "Error: Invalid ID. Must match ^[a-z][a-z0-9_-]{0,31}$"
        continue
    fi

    # Optional label
    read -p "Enter label (optional, press Enter to skip): " label

    # Register
    echo "Registering $agent profile '$id'..."
    register_cmd=("agent-bus" "auth" "register" "$agent" "$id")
    if [[ -n "$label" ]]; then
        register_cmd+=("--label" "$label")
    fi

    if ! "${register_cmd[@]}"; then
        echo "Error: Registration failed."
        read -p "Retry? [y/N]: " retry
        if [[ "$retry" =~ ^[yY]$ ]]; then
            continue
        else
            echo "Skipping registration."
        fi
    else
        # Login
        echo "Logging in to $agent profile '$id'..."
        echo "A browser OAuth flow will open. Complete it and then press Enter to continue."
        if agent-bus auth login "$agent" "$id"; then
            read -p "Press Enter to continue..."

            # Recheck
            echo "Confirming login status..."
            if agent-bus auth recheck "$agent" "$id"; then
                echo "Success: $agent profile '$id' is set up."
            else
                echo "Error: Verification failed for $agent profile '$id'."
            fi
        else
            echo "Error: Login failed."
        fi
    fi

    # Add another?
    read -p "Add another profile? [y/N]: " another
    if [[ ! "$another" =~ ^[yY]$ ]]; then
        break
    fi
done

# Final summary
echo "----------------------------------------------------"
echo "  Registered Profiles Summary"
echo "----------------------------------------------------"
agent-bus auth list
