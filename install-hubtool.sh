#!/bin/sh
# install-hubtool.sh: connect the Claude Code and Codex sessions on this
# computer to an orgtree mail hub. It installs only hubtool.py (one file,
# Python standard library only), registers it as the "mailhub" MCP server and
# lets sessions use its tools without asking each time. No Orgtree, no hub
# and no sudo are needed; Python 3.8+ is.
#
#   curl -fsSL https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download/install-hubtool.sh | sh
#
# The hub address is asked for. To give it up front, or to uninstall:
#
#   curl -fsSL <the same url> | sh -s -- --hub home-pc:7370
#   curl -fsSL <the same url> | sh -s -- --uninstall
#
# (HUBTOOL_HUB=home-pc:7370 in the environment works too.)
#
# Safe to run again: it replaces hubtool.py in place, keeps the hub you chose
# unless you give another, and registers the server again. What it writes:
#   ~/.orgtree/hubtool/hubtool.py   the program
#   ~/.orgtree/hub-clients/         hubtool's identities and the hub address
#                                   (kept on uninstall)
#   the "mailhub" entry in Claude Code's user MCP config and in Codex's config
#   the pre-approval of the mailhub tools, and of no other: "mcp__mailhub" in
#   permissions.allow of Claude Code's settings.json, and
#   default_tools_approval_mode = "approve" in Codex's [mcp_servers.mailhub]
#
# Everything runs from main() on the last line, so a download cut short
# runs nothing.

set -eu

# The hubtool.py this installer accepts: the SHA-256 of the file released
# beside it. tests/test_install_hubtool.py keeps it equal to the repo's
# hubtool.py; tools/hubtool-assets.py refuses to build a release otherwise.
HUBTOOL_SHA256=5d83fc2a9f57a26e904b51bb6bad828fe558c3c030b4f9d2d4a207c3b461e7b7
SERVER=mailhub

say() { printf '%s\n' "$*"; }
fail() { printf 'install-hubtool: %s\n' "$*" >&2; }

# Python 3.8 or newer, as this user's shell would run it: sets PY, PYVER.
find_python() {
    for name in python3 python; do
        cmd=$(command -v "$name" 2>/dev/null) || continue
        ver=$("$cmd" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null) || continue
        major=${ver%%.*}
        minor=${ver#*.}
        case "$major.$minor" in
            *[!0-9.]*|.*|*.) continue ;;
        esac
        if [ "$major" -eq 3 ] && [ "$minor" -ge 8 ]; then
            PY=$cmd
            PYVER=$ver
            return 0
        fi
    done
    return 1
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        fail "neither curl nor wget is available"
        return 1
    fi
}

# One field of the JSON `hubtool.py defaulthub` printed, '' when absent.
field() {
    printf '%s' "$1" | "$PY" -c 'import json, sys
try:
    v = json.load(sys.stdin).get(sys.argv[1])
except ValueError:
    v = None
print("" if v is None or v is False else ("yes" if v is True else v))' "$2"
}

# Sessions use the mailhub tools without asking each time (the user's
# choice): hubtool.py writes the one setting a client reads for that. Sets
# APPROVAL_ERROR ('' when it worked), APPROVAL_CHANGED (yes or '') and
# APPROVAL_FILE.
approve() {
    res=$("$PY" "$DEST" preapprove "$@" 2>/dev/null) || true
    APPROVAL_ERROR=$(field "$res" error)
    APPROVAL_CHANGED=$(field "$res" changed)
    APPROVAL_FILE=$(field "$res" file)
    if [ -z "$APPROVAL_ERROR" ] && [ -z "$(field "$res" client)" ]; then
        APPROVAL_ERROR="hubtool.py preapprove answered: $res"
    fi
}

uninstall() {
    APPROVAL_ERROR= APPROVAL_CHANGED= APPROVAL_FILE=
    if [ -f "$DEST" ] && find_python; then
        approve claude --remove
    fi
    if command -v claude >/dev/null 2>&1; then
        claude mcp remove -s user "$SERVER" >/dev/null 2>&1 || true
        say "Claude Code: removed the $SERVER MCP server."
    fi
    if [ -n "$APPROVAL_CHANGED" ]; then
        say "Claude Code: removed the pre-approval of the $SERVER tools from $APPROVAL_FILE."
    elif [ -n "$APPROVAL_ERROR" ]; then
        fail "Claude Code: the pre-approval of the $SERVER tools stays: $APPROVAL_ERROR"
    fi
    if command -v codex >/dev/null 2>&1; then
        codex mcp remove "$SERVER" >/dev/null 2>&1 || true
        say "Codex: removed the $SERVER MCP server and the pre-approval of its tools."
    fi
    if [ -d "$DIR" ]; then
        rm -rf "$DIR"
        say "Removed $DIR."
    fi
    say "Kept $HOME/.orgtree/hub-clients: it holds your hub identities (the secrets"
    say "of your addresses) and the hub address. Delete it too if you no longer want them."
}

