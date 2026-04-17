#!/bin/bash
set -e

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo)"
  exit 1
fi

echo "Setting up agent-bus group..."
groupadd -f agent-bus

if [ -n "$SUDO_USER" ]; then
  echo "Adding $SUDO_USER to agent-bus group..."
  usermod -aG agent-bus "$SUDO_USER"
fi

echo "Creating /etc/agent-bus..."
install -d -o root -g agent-bus -m 0750 /etc/agent-bus

# Initialize blacklist if binary is present
if [ -f "/usr/local/bin/agent-bus" ]; then
  echo "Initializing blacklist via binary..."
  /usr/local/bin/agent-bus blacklist init
else
  echo "Binary not found at /usr/local/bin/agent-bus, performing manual init..."
  if [ ! -f "/etc/agent-bus/blacklist.key" ]; then
    dd if=/dev/urandom of=/etc/agent-bus/blacklist.key bs=32 count=1 status=none
    chmod 0640 /etc/agent-bus/blacklist.key
    chown root:agent-bus /etc/agent-bus/blacklist.key
  fi
  
  if [ ! -f "/etc/agent-bus/blacklist.conf" ]; then
    touch /etc/agent-bus/blacklist.conf
    chmod 0640 /etc/agent-bus/blacklist.conf
    chown root:agent-bus /etc/agent-bus/blacklist.conf
    
    # Simple HMAC if binary missing (requires openssl)
    KEY_HEX=$(xxd -p -c 32 /etc/agent-bus/blacklist.key)
    HMAC=$(echo -n "" | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$KEY_HEX" | sed 's/.*= //')
    echo -n "$HMAC" > /etc/agent-bus/blacklist.conf.hmac
    chmod 0640 /etc/agent-bus/blacklist.conf.hmac
    chown root:agent-bus /etc/agent-bus/blacklist.conf.hmac
  fi
fi

echo "Installing systemd user unit..."
mkdir -p /etc/systemd/user
cp scripts/agent-bus.service /etc/systemd/user/agent-bus.service

echo "✓ Installed. Next steps:"
echo "  - Log out and back in for group membership to apply"
echo "  - systemctl --user daemon-reload"
echo "  - systemctl --user enable --now agent-bus"
