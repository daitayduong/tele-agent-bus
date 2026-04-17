#!/bin/bash
set -e

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo)"
  exit 1
fi

echo "Stopping agent-bus service for all users (if possible)..."
# This is tricky for user units from root, but we try
if [ -n "$SUDO_USER" ]; then
  sudo -u "$SUDO_USER" systemctl --user stop agent-bus || true
fi

echo "Removing systemd unit..."
rm -f /etc/systemd/user/agent-bus.service

echo "Removing /etc/agent-bus..."
# Non-interactive confirmation
read -p "Are you sure you want to remove /etc/agent-bus? [y/N] " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    rm -rf /etc/agent-bus
fi

echo "Removing binaries..."
rm -f /usr/local/bin/agent-bus
rm -f /usr/local/bin/agent-bus-hook

echo "Uninstalled. Note: ~/.agent-bus/ was NOT touched."
