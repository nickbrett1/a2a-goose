#!/bin/bash
set -e

# Determine the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The project root directory is one level up from the scripts directory
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Change to the project root directory so that relative paths work correctly
cd "$PROJECT_ROOT"

# Doppler login first: it is the critical path for goose (and the wrangler /
# google-cloud sections below depend on it). Tailscale is optional SSH access
# and its interactive 'tailscale up' prompt must not block the rest.

# Doppler login/setup
if command -v doppler &> /dev/null; then
  if doppler whoami &> /dev/null 2>&1; then
    echo "✅ Already logged in to Doppler."
  else
    echo "INFO: Logging into Doppler (browser flow)..."
    echo "      If a browser does not open, copy the URL and auth code printed above into"
    echo "      your browser to complete the login, then return here."
    if doppler login --no-check-version --yes; then
      echo "✅ Doppler login successful."
      if doppler setup --no-interactive --project a2a-goose --config dev; then
        echo "✅ Doppler project a2a-goose/dev configured."
      else
        echo "WARN: doppler setup failed for a2a-goose/dev - the project may not"
        echo "      exist yet. Create it at https://dashboard.doppler.com, then run:"
        echo "      doppler setup --no-interactive --project a2a-goose --config dev"
      fi
    else
      echo "❌ Doppler login did not complete. Re-run this script (or 'doppler login'),"
      echo "   or authenticate with a service token:  export DOPPLER_TOKEN=dp.st.<token>"
    fi
  fi
else
  echo "⚠️  Doppler CLI not found. Skipping Doppler login - run 'goose' after the"
  echo "    devcontainer post-create setup finishes, or install the CLI manually."
fi

# Tailscale login
if command -v tailscale &> /dev/null; then
  if ! pgrep -x tailscaled > /dev/null; then
    echo "INFO: Starting Tailscale daemon..."
    sudo tailscaled --state=/var/lib/tailscale/tailscaled.state > /dev/null 2>&1 &
    sleep 2
  fi
  if ! sudo tailscale status &> /dev/null; then
    echo "INFO: Logging into Tailscale..."
    sudo tailscale up --hostname=a2a-goose
  else
    echo "✅ Already logged in to Tailscale."
  fi
fi







echo "Cloud login script finished."
