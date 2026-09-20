#!/bin/bash
# This file is executed once per session to set up the devcontainer.
# For example:
# echo "Running devcontainer setup script..."
# npm install

CURRENT_USER=$(whoami)
USER_HOME_DIR="$HOME"

echo "INFO: Restoring or backing up SSH host keys..."
sudo mkdir -p /var/lib/tailscale/ssh
if [ -n "$(ls -A /var/lib/tailscale/ssh/ssh_host_* 2>/dev/null)" ]; then
    echo "INFO: Restoring SSH host keys from /var/lib/tailscale/ssh..."
    sudo cp -f /var/lib/tailscale/ssh/ssh_host_* /etc/ssh/
    sudo chmod 600 /etc/ssh/ssh_host_*_key
    sudo chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null || true
else
    echo "INFO: Backing up SSH host keys to /var/lib/tailscale/ssh..."
    sudo ssh-keygen -A || true
    sudo cp -f /etc/ssh/ssh_host_* /var/lib/tailscale/ssh/
fi

echo "INFO: Ensuring SSH service is running..."
sudo service ssh restart

echo "INFO: Ensuring gemini directory permissions..."
mkdir -p "$USER_HOME_DIR/.gemini"
sudo chown -R "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.gemini"


echo "INFO: Creating Oh My Zsh custom directories..."
mkdir -p "$USER_HOME_DIR/.oh-my-zsh/custom/themes" "$USER_HOME_DIR/.oh-my-zsh/custom/plugins"

if [ -f "/workspaces/a2a-goose/.devcontainer/.zshrc" ]; then
    echo "INFO: Copying .zshrc to $USER_HOME_DIR/.zshrc"
    cp "/workspaces/a2a-goose/.devcontainer/.zshrc" "$USER_HOME_DIR/.zshrc"
    sudo chown "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.zshrc"
else
    echo "INFO: /workspaces/a2a-goose/.devcontainer/.zshrc not found, skipping copy."
fi

if [ -f "/workspaces/a2a-goose/.devcontainer/.p10k.zsh" ]; then
    echo "INFO: Copying .p10k.zsh to $USER_HOME_DIR/.p10k.zsh"
    cp "/workspaces/a2a-goose/.devcontainer/.p10k.zsh" "$USER_HOME_DIR/.p10k.zsh"
    sudo chown "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.p10k.zsh"
else
    echo "INFO: /workspaces/a2a-goose/.devcontainer/.p10k.zsh not found, skipping copy."
fi

if [ -f "/workspaces/a2a-goose/.devcontainer/.tmux.conf" ]; then
    echo "INFO: Copying .tmux.conf to $USER_HOME_DIR/.tmux.conf"
    cp "/workspaces/a2a-goose/.devcontainer/.tmux.conf" "$USER_HOME_DIR/.tmux.conf"
    sudo chown "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.tmux.conf"
else
    echo "INFO: /workspaces/a2a-goose/.devcontainer/.tmux.conf not found, skipping copy."
fi

