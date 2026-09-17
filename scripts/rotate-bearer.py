"""Rotate a host's agent bearer, with a bounded leak scan.

    python3 scripts/rotate-bearer.py     # then SIGTERM the payload yourself

Run as the user that owns ~/.config/a2a-goose/env. It reads nothing outside the
roots below, writes exactly one line of one file, and prints only sha256
prefixes — the token never reaches argv or stdout, because a command line is
world-readable through `ps` (measured on the NAS, 2026-09-17).

The one thing it deliberately does not do is restart the agent: a payload cannot
supervise its own replacement, so `SIGTERM` it and let the wrapper's loop relaunch
the launcher. `RUNBOOK.md` has the sequence, the timing and the traps.
"""

import hashlib
import os
import re
import secrets
import shutil
import subprocess
import time

ENV = os.path.expanduser("~/.config/a2a-goose/env")
ROOTS = [
    # Per-host paths. The docker tree is restricted to config-shaped names at a
    # bounded depth on purpose: a full-content walk of a media tree does not
    # finish in useful time, which is how this script first went wrong.
    (os.path.expanduser("~/.config/a2a-goose"), None),
    (os.path.expanduser("~/a2a-goose"), None),
    # The media tree under the docker dir is enormous and is not where a
    # credential would live: config-shaped files, two levels down, and a
    # deadline. A partial scan is reported as partial.
    (os.environ.get("SCAN_EXTRA_ROOT", "/volumeUSB1/usbshare/docker"), 3),
]
CONFIG_SHAPED = (".yml", ".yaml", ".json", ".conf", ".env", ".sh", ".toml", ".ini")
SCAN_DEADLINE = 60
SKIP_DIRS = {".git", "node_modules", "target", "__pycache__", "venv", ".venv", "site-packages",
             "media", "movies", "tv", "photos", "Music", "downloads"}
SKIP_EXT = {".tar", ".gz", ".tgz", ".zip", ".png", ".jpg", ".jpeg", ".gif", ".mp4", ".mkv",
            ".webp", ".parquet", ".sqlite", ".db", ".bin", ".so", ".pyc", ".log"}
MAX_BYTES = 2 * 1024 * 1024


def fp(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()[:16]


if os.stat(ENV).st_mode & 0o777 != 0o600:
    raise SystemExit("refusing to touch an env file that is not mode 0600")

text = open(ENV).read()
m = re.findall(r"^export A2A_GOOSE_BEARER_TOKEN=(.*)$", text, re.M)
if len(m) != 1:
    raise SystemExit("expected exactly one export A2A_GOOSE_BEARER_TOKEN line, found %d" % len(m))

old_tok = m[0].strip().strip('"').strip("'")
print("old fp:", fp(old_tok), "| length:", len(old_tok), flush=True)

backup = ENV + ".bak-rot2-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
shutil.copy2(ENV, backup)
os.chmod(backup, 0o600)
print("backup:", backup)

# --- leak scan: filenames only, never the value ---------------------------------
hits, scanned, partial = [], 0, []
started = time.time()
for root, max_depth in ROOTS:
    if not os.path.isdir(root):
        print("scan: missing root", root, flush=True)
        continue
    if time.time() - started > SCAN_DEADLINE:
        partial.append(root)
        continue
    root_depth = root.rstrip("/").count("/")
    for dirpath, dirnames, filenames in os.walk(root, onerror=lambda e: None):
        if time.time() - started > SCAN_DEADLINE:
            partial.append(root)
            break
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        if max_depth is not None and dirpath.rstrip("/").count("/") - root_depth >= max_depth:
            dirnames[:] = []
        for name in filenames:
            path = os.path.join(dirpath, name)
            ext = os.path.splitext(name)[1].lower()
            if ext in SKIP_EXT:
                continue
            if max_depth is not None and ext not in CONFIG_SHAPED and not name.startswith(".env"):
                continue
            try:
                if os.path.getsize(path) > MAX_BYTES or os.path.islink(path):
                    continue
                with open(path, "rb") as fh:
                    blob = fh.read()
            except OSError:
                continue
            scanned += 1
            if old_tok.encode() in blob:
                hits.append(path)
print("scan: %d files read across %d roots | partial: %s" % (scanned, len(ROOTS), partial or "no"), flush=True)
for h in sorted(hits):
    print("  holds the old token:", h)

expected = {ENV, backup}
unexpected = [h for h in hits if h not in expected and not h.startswith(ENV + ".bak")]
print("unexpected holders:", len(unexpected))

# --- rotate --------------------------------------------------------------------
new_tok = secrets.token_hex(32)
if len(new_tok) != 64:
    raise SystemExit("generated token is not 64 hex chars")
new_text, n = re.subn(r"^export A2A_GOOSE_BEARER_TOKEN=.*$",
                      "export A2A_GOOSE_BEARER_TOKEN=" + new_tok,
                      text, count=1, flags=re.M)
if n != 1 or new_text == text or new_text.count("\n") != text.count("\n"):
    raise SystemExit("in-place replacement did not do what it should")
with open(ENV, "w") as fh:
    fh.write(new_text)
os.chmod(ENV, 0o600)

print("new fp:", fp(new_tok), "| length:", len(new_tok))
print("mode:", oct(os.stat(ENV).st_mode & 0o777))
print("bash -n:", subprocess.run(["bash", "-n", ENV]).returncode == 0)
src = subprocess.run(["bash", "-c", "set -a; . %s; set +a; test -n \"$A2A_GOOSE_BEARER_TOKEN\"" % ENV])
print("sources with the token set:", src.returncode == 0)
print("old fp still in file:", old_tok in open(ENV).read())
