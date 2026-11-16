#!/bin/bash
# This file is executed every time the dev container starts up or resumes.
# It automatically checks if tailscaled, sshd, and socat are running and starts them if not.

echo "INFO: Checking SSH service status..."
if ! pgrep -x sshd >/dev/null; then
    echo "INFO: SSH service not running. Starting it..."
    sudo service ssh restart
fi

echo "INFO: Checking Tailscale status..."
if ! pgrep -x tailscaled >/dev/null; then
    echo "INFO: Tailscale daemon not running. Starting it..."
    sudo start-stop-daemon --start --background --oknodo --pidfile /var/run/tailscaled.pid --make-pidfile --exec /usr/sbin/tailscaled -- --state=/var/lib/tailscale/tailscaled.state
fi

echo "INFO: Checking socat tunnel status..."
if ! pgrep -f 'socat TCP-LISTEN:9222' >/dev/null; then
    echo "INFO: socat tunnel not running. Starting it..."
    sudo start-stop-daemon --start --background --pidfile /var/run/socat-9222.pid --make-pidfile --chuid $(id -un):$(id -gn) --exec /usr/bin/socat -- TCP-LISTEN:9222,fork,bind=127.0.0.1 TCP:host.docker.internal:9222
fi



echo "INFO: Checking goose version..."
if command -v goose >/dev/null 2>&1; then
    # `goose update` downloads the whole release even when the installed build
    # is already current (~55MB, ~10s on every start). Ask GitHub for the
    # latest stable tag and update only when it differs. Any failure here
    # (offline, rate-limited, unparseable, no curl) leaves `latest` empty and
    # falls back to updating, which is the old behaviour.
    installed="$(goose --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
    latest="$(curl -fsSL --max-time 10 https://api.github.com/repos/block/goose/releases/latest 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1)"
    if [ -n "$installed" ] && [ "$installed" = "$latest" ]; then
        echo "INFO: goose $installed is already the latest stable release - skipping update."
    else
        echo "INFO: goose ${installed:-unknown} -> ${latest:-latest} - updating..."
        goose update || echo "WARN: goose update failed, keeping current version"
    fi
else
    echo "WARN: goose not found, skipping update"
fi

echo "INFO: Checking the container agent..."
if [ -x "/workspaces/a2a-goose/scripts/agent-dev.sh" ]; then
    "/workspaces/a2a-goose/scripts/agent-dev.sh" start || true
else
    echo "WARN: scripts/agent-dev.sh not found, skipping the container agent"
fi

echo "INFO: Services check/startup complete."