echo "INFO: Ensuring doppler directory permissions..."
mkdir -p "$USER_HOME_DIR/.doppler"
sudo chown -R "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.doppler"
# Round-5 (memo genproj-fixes-round5): guarantee the CLI is on PATH. The
# Dockerfile installs it for fresh projects, but a regenerated project whose
# Dockerfile was preserved (round-3 idempotent overwrite) needs the fallback.
# (A devcontainer feature was tried first but ghcr.io/devcontainers-contrib
# features are no longer reliably pullable — 'denied'.)
if ! command -v doppler &> /dev/null; then
    echo "INFO: Installing Doppler CLI (fallback)..."
    (curl -Ls --tlsv1.2 --proto "=https" --retry 3 https://cli.doppler.com/install.sh || wget -t 3 -qO- https://cli.doppler.com/install.sh) | sudo sh
fi
# genproj-doppler-context-pin (memo Gi8CN7XqpH6CxFAc2YUJsK): ambient
# DOPPLER_PROJECT/DOPPLER_CONFIG/DOPPLER_ENVIRONMENT from the launching session
# override doppler.yaml (env > yaml) and silently point every 'doppler' command
# at the wrong project. Pin the repo context in ~/.bashrc + ~/.zshrc so new
# shells (including agent-spawned ones) inherit it. The marker keeps the
# append idempotent across post-create re-runs.
DOPPLER_RC_MARKER='# genproj-doppler-context-pin'
if ! grep -qF "$DOPPLER_RC_MARKER" "$HOME/.bashrc" 2>/dev/null; then
    cat >> "$HOME/.bashrc" <<'EOF'
# genproj-doppler-context-pin: this repo's doppler.yaml context wins over ambient env
export DOPPLER_PROJECT=a2a-goose
export DOPPLER_CONFIG=dev
unset DOPPLER_ENVIRONMENT 2>/dev/null || true

EOF
    echo "INFO: Pinned doppler context (a2a-goose/dev) in ~/.bashrc"
fi
if ! grep -qF "$DOPPLER_RC_MARKER" "$HOME/.zshrc" 2>/dev/null; then
    cat >> "$HOME/.zshrc" <<'EOF'
# genproj-doppler-context-pin: this repo's doppler.yaml context wins over ambient env
export DOPPLER_PROJECT=a2a-goose
export DOPPLER_CONFIG=dev
unset DOPPLER_ENVIRONMENT 2>/dev/null || true

EOF
    echo "INFO: Pinned doppler context (a2a-goose/dev) in ~/.zshrc"
fi
# Apply to this shell too, then verify resolution is never silently wrong.
export DOPPLER_PROJECT=a2a-goose
export DOPPLER_CONFIG=dev
unset DOPPLER_ENVIRONMENT 2>/dev/null || true
if command -v doppler &> /dev/null && doppler whoami &> /dev/null 2>&1; then
    RESOLVED_PROJECT="$(doppler run -- printenv DOPPLER_PROJECT 2>/dev/null | tail -n 1)"
    if [ -n "$RESOLVED_PROJECT" ] && [ "$RESOLVED_PROJECT" != "a2a-goose" ]; then
        echo "WARNING: 'doppler run' resolves project '$RESOLVED_PROJECT', but doppler.yaml"
        echo "         declares 'a2a-goose'. An ambient DOPPLER_* export is overriding"
        echo "         the repo context. Run: unset DOPPLER_PROJECT DOPPLER_CONFIG DOPPLER_ENVIRONMENT"
        echo "         then 'doppler setup --no-interactive --project a2a-goose --config dev'."
    elif [ -z "$RESOLVED_PROJECT" ]; then
        echo "WARNING: could not resolve the doppler project via 'doppler run'. If"
        echo "         'doppler projects get a2a-goose' 404s, create it and run"
        echo "         'doppler setup --no-interactive --project a2a-goose --config dev'."
    else
        echo "INFO: doppler context verified: a2a-goose/dev"
    fi
fi



echo "INFO: Installing Cursor CLI..."
curl https://cursor.com/install -fsS | bash







echo "INFO: Configuring git safe directory..."
git config --global --add safe.directory /workspaces/a2a-goose


echo "INFO: Configuring GitHub auth over SSH (no PAT)..."
# genproj-github-auth (SSH-first): GitHub remotes authenticate via an SSH key
# supplied by the host bind-mount (~/.ssh) or the forwarded SSH agent. No PAT
# is ever written to ~/.gitconfig or remote URLs. Defaults to SSH; fails loud
# with guidance if no working key/agent is found. Idempotent: re-runs must not
# duplicate or clobber the existing rewrite.

# --- 1. Make a usable key for the container user ---------------------------
# The host ~/.ssh is bind-mounted at $HOME/.ssh. Those files keep the host uid
# (macOS 501), which OpenSSH (running as the container uid, typically 1000)
# refuses to use. We never chown the mount (that mutates the host file).
# Preferred: forward the SSH agent (zero keys on disk). Fallback: copy the
# mounted key into a container-owned dir with mode 600.
KEY_COPIED=""
if [ -n "${SSH_AUTH_SOCK:-}" ] && command -v ssh-add &> /dev/null && ssh-add -l >/dev/null 2>&1; then
    echo "INFO: GitHub auth via forwarded SSH agent (${SSH_AUTH_SOCK})."
else
    mkdir -p "$HOME/.genproj-ssh" && chmod 700 "$HOME/.genproj-ssh"
    for KEY in "$HOME/.ssh/id_ed25519" "$HOME/.ssh/id_rsa"; do
        if [ -r "$KEY" ]; then
            DEST="$HOME/.genproj-ssh/$(basename "$KEY")"
            cp "$KEY" "$DEST"
            chmod 600 "$DEST"
            KEY_COPIED="$DEST"
            echo "INFO: Copied host-mounted key $KEY into $DEST."
            break
        fi
    done
fi

# --- 2. Point git's ssh at the copied key (if any) -------------------------
# Persisted in ~/.gitconfig (no secret involved), so it survives re-runs.
if [ -n "$KEY_COPIED" ]; then
    git config --global core.sshCommand "ssh -i $KEY_COPIED -o IdentitiesOnly=yes"
fi

# --- 3. Idempotent SSH insteadOf rewrite for github.com ---------------------
if git config --global --get-regexp '^url\.git@github\.com:.*\.insteadof' >/dev/null 2>&1; then
    echo "INFO: GitHub SSH rewrite already configured; leaving in place."
elif ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -T git@github.com 2>&1 | grep -qi "successfully authenticated"; then
    git config --global url."git@github.com:".insteadOf "https://github.com/"
    echo "INFO: GitHub remotes now use SSH (git@github.com:)."
else
    echo "WARN: No working SSH key/agent found for github.com."
    echo "      Add an SSH public key at https://github.com/settings/keys,"
    echo "      load it on the host (ssh-add --apple-use-keychain), and"
    echo "      rebuild/re-run this setup. HTTPS push/pull will use the"
    echo "      default credential helper until then."
fi







echo "INFO: Installing Antigravity CLI and Specify CLI..."
if ! command -v npm &> /dev/null; then
    echo "npm not found. Installing nodejs and npm..."
    sudo apt-get update
    sudo apt-get install -y nodejs npm
fi
sudo npm install -g @specifyapp/cli
curl -fsSL https://antigravity.google/cli/install.sh | bash
echo "INFO: Antigravity CLI and Specify CLI installation complete."

echo "INFO: Initializing Antigravity CLI global settings..."
mkdir -p "$USER_HOME_DIR/.agy"
printf '{\n  "selectedAuthType": "oauth-personal",\n  "general": {\n    "sessionRetention": {\n      "enabled": true,\n      "maxAge": "30d",\n      "warningAcknowledged": true\n    }\n  },\n  "ide": {\n    "hasSeenNudge": true,\n    "enabled": true\n  }\n}\n' > "$USER_HOME_DIR/.agy/settings.json"
sudo chown -R "$CURRENT_USER:$CURRENT_USER" "$USER_HOME_DIR/.agy"

echo "INFO: Setting up goose configuration and MCP servers..."

CONFIG="$HOME/.config/goose/config.yaml"
mkdir -p "$HOME/.config/goose"

# The managed fragments, written once into a scratch file. The merge below
# picks the subset the config is missing. A fragment starts at '  <key>:' —
# exactly two spaces, the child indent under a top-level `extensions:`.
MANAGED_BODY="$(mktemp)"
cat > "$MANAGED_BODY" <<'GOOSECFGBODYEOF'
  mcphub-dev:
    type: streamable_http
    name: mcphub-dev
    enabled: true
    uri: http://nas:8781/mcp/dev
    timeout: 300
GOOSECFGBODYEOF

if [ ! -f "$CONFIG" ]; then
    echo "INFO: No goose config found - writing project goose config (extensions only; provider resolves from Doppler env at runtime)."
    cat > "$CONFIG" <<'GOOSECFGEOF'
extensions:
  mcphub-dev:
    type: streamable_http
    name: mcphub-dev
    enabled: true
    uri: http://nas:8781/mcp/dev
    timeout: 300
GOOSECFGEOF
    echo "INFO: Wrote project goose config (MCPHub dev group + local/remote exceptions)."
else
    MISSING=""
    for KEY in mcphub-dev; do
        grep -q "^[[:space:]]*$KEY:" "$CONFIG" || MISSING="$MISSING $KEY"
    done

    MISSING_BODY="$(mktemp)"
    awk -v missing=" $MISSING " '
        /^  [A-Za-z0-9_-]+:[[:space:]]*$/ {
            k = $0
            sub(/^ +/, "", k)
            sub(/:.*/, "", k)
            keep = (index(missing, " " k " ") > 0)
        }
        keep { print }
    ' "$MANAGED_BODY" > "$MISSING_BODY"

    if [ -z "$MISSING" ] || [ ! -s "$MISSING_BODY" ]; then
        echo "INFO: Project goose extensions already present in $CONFIG - leaving it untouched."
    elif grep -q '^extensions:' "$CONFIG" && ! grep -q '^extensions:[[:space:]]*$' "$CONFIG"; then
        echo "WARN: $CONFIG declares 'extensions:' inline; genproj will not merge into that form."
        echo "WARN: add the entry below by hand:"
        sed 's/^/WARN:   /' "$MISSING_BODY"
    elif ! grep -q '^extensions:[[:space:]]*$' "$CONFIG"; then
        echo "INFO: $CONFIG exists without an extensions section (goose's own default) - adding the project extensions."
        # Nothing to merge with, so a fresh section is safe and idempotent:
        # the next run finds every managed key already present and stops here.
        printf '\n' >> "$CONFIG"
        {
            printf 'extensions:\n'
            cat "$MISSING_BODY"
        } >> "$CONFIG"
        echo "INFO: Added project goose extensions to $CONFIG."
    else
        # Merge under the existing top-level extensions: key. The indentation
        # of its first child tells us whether a 2-space block can be spliced in.
        # No child (empty/EOF) or 0 spaces (null section) are fine — our block
        # becomes the section's content; 4+ spaces would need re-indenting, so
        # that (and only that) is left to the user.
        CHILD_INDENT="$(awk '
            /^extensions:[[:space:]]*$/ { found = 1; next }
            found && (/^[[:space:]]*$/ || /^[[:space:]]*#/) { next }
            found { match($0, /^ */); print RLENGTH; exit }
        ' "$CONFIG")"
        if [ -n "$CHILD_INDENT" ] && [ "$CHILD_INDENT" != "2" ]; then
            echo "WARN: $CONFIG has an extensions: section indented by $CHILD_INDENT spaces (not 2)."
            echo "WARN: add the entry below by hand so the YAML stays valid:"
            sed 's/^/WARN:   /' "$MISSING_BODY"
        else
            MERGED="$(mktemp)"
            awk -v body="$MISSING_BODY" '
                /^extensions:[[:space:]]*$/ && !done {
                    print
                    while ((getline line < body) > 0) print line
                    close(body)
                    done = 1
                    next
                }
                { print }
            ' "$CONFIG" > "$MERGED"
            cat "$MERGED" > "$CONFIG"
            rm -f "$MERGED"
            echo "INFO: Merged project goose extensions into the existing extensions: section of $CONFIG (added:$MISSING)."
        fi
    fi
    rm -f "$MISSING_BODY"
fi
rm -f "$MANAGED_BODY"

echo "INFO: Ensuring goose recipes are available (spec-first development process)..."
RECIPES_DIR="$HOME/.config/goose/recipes"
if [ -d "$RECIPES_DIR/.git" ]; then
    (cd "$RECIPES_DIR" && git pull --ff-only --quiet)         || echo "WARN: Could not update goose-recipes (offline or conflict); keeping existing copy."
else
    mkdir -p "$HOME/.config/goose"
    git clone --quiet https://github.com/nickbrett1/goose-recipes.git "$RECIPES_DIR"         || echo "WARN: Could not clone goose-recipes; recipes will be unavailable."
fi

echo "INFO: goose configuration complete."



if ! pgrep -f "socat TCP-LISTEN:9222" > /dev/null; then
    echo "Setup bridget to access Chrome DevTools Protocol over a secure tunnel..."
    sudo start-stop-daemon --start --background --pidfile /var/run/socat-9222.pid --make-pidfile --chuid $(id -un):$(id -gn) --exec /usr/bin/socat -- TCP-LISTEN:9222,fork,bind=127.0.0.1 TCP:host.docker.internal:9222
fi


echo "INFO: Checking Tailscale status..."
if ! command -v tailscale &> /dev/null; then
    echo "INFO: Installing Tailscale..."
    curl -fsSL https://tailscale.com/install.sh | sh
fi

if ! pgrep -x tailscaled > /dev/null; then
    echo "INFO: Starting Tailscale daemon..."
    sudo start-stop-daemon --start --background --oknodo --pidfile /var/run/tailscaled.pid --make-pidfile --exec /usr/sbin/tailscaled -- --state=/var/lib/tailscale/tailscaled.state
fi

echo -e "\nINFO: Custom container setup script finished."
echo -e "\n⚠️  To complete cloud login, run:"
echo "    cd /workspaces/a2a-goose && bash scripts/cloud_login.sh"