main() {
    HUB=${HUBTOOL_HUB:-}
    UNINSTALL=
    while [ $# -gt 0 ]; do
        case "$1" in
            --hub) [ $# -ge 2 ] || { fail "--hub needs an address"; exit 2; }
                   HUB=$2; shift ;;
            --hub=*) HUB=${1#--hub=} ;;
            --uninstall) UNINSTALL=1 ;;
            -h|--help) say "usage: install-hubtool.sh [--hub <address>] [--uninstall]"
                       say "  installs hubtool.py in ~/.orgtree/hubtool and registers it as the"
                       say "  \"$SERVER\" MCP server with Claude Code and/or Codex"
                       exit 0 ;;
            *) fail "unknown option: $1 (use --hub <address> or --uninstall)"; exit 2 ;;
        esac
        shift
    done
    [ -n "${HOME:-}" ] || { fail "HOME is not set"; exit 1; }
    BASE=${HUBTOOL_BASE_URL:-https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download}
    BASE=${BASE%/}
    DIR=$HOME/.orgtree/hubtool
    DEST=$DIR/hubtool.py

    if [ -n "$UNINSTALL" ]; then
        uninstall
        exit 0
    fi

    if ! find_python; then
        fail "Python 3.8 or newer is needed and was not found."
        case "$(uname -s 2>/dev/null)" in
            Darwin) say "Install it from https://www.python.org/downloads/ or with Homebrew"
                    say "(brew install python), then run this command again." ;;
            *) say "Install python3 with your package manager (for example"
               say "sudo apt install python3, or sudo dnf install python3), then run"
               say "this command again." ;;
        esac
        exit 1
    fi

    # hubtool.py: downloaded beside its final place, checked, then moved over it.
    mkdir -p "$DIR"
    tmp=$DEST.download
    if ! download "$BASE/hubtool.py" "$tmp"; then
        rm -f "$tmp"
        fail "could not download $BASE/hubtool.py"
        exit 1
    fi
    sha=$("$PY" -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$tmp")
    if [ "$sha" != "$HUBTOOL_SHA256" ]; then
        rm -f "$tmp"
        fail "the downloaded hubtool.py is not the one this installer was released with"
        say "(sha256 $sha, expected $HUBTOOL_SHA256). A new release may have come out a"
        say "moment ago: run the command again. Nothing was changed."
        exit 1
    fi
    mv -f "$tmp" "$DEST"

    # The hub address: --hub, else HUBTOOL_HUB, else ask (the current one is the default).
    suggest=$("$PY" -c 'import sys; sys.path.insert(0, sys.argv[1]); import hubtool; print(hubtool._stored_default_hub() or hubtool._LOCAL_HUB)' "$DIR" 2>/dev/null) \
        || suggest=http://127.0.0.1:7370
    if [ -z "$HUB" ]; then
        if (exec </dev/tty) 2>/dev/null; then
            printf 'Hub address (host, host:port or https://...) [%s]: ' "$suggest" >/dev/tty
            IFS= read -r HUB </dev/tty || HUB=
        else
            say "No hub address given and none can be asked for here; using $suggest."
        fi
        [ -n "$HUB" ] || HUB=$suggest
    fi
    if res=$("$PY" "$DEST" defaulthub "$HUB" 2>/dev/null); then :; else
        why=$(field "$res" error)
        fail "the hub address was not set: ${why:-$res}"
        say "hubtool.py is installed; run this command again with a valid address."
        exit 1
    fi
    hub=$(field "$res" default_hub)
    if [ "$(field "$res" reachable)" = yes ]; then
        name=$(field "$res" hub_name)
        version=$(field "$res" hub_version)
        who=${name}${version:+${name:+, }version $version}
        hubline="$hub (it answers${who:+: $who})"
    else
        hubline="$hub (no answer from it right now; it is used once it answers)"
    fi
    note=$(field "$res" note)

    # The MCP server, registered with whichever client is installed, and its
    # tools pre-approved there.
    lines=
    notes=
    found=
    if command -v claude >/dev/null 2>&1; then
        found=1
        claude mcp remove -s user "$SERVER" >/dev/null 2>&1 || true
        if out=$(claude mcp add -s user "$SERVER" -- "$PY" "$DEST" 2>&1); then
            approve claude
            if [ -z "$APPROVAL_ERROR" ]; then
                lines="$lines  Claude Code  the $SERVER MCP server is registered (user scope), its tools pre-approved
"
            else
                lines="$lines  Claude Code  the $SERVER MCP server is registered (user scope)
"
                notes="$notes  note         Claude Code will ask before each mailhub tool call: $APPROVAL_ERROR
"
            fi
        else
            fail "Claude Code refused the server: $out"
        fi
    fi
    if command -v codex >/dev/null 2>&1; then
        found=1
        codex mcp remove "$SERVER" >/dev/null 2>&1 || true
        if out=$(codex mcp add "$SERVER" -- "$PY" "$DEST" 2>&1); then
            approve codex
            if [ -z "$APPROVAL_ERROR" ]; then
                lines="$lines  Codex        the $SERVER MCP server is registered, its tools pre-approved
"
            else
                lines="$lines  Codex        the $SERVER MCP server is registered
"
                notes="$notes  note         Codex will ask before each mailhub tool call: $APPROVAL_ERROR
"
            fi
        else
            fail "Codex refused the server: $out"
        fi
    fi

    say ""
    say "hubtool is installed."
    say "  hubtool.py   $DEST (Python $PYVER)"
    say "  hub          $hubline"
    [ -z "$note" ] || say "  note         $note"
    printf '%s' "$lines$notes"
    if [ -z "$found" ]; then
        say ""
        say "Neither Claude Code nor Codex was found on PATH. Once one is installed,"
        say "run this command again, or register the server yourself:"
        say "  claude mcp add -s user $SERVER -- \"$PY\" \"$DEST\""
        say "  codex mcp add $SERVER -- \"$PY\" \"$DEST\""
        exit 0
    fi
    say ""
    say "Start a new Claude Code or Codex session: its mailhub tools are there. Ask it"
    say "to join the hub under a name of its own (hub_register), then hub_list,"
    say "hub_send, hub_read and hub_wait. Run this command again to update hubtool.py"
    say "or change the hub. To remove it:"
    say "  curl -fsSL $BASE/install-hubtool.sh | sh -s -- --uninstall"
}

main "$@"
