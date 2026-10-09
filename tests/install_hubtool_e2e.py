"""End-to-end proof of the one-line hubtool installers, in a THROWAWAY home.

It serves the release assets (hubtool.py and both installers from the working
tree) on a loopback port and runs the installer the way the README does
(`irm | iex` on Windows, `curl | sh` elsewhere), with every per-user path
pointed into a temporary folder: USERPROFILE and HOME, CLAUDE_CONFIG_DIR
(where Claude Code keeps .claude.json, its user MCP servers) and CODEX_HOME
(Codex's config.toml). The real ~/.claude.json and ~/.codex/config.toml are
only read, before and after, to show they gained no mailhub entry.

    python tests/install_hubtool_e2e.py [--hub URL] [--stub-clients]
                                        [--real-sessions [--evidence DIR]] [--keep]

  --hub URL        a SCRATCH hub to register against, never a real one.
                   Without it the asset server answers /healthz and the hub
                   round trips (register, send, listen) are skipped.
  --stub-clients   put recording stand-ins for claude and codex first on PATH,
                   for a machine without them (they log their arguments; the
                   real CLIs are exercised where they are installed)
  --real-sessions  also run one REAL session of each installed CLI (Claude
                   Code on haiku, Codex on gpt-6-luna) that calls
                   hub_register, hub_list, hub_send, hub_wait, hub_read,
                   hub_history and hub_message through the server the
                   installer registered, while a scripted peer answers, with
                   NO approval given on the command line: Claude Code loads
                   the settings.json the installer wrote (--settings, with
                   anything that would ask refused), Codex the mailhub table
                   the installer wrote. A short control session of each,
                   without the installer's pre-approval, shows the same CLI
                   refusing the tool. The sessions use the CLI's own login
                   from the real profile (nothing is read from or written to
                   its credential or MCP config files: the server entry is
                   passed per session, user settings and hooks are not
                   loaded, and nothing is persisted). Spends model usage.
  --evidence DIR   where the sessions' event streams are written
  --keep           leave the throwaway folder for inspection

Standard library only.
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import http.server
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from typing import Any

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]

_REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
WINDOWS = os.name == "nt"
PASS = 0
FAIL: list[tuple[str, str]] = []
NOTES: list[str] = []


def check(label: str, fn) -> Any:
    global PASS
    try:
        out = fn()
    except Exception:                                            # noqa: BLE001
        FAIL.append((label, traceback.format_exc()))
        print(f"  FAIL     {label}", flush=True)
        return None
    PASS += 1
    print(f"  ok {PASS:3d}  {label}", flush=True)
    return out


def sha256_file(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def find_program(name: str, path: str) -> str | None:
    """As the installers look for it: on Windows a real executable or batch
    file, never npm's extensionless shell shim (which CreateProcess cannot
    start)."""
    if WINDOWS:
        for ext in (".exe", ".cmd", ".bat"):
            p = shutil.which(name + ext, path=path)
            if p:
                return p
        return None
    return shutil.which(name, path=path)


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a: Any) -> None:
        pass


# one line per call, each argument ended by a unit separator (0x1f), so an
# argument with a space in it stays one argument. The codex stand-in also
# keeps its [mcp_servers.<name>] table in config.toml, as `codex mcp add` /
# `codex mcp remove` do, for the installer's pre-approval to find.
STUB = r"""#!/bin/sh
{ for a in "$@"; do printf '%s\037' "$a"; done; printf '\n'; } >> "$STUB_LOG_DIR/{name}.calls"
cfg="$CODEX_HOME/config.toml"
if [ "{name}" = codex ] && [ "$1" = mcp ] && [ "$2" = remove ] && [ -f "$cfg" ]; then
    awk -v h="[mcp_servers.$3]" '$0 == h { skip = 1; next } /^\[/ { skip = 0 } !skip' "$cfg" > "$cfg.stub" && mv "$cfg.stub" "$cfg"
fi
if [ "{name}" = codex ] && [ "$1" = mcp ] && [ "$2" = add ]; then
    name=$3; shift 3; [ "$1" = -- ] && shift
    esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
    command=$(esc "$1"); shift
    args=; for a in "$@"; do args="$args${args:+, }\"$(esc "$a")\""; done
    printf '[mcp_servers.%s]\ncommand = "%s"\nargs = [%s]\n' "$name" "$command" "$args" >> "$cfg"
fi
exit 0
"""
US = "\x1f"
# what the throwaway profile holds before the first install: the installer
# must add to it, keep it, and on uninstall give it back
SETTINGS_SEED = {"model": "haiku", "permissions": {"allow": ["Bash(git status)"]}}
CODEX_SEED = '# the user\'s own settings\nmodel = "gpt-6-luna"\n'


class Rig:
    def __init__(self, hub: str | None, stub: bool, keep: bool) -> None:
        # a space in every path below, as in C:\Users\John Doe: quoting is
        # part of what is being proven
        self.tmp = tempfile.mkdtemp(prefix="hubtool install e2e ")
        self.keep = keep
        self.hub = hub
        self.assets = os.path.join(self.tmp, "assets")
        self.home = os.path.join(self.tmp, "home")
        self.claude_dir = os.path.join(self.tmp, "claude-config")
        self.codex_dir = os.path.join(self.tmp, "codex-home")
        self.stub_dir = os.path.join(self.tmp, "stub-bin") if stub else ""
        for d in (self.assets, self.home, self.claude_dir, self.codex_dir):
            os.makedirs(d)
        self.claude_settings = os.path.join(self.claude_dir, "settings.json")
        self.codex_config = os.path.join(self.codex_dir, "config.toml")
        with open(self.claude_settings, "w", encoding="utf-8") as f:
            json.dump(SETTINGS_SEED, f, indent=2)
        with open(self.codex_config, "w", encoding="utf-8", newline="\n") as f:
            f.write(CODEX_SEED)
        for name in ("hubtool.py", "install-hubtool.ps1", "install-hubtool.sh"):
            shutil.copyfile(os.path.join(_REPO, name), os.path.join(self.assets, name))
        with open(os.path.join(self.assets, "healthz"), "w", encoding="utf-8") as f:
            json.dump({"ok": True, "name": "asset-server", "version": "e2e"}, f)
        self.expected_sha = sha256_file(os.path.join(self.assets, "hubtool.py"))
        handler = functools.partial(Quiet, directory=self.assets)
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.hub_url = hub or self.base
        self.dest = os.path.join(self.home, ".orgtree", "hubtool", "hubtool.py")
        self.env = {k: v for k, v in os.environ.items()
                    if k not in ("MAILHUB_URL", "MAILHUB_NAME", "HUBTOOL_HUB")}
        self.env.update(USERPROFILE=self.home, HOME=self.home,
                        CLAUDE_CONFIG_DIR=self.claude_dir, CODEX_HOME=self.codex_dir,
                        HUBTOOL_BASE_URL=self.base, PYTHONIOENCODING="utf-8")
        if stub:
            os.makedirs(self.stub_dir)
            for name in ("claude", "codex"):
                p = os.path.join(self.stub_dir, name)
                with open(p, "w", encoding="utf-8", newline="\n") as f:
                    f.write(STUB.replace("{name}", name))
                os.chmod(p, 0o755)
            self.env["STUB_LOG_DIR"] = self.stub_dir
            self.env["PATH"] = self.stub_dir + os.pathsep + self.env.get("PATH", "")
        self.claude = None if stub else find_program("claude", self.env["PATH"])
        self.codex = None if stub else find_program("codex", self.env["PATH"])

    def close(self) -> None:
        self.server.shutdown()
        if self.keep:
            print(f"kept {self.tmp}")
        else:
            shutil.rmtree(self.tmp, ignore_errors=True)

    # ── running the installer the README's way
    def install(self, *, hub: str | None = None, form: str = "pipe",
                uninstall: bool = False, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
        e = dict(env or self.env)
        if hub and form == "pipe":
            e["HUBTOOL_HUB"] = hub
        if WINDOWS:
            url = f"{self.base}/install-hubtool.ps1"
            if form == "pipe" and not uninstall:
                cmd = f"irm {url} | iex"
            else:
                args = " -Uninstall" if uninstall else (f" -Hub '{hub}'" if hub else "")
                cmd = f"& ([scriptblock]::Create((irm {url}))){args}"
            ps = shutil.which("powershell") or "powershell"
            argv = [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-NonInteractive", "-Command", cmd]
        else:
            url = f"{self.base}/install-hubtool.sh"
            tail = " -s -- --uninstall" if uninstall else (f" -s -- --hub '{hub}'" if hub and form != "pipe" else "")
            argv = ["sh", "-c", f"curl -fsSL {url} | sh{tail}"]
        # POSIX: no controlling terminal, so /dev/tty cannot be opened and a
        # missing address falls back instead of waiting at the prompt
        extra: dict[str, Any] = {} if WINDOWS else {"start_new_session": True}
        r = subprocess.run(argv, env=e, capture_output=True, text=True, encoding="utf-8",
                           errors="replace", stdin=subprocess.DEVNULL, timeout=300, **extra)
        return r

    def hubtool(self, *args: str, env: dict[str, str] | None = None, timeout: float = 60) -> subprocess.CompletedProcess[str]:
        return subprocess.run(self.launch() + list(args), env=env or self.env, capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=timeout)

    # ── what the clients were told
    def claude_entry(self) -> dict[str, Any] | None:
        p = os.path.join(self.claude_dir, ".claude.json")
        if not os.path.exists(p):
            return None
        with open(p, encoding="utf-8") as f:
            return (json.load(f).get("mcpServers") or {}).get("mailhub")

    def codex_entry(self) -> dict[str, Any] | None:
        p = os.path.join(self.codex_dir, "config.toml")
        if not os.path.exists(p):
            return None
        with open(p, encoding="utf-8") as f:
            text = f.read()
        try:
            import tomllib
            return (tomllib.loads(text).get("mcp_servers") or {}).get("mailhub")
        except ModuleNotFoundError:
            m = re.search(r"^\[mcp_servers\.mailhub\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
            if not m:
                return None
            body = m.group(1)
            cmd = re.search(r'^command\s*=\s*"((?:[^"\\]|\\.)*)"', body, re.M)
            args = re.search(r"^args\s*=\s*\[(.*?)\]", body, re.M | re.S)
            mode = re.search(r'^default_tools_approval_mode\s*=\s*"([^"]*)"', body, re.M)
            unq = lambda s: json.loads('"' + s + '"')        # noqa: E731
            out = {"command": unq(cmd.group(1)) if cmd else None,
                   "args": [unq(a) for a in re.findall(r'"((?:[^"\\]|\\.)*)"', args.group(1))] if args else []}
            if mode:
                out["default_tools_approval_mode"] = mode.group(1)
            return out

    def claude_settings_now(self) -> dict[str, Any]:
        with open(self.claude_settings, encoding="utf-8") as f:
            return json.load(f)

    def stub_argv(self, name: str) -> list[list[str]]:
        p = os.path.join(self.stub_dir, f"{name}.calls")
        if not os.path.exists(p):
            return []
        with open(p, encoding="utf-8") as f:
            return [ln.rstrip("\n").rstrip(US).split(US) for ln in f]

    def stub_calls(self, name: str) -> list[str]:
        return [" ".join(a) for a in self.stub_argv(name)]

    def launch(self) -> list[str]:
        """The command the clients were registered with."""
        e = self.claude_entry() or self.codex_entry()
        if e:
            return [str(e["command"])] + [str(a) for a in e.get("args") or []]
        for name in ("claude", "codex"):
            for args in reversed(self.stub_argv(name)):
                if args[:2] == ["mcp", "add"] and "--" in args:
                    return args[args.index("--") + 1:]
        return [sys.executable, self.dest]


def real_configs() -> dict[str, bool]:
    """Whether the REAL user configs carry a mailhub entry or its
    pre-approval (read only)."""
    out = {}
    p = os.path.join(os.path.expanduser("~"), ".claude.json")
    try:
        with open(p, encoding="utf-8") as f:
            out["claude"] = "mailhub" in (json.load(f).get("mcpServers") or {})
    except (OSError, ValueError):
        out["claude"] = False
    p = os.path.join(os.path.expanduser("~"), ".claude", "settings.json")
    try:
        with open(p, encoding="utf-8") as f:
            allow = (json.load(f).get("permissions") or {}).get("allow") or []
        out["claude_settings"] = any("mailhub" in str(r) for r in allow)
    except (OSError, ValueError):
        out["claude_settings"] = False
    p = os.path.join(os.path.expanduser("~"), ".codex", "config.toml")
    try:
        with open(p, encoding="utf-8") as f:
            out["codex"] = re.search(r"^\[mcp_servers\.mailhub\]", f.read(), re.M) is not None
    except OSError:
        out["codex"] = False
    return out


class Mcp:
    """A minimal MCP client over stdio (newline-delimited JSON-RPC)."""

    def __init__(self, argv: list[str], env: dict[str, str]) -> None:
        self.p = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.q: queue.Queue[str] = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.n = 0

    def _pump(self) -> None:
        assert self.p.stdout
        for line in self.p.stdout:
            self.q.put(line)

    def call(self, method: str, params: dict[str, Any] | None = None, timeout: float = 30) -> dict[str, Any]:
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method, "params": params or {}}
        assert self.p.stdin
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        end = time.time() + timeout
        while time.time() < end:
            try:
                r = json.loads(self.q.get(timeout=max(0.1, end - time.time())))
            except queue.Empty:
                break
            if r.get("id") == self.n:
                return r
        raise TimeoutError(f"no answer to {method}")

    def notify(self, method: str) -> None:
        assert self.p.stdin
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")
        self.p.stdin.flush()

    def tool(self, name: str, args: dict[str, Any]) -> dict[str, Any]:
        r = self.call("tools/call", {"name": name, "arguments": args}, timeout=90)
        text = r["result"]["content"][0]["text"]
        return json.loads(text)

    def close(self) -> None:
        self.p.kill()
        self.p.wait(timeout=10)


SESSION_PROMPT = """This is an automated end-to-end test of the `mailhub` MCP tools, a mailbox on a test mail hub. Use only the mailhub tools, one call per step, in this order:
1. hub_register with name "{name}".
2. hub_list.
3. hub_send with to "{peer}" and body "hello from a real {harness} session".
4. hub_wait with timeout 45 (a reply is on its way).
5. hub_read.
6. hub_history with peer "{peer}".
7. hub_message with the id of the first message hub_history listed.
Then answer with one short line per step saying what it returned. Do nothing else.
"""
SESSION_TOOLS = ["hub_register", "hub_list", "hub_send", "hub_wait", "hub_read", "hub_history", "hub_message"]
# the control: the same CLI and server without the installer's pre-approval
CONTROL_PROMPT = """This is an automated test of the `mailhub` MCP tools. Call the mailhub tool hub_list exactly once, then answer with one word: done. Do nothing else.
"""


class Responder:
    """The other side of the conversation: a second identity in the same
    throwaway home, listening through the CLI and answering each hello
    twice: hub_wait takes the first answer, and the second, 1.5 s later,
    is usually there for the session's hub_read."""

    def __init__(self, rig: Rig) -> None:
        self.rig = rig
        r = rig.hubtool("register", "e2e-peer")
        self.slug = json.loads(r.stdout)["slug"]
        self.seen: list[str] = []
        self.answered: list[str] = []
        self.proc = subprocess.Popen(rig.launch() + ["listen", "e2e-peer"], env=rig.env, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self) -> None:
        assert self.proc.stdout
        for ln in self.proc.stdout:
            self.seen.append(ln.rstrip())
            m = re.match(r"\[hub mail from (\S+) at ", ln)
            if m and "hello from a real" in ln and m.group(1) not in self.answered:
                self.answered.append(m.group(1))
                threading.Thread(target=self._answer, args=(m.group(1),), daemon=True).start()

    def _answer(self, who: str) -> None:
        self.rig.hubtool("send", "e2e-peer", who, f"reply 1 from the e2e peer to {who}")
        time.sleep(1.5)
        self.rig.hubtool("send", "e2e-peer", who, f"reply 2 from the e2e peer to {who}")

    def close(self) -> None:
        self.proc.kill()
        self.proc.wait(timeout=10)


def session_env() -> dict[str, str]:
    """The REAL profile, for the CLI's own login (no credential file is read
    or copied here), minus this process's own agent variables; the hub
    session hook in the user's settings stands down for ORGTREE_NODE."""
    env = {k: v for k, v in os.environ.items()
           if not k.upper().startswith(("CLAUDE", "CODEX_HOME", "MAILHUB_", "HUBTOOL_"))}
    env["ORGTREE_NODE"] = "hubtool-install-e2e"
    env["PYTHONIOENCODING"] = "utf-8"
    return env


def tool_calls_claude(lines: list[str]) -> list[tuple[str, str]]:
    """(tool, result text) pairs from Claude Code's stream-json, in order."""
    uses: dict[str, str] = {}
    out: list[tuple[str, str]] = []
    for ln in lines:
        try:
            ev = json.loads(ln)
        except ValueError:
            continue
        for c in (ev.get("message") or {}).get("content") or []:
            if not isinstance(c, dict):
                continue
            if c.get("type") == "tool_use":
                uses[c.get("id")] = str(c.get("name", "")).split("__")[-1]
            elif c.get("type") == "tool_result" and c.get("tool_use_id") in uses:
                body = c.get("content")
                text = body if isinstance(body, str) else " ".join(
                    str(p.get("text", "")) for p in body or [] if isinstance(p, dict))
                out.append((uses[c["tool_use_id"]], text))
    return out


def tool_calls_codex(lines: list[str]) -> list[tuple[str, str]]:
    """(tool, result text) pairs from `codex exec --json`, in order."""
    out: list[tuple[str, str]] = []
    for ln in lines:
        try:
            ev = json.loads(ln)
        except ValueError:
            continue
        item = ev.get("item") or {}
        if ev.get("type") == "item.completed" and item.get("type") == "mcp_tool_call":
            out.append((str(item.get("tool")), json.dumps(item.get("result") or item.get("error"), ensure_ascii=False)))
    return out


def claude_result(lines: list[str]) -> dict[str, Any]:
    """The closing `result` event of Claude Code's stream-json (it lists the
    permission_denials: every tool call that would have asked)."""
    for ln in reversed(lines):
        try:
            ev = json.loads(ln)
        except ValueError:
            continue
        if isinstance(ev, dict) and ev.get("type") == "result":
            return ev
    return {}


def toml_value(v: Any) -> str:
    """A -c override's value: TOML literal strings where they can be (no
    escapes for Windows paths), basic strings otherwise."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, str):
        return "'" + v + "'" if "'" not in v and "\n" not in v else json.dumps(v)
    if isinstance(v, list):
        return "[" + ", ".join(toml_value(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join(f"{json.dumps(k)} = {toml_value(x)}" for k, x in v.items()) + "}"
    return str(v)


def judge_session(harness: str, calls: list[tuple[str, str]], peer: Responder, name: str) -> None:
    names = [t for t, _ in calls]
    it = iter(names)
    assert all(t in it for t in SESSION_TOOLS), f"{harness} called {names}, not {SESSION_TOOLS} in order"
    first = {t: r for t, r in reversed(calls)}
    for t in ("hub_register", "hub_send", "hub_wait", "hub_history"):
        assert '"error"' not in first[t] and "\\\"error\\\"" not in first[t], (t, first[t][:400])
    got = first["hub_wait"] + first["hub_read"]
    assert "reply 1 from the e2e peer" in got, ("no reply reached hub_wait/hub_read", got[:400])
    assert f"hello from a real {harness} session" in first["hub_history"] and "reply 1" in first["hub_history"], \
        ("hub_history did not recall the conversation", first["hub_history"][:600])
    assert f"hello from a real {harness} session" in first["hub_message"] and '"error"' not in first["hub_message"], \
        ("hub_message did not give the first message's whole text", first["hub_message"][:400])
    assert any(s.startswith(name + ".") for s in peer.answered), (peer.answered, peer.seen[-5:])


def real_sessions(rig: Rig, evidence: str) -> None:
    os.makedirs(evidence, exist_ok=True)
    peer = Responder(rig)

    def save(name: str, text: str) -> None:
        with open(os.path.join(evidence, name), "w", encoding="utf-8") as f:
            f.write(text)

    def claude_run(prompt: str, preapproved: bool) -> subprocess.CompletedProcess[str]:
        entry = rig.claude_entry()
        assert entry, "the installer registered nothing with Claude Code"
        cfg = os.path.join(rig.tmp, "claude-session-mcp.json")
        with open(cfg, "w", encoding="utf-8") as f:
            json.dump({"mcpServers": {"mailhub": {
                "type": "stdio", "command": entry["command"], "args": entry.get("args", []),
                # the installer's entry, with hubtool's home pointed at the
                # throwaway one (the session itself runs on the real login)
                "env": {**(entry.get("env") or {}), "USERPROFILE": rig.home, "HOME": rig.home}}}}, f)
        cwd = os.path.join(rig.tmp, "claude-session")
        os.makedirs(cwd, exist_ok=True)
        argv = [rig.claude, "-p", "--model", "haiku", "--setting-sources", "project",
                "--strict-mcp-config", "--mcp-config", cfg,
                # nothing here allows a mailhub tool and nobody can be asked:
                # only the settings.json the installer wrote can let one run
                "--permission-prompts", "none",
                # only the mailhub tools: no shell, no file or web tools
                "--disallowedTools", "Bash,PowerShell,Write,Edit,NotebookEdit,WebFetch,WebSearch,Task,Agent",
                "--no-session-persistence", "--output-format", "stream-json", "--verbose"]
        if preapproved:
            argv += ["--settings", rig.claude_settings]
        return subprocess.run(argv, input=prompt, env=session_env(), cwd=cwd, capture_output=True, text=True,
                              encoding="utf-8", errors="replace", timeout=600)

    def codex_run(prompt: str, preapproved: bool) -> subprocess.CompletedProcess[str]:
        entry = rig.codex_entry()
        assert entry, "the installer registered nothing with Codex"
        # the installed table, key by key (the approval mode the installer
        # wrote among them), with hubtool's home pointed at the throwaway one
        table = dict(entry)
        if not preapproved:
            table.pop("default_tools_approval_mode", None)
        table["env"] = {**(table.get("env") or {}), "USERPROFILE": rig.home, "HOME": rig.home}
        cwd = os.path.join(rig.tmp, "codex-session")
        os.makedirs(cwd, exist_ok=True)
        argv = [rig.codex, "exec", "--json", "--ephemeral", "--ignore-user-config", "--skip-git-repo-check",
                "-m", "gpt-6-luna", "-c", 'model_reasoning_effort="low"', "-s", "read-only"]
        for k, v in table.items():
            argv += ["-c", f"mcp_servers.mailhub.{k}={toml_value(v)}"]
        argv.append("-")
        return subprocess.run(argv, input=prompt, env=session_env(), cwd=cwd, capture_output=True, text=True,
                              encoding="utf-8", errors="replace", timeout=600)

    try:
        if rig.claude and "claude" in SESSIONS:
            def _claude_session():
                name = "e2e-claude-session"
                r = claude_run(SESSION_PROMPT.format(name=name, peer=peer.slug, harness="Claude Code"), True)
                save("claude-code-session.jsonl", r.stdout)
                lines = r.stdout.splitlines()
                calls = tool_calls_claude(lines)
                save("claude-code-tool-calls.json", json.dumps(calls, indent=1, ensure_ascii=False))
                assert r.returncode == 0, (r.returncode, r.stderr[-800:])
                denied = claude_result(lines).get("permission_denials")
                assert denied == [], ("a mailhub tool would have asked", denied)
                judge_session("Claude Code", calls, peer, name)
            check("real session: Claude Code (haiku), allowed only by the settings.json the installer wrote, "
                  "calls hub_register, hub_list, hub_send, hub_wait, hub_read, hub_history and hub_message "
                  "with no approval asked", _claude_session)

            def _claude_control():
                r = claude_run(CONTROL_PROMPT, False)
                save("claude-code-control.jsonl", r.stdout)
                denied = [d.get("tool_name") for d in claude_result(r.stdout.splitlines()).get("permission_denials") or []]
                assert "mcp__mailhub__hub_list" in denied, ("the control was not refused", denied, r.stdout[-800:])
            check("control: the same Claude Code session without that settings.json is refused hub_list "
                  "(it would have asked)", _claude_control)
        if rig.codex and "codex" in SESSIONS:
            def _codex_session():
                name = "e2e-codex-session"
                assert (rig.codex_entry() or {}).get("default_tools_approval_mode") == "approve", rig.codex_entry()
                r = codex_run(SESSION_PROMPT.format(name=name, peer=peer.slug, harness="Codex"), True)
                save("codex-session.jsonl", r.stdout)
                calls = tool_calls_codex(r.stdout.splitlines())
                save("codex-tool-calls.json", json.dumps(calls, indent=1, ensure_ascii=False))
                assert r.returncode == 0, (r.returncode, r.stderr[-800:])
                asked = [t for t, res in calls if "requires approval" in res]
                assert not asked, ("a mailhub tool would have asked", asked)
                judge_session("Codex", calls, peer, name)
            check("real session: Codex (gpt-6-luna), allowed only by the mailhub table the installer wrote, "
                  "calls hub_register, hub_list, hub_send, hub_wait, hub_read, hub_history and hub_message "
                  "with no approval asked", _codex_session)

            def _codex_control():
                r = codex_run(CONTROL_PROMPT, False)
                save("codex-control.jsonl", r.stdout)
                calls = tool_calls_codex(r.stdout.splitlines())
                assert calls and all("requires approval" in res for _, res in calls), \
                    ("the control was not refused", calls, r.stdout[-800:])
            check("control: the same Codex session without the approval mode is refused hub_list "
                  "(it would have asked)", _codex_control)
    finally:
        peer.close()
        with open(os.path.join(evidence, "peer-listener.txt"), "w", encoding="utf-8") as f:
            f.write("\n".join(peer.seen) + "\n")


def run(rig: Rig) -> None:
    hub_norm = rig.hub_url.rstrip("/")
    has_claude = bool(rig.claude or rig.stub_dir)       # the installer will find one
    has_codex = bool(rig.codex or rig.stub_dir)
    before = real_configs()
    print(f"assets {rig.base} · hub {rig.hub_url} · claude {rig.claude or ('stub' if rig.stub_dir else 'absent')}"
          f" · codex {rig.codex or ('stub' if rig.stub_dir else 'absent')}", flush=True)

    def _first_install():
        r = rig.install(hub=rig.hub_url, form="pipe")
        assert r.returncode == 0 and "hubtool is installed" in r.stdout, r.stdout + r.stderr
        assert "it answers" in r.stdout, r.stdout
        if VERBOSE:
            print(r.stdout)
        return r
    first = check("install: the README's one-liner (" + ("irm | iex" if WINDOWS else "curl | sh") +
                  ") installs and says the hub answers", _first_install)

    def _file():
        assert os.path.isfile(rig.dest), rig.dest
        assert sha256_file(rig.dest) == rig.expected_sha
        assert not os.path.exists(rig.dest + ".download")
    check("install: hubtool.py is in ~/.orgtree/hubtool with the released checksum", _file)

    def _stored_hub():
        r = rig.hubtool("defaulthub")
        out = json.loads(r.stdout)
        assert out["default_hub"] == hub_norm and out["source"] == "stored", out
    check("install: the hub address is stored (defaulthub), not in any environment", _stored_hub)

    def _clients():
        launch = rig.launch()
        assert launch[-1] == rig.dest, launch
        if rig.stub_dir:
            for name, add in (("claude", "mcp add -s user mailhub -- "), ("codex", "mcp add mailhub -- ")):
                calls = rig.stub_calls(name)
                assert any(c.startswith(add) and c.endswith(rig.dest) for c in calls), (name, calls)
            return
        assert rig.claude or rig.codex, "neither claude nor codex is installed here: use --stub-clients"
        if rig.claude:
            e = rig.claude_entry()
            assert e and e.get("args", [])[-1] == rig.dest, e
        if rig.codex:
            e = rig.codex_entry()
            assert e and e.get("args", [])[-1] == rig.dest, e
    check("install: Claude Code (user scope) and Codex were given the mailhub server", _clients)

    def _preapproved():
        allow = SETTINGS_SEED["permissions"]["allow"] + (["mcp__mailhub"] if has_claude else [])
        assert rig.claude_settings_now() == {**SETTINGS_SEED, "permissions": {"allow": allow}}, \
            rig.claude_settings_now()
        with open(rig.codex_config, encoding="utf-8") as f:
            text = f.read()
        assert text.startswith(CODEX_SEED), text
        assert text.count("default_tools_approval_mode") == (1 if has_codex else 0), text
        if has_codex:
            e = rig.codex_entry()
            assert e and e.get("default_tools_approval_mode") == "approve", e
            assert "registered, its tools pre-approved" in first.stdout, first.stdout
        if has_claude:
            assert "registered (user scope), its tools pre-approved" in first.stdout, first.stdout
    check("install: the mailhub tools, and no others, are pre-approved: mcp__mailhub added to Claude "
          "Code's settings.json (the rest kept), default_tools_approval_mode in Codex's mailhub table",
          _preapproved)

    def _real_configs_untouched():
        after = real_configs()
        assert after == before and not any(after.values()), (before, after)
    check("install: the REAL ~/.claude.json, ~/.claude/settings.json and ~/.codex/config.toml "
          "gained no mailhub", _real_configs_untouched)

    def _mcp_round_trip():
        env = dict(rig.env)
        m = Mcp(rig.launch(), env)
        try:
            init = m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                         "clientInfo": {"name": "install-e2e", "version": "1"}})
            assert init["result"]["serverInfo"]["name"] == "mailhub", init
            m.notify("notifications/initialized")
            names = [t["name"] for t in m.call("tools/list")["result"]["tools"]]
            for t in ("hub_register", "hub_list", "hub_send", "hub_read", "hub_wait"):
                assert t in names, names
            if rig.hub:
                reg = m.tool("hub_register", {"name": "installer-e2e"})
                assert reg.get("slug", "").startswith("installer-e2e.") and "error" not in reg, reg
                assert reg["hubs"] == {hub_norm: reg["hubs"][hub_norm]} and "error" not in reg["hubs"][hub_norm], reg
                return reg["slug"]
            NOTES.append("no --hub: the MCP server answered initialize and tools/list; "
                         "hub_register was not tried")
            return None
        finally:
            m.close()
    slug = check("mcp: the registered command starts hubtool's MCP server" +
                 (" and hub_register reaches the stored hub" if rig.hub else ""), _mcp_round_trip)

    if rig.claude:
        def _claude_lists_it():
            r = subprocess.run([rig.claude, "mcp", "list"], env=rig.env, capture_output=True, text=True,
                               encoding="utf-8", errors="replace", timeout=120, cwd=rig.home)
            line = next((ln for ln in r.stdout.splitlines() if ln.startswith("mailhub")), "")
            assert "Connected" in line, r.stdout + r.stderr
        check("mcp: `claude mcp list` (Claude Code itself) connects to mailhub", _claude_lists_it)

    if rig.hub and slug:
        def _listener_hears_mail():
            lst = subprocess.Popen(rig.launch() + ["listen", "installer-e2e"], env=rig.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                                   encoding="utf-8")
            q: queue.Queue[str] = queue.Queue()
            threading.Thread(target=lambda: [q.put(ln) for ln in lst.stdout or []], daemon=True).start()
            try:
                time.sleep(3)
                r = rig.hubtool("register", "installer-e2e-peer")
                assert r.returncode == 0, r.stdout + r.stderr
                body = f"hello from the installer e2e {os.getpid()}"
                r = rig.hubtool("send", "installer-e2e-peer", str(slug), body)
                assert r.returncode == 0 and "error" not in r.stdout, r.stdout + r.stderr
                end = time.time() + 30
                seen = []
                while time.time() < end:
                    try:
                        ln = q.get(timeout=1)
                    except queue.Empty:
                        continue
                    seen.append(ln)
                    if body in ln:
                        return
                raise AssertionError(f"the listener printed no mail within 30 s: {seen}")
            finally:
                lst.kill()
                lst.wait(timeout=10)
        check("listen: a listener started by hand (no MAILHUB_URL) hears mail on the stored hub",
              _listener_hears_mail)

    if REAL_SESSIONS:
        if not rig.hub or not (rig.claude or rig.codex):
            NOTES.append("--real-sessions needs --hub and a real claude or codex: skipped")
        else:
            real_sessions(rig, EVIDENCE or os.path.join(rig.tmp, "evidence"))

    def _rerun_keeps_the_hub():
        r = rig.install(form="block")           # no address: the stored one stays
        assert r.returncode == 0 and "hubtool is installed" in r.stdout, r.stdout + r.stderr
        out = json.loads(rig.hubtool("defaulthub").stdout)
        assert out["default_hub"] == hub_norm, out
        if rig.claude:
            with open(os.path.join(rig.claude_dir, ".claude.json"), encoding="utf-8") as f:
                assert list((json.load(f).get("mcpServers") or {}).keys()) == ["mailhub"]
        with open(rig.codex_config, encoding="utf-8") as f:
            text = f.read()
        assert len(re.findall(r"^\[mcp_servers\.mailhub\]", text, re.M)) == int(has_codex), text
        assert text.count("default_tools_approval_mode") == int(has_codex), text
        assert rig.claude_settings_now()["permissions"]["allow"].count("mcp__mailhub") == int(has_claude)
        assert "its tools pre-approved" in r.stdout, r.stdout
        if rig.stub_dir:
            adds = [c for c in rig.stub_calls("claude") if c.startswith("mcp add")]
            removes = [c for c in rig.stub_calls("claude") if c.startswith("mcp remove")]
            assert len(adds) == 2 and len(removes) == 2, rig.stub_calls("claude")
    check("re-run: with no address it keeps the stored hub and leaves ONE mailhub entry and ONE "
          "pre-approval per client", _rerun_keeps_the_hub)

    def _rerun_changes_the_hub():
        r = rig.install(hub="dark-hub.invalid:7399", form="block")
        assert r.returncode == 0 and "no answer from it right now" in r.stdout, r.stdout + r.stderr
        out = json.loads(rig.hubtool("defaulthub").stdout)
        assert out["default_hub"] == "http://dark-hub.invalid:7399", out
        r = rig.install(hub=rig.hub_url, form="block")
        assert r.returncode == 0, r.stdout + r.stderr
        assert json.loads(rig.hubtool("defaulthub").stdout)["default_hub"] == hub_norm
    check("re-run: an address given up front replaces the hub (an unreachable one is kept, with a warning)",
          _rerun_changes_the_hub)

    def _bad_address():
        r = rig.install(hub="ftp://nope", form="block")
        text = r.stdout + r.stderr
        assert "is not a hub address" in text, text
        assert json.loads(rig.hubtool("defaulthub").stdout)["default_hub"] == hub_norm
    check("re-run: a malformed address is refused and the stored hub stays", _bad_address)

    def _tampered_download():
        path = os.path.join(rig.assets, "hubtool.py")
        with open(path, "rb") as f:
            good = f.read()
        try:
            with open(path, "ab") as f:
                f.write(b"\n# tampered\n")
            r = rig.install(hub=rig.hub_url, form="block")
            text = r.stdout + r.stderr
            assert "is not the one this installer was released with" in text, text
            assert sha256_file(rig.dest) == rig.expected_sha, "the installed hubtool.py changed"
            assert not os.path.exists(rig.dest + ".download")
        finally:
            with open(path, "wb") as f:
                f.write(good)
    check("download: a hubtool.py with another checksum is refused and nothing changes", _tampered_download)

    def _no_python():
        env = dict(rig.env)
        if WINDOWS:
            sysroot = os.environ.get("SystemRoot", r"C:\Windows")
            env["PATH"] = os.pathsep.join([os.path.join(sysroot, "System32"),
                                           os.path.join(sysroot, "System32", "WindowsPowerShell", "v1.0")])
        else:
            fake = os.path.join(rig.tmp, "no-python-bin")
            os.makedirs(fake, exist_ok=True)
            for name in ("python3", "python"):
                p = os.path.join(fake, name)
                with open(p, "w", encoding="utf-8", newline="\n") as f:
                    f.write("#!/bin/sh\nexit 1\n")
                os.chmod(p, 0o755)
            env["PATH"] = fake + os.pathsep + env["PATH"]
        stamp = os.path.getmtime(rig.dest)
        r = rig.install(hub=rig.hub_url, form="block", env=env)
        text = r.stdout + r.stderr
        assert "Python 3.8 or newer is needed" in text, text
        assert os.path.getmtime(rig.dest) == stamp, "it touched the install without Python"
    check("python: without Python 3 it says what to install and changes nothing", _no_python)

    def _uninstall():
        r = rig.install(uninstall=True)
        assert r.returncode == 0, r.stdout + r.stderr
        assert not os.path.exists(os.path.dirname(rig.dest)), "the program folder is still there"
        assert os.path.isfile(os.path.join(rig.home, ".orgtree", "hub-clients", "clients.sqlite3")), \
            "uninstall must keep the identities"
        if rig.claude:
            assert rig.claude_entry() is None, rig.claude_entry()
        assert rig.codex_entry() is None, rig.codex_entry()
        if rig.stub_dir:
            assert rig.stub_calls("claude")[-1] == "mcp remove -s user mailhub", rig.stub_calls("claude")
            assert rig.stub_calls("codex")[-1] == "mcp remove mailhub", rig.stub_calls("codex")
        assert rig.claude_settings_now() == SETTINGS_SEED, rig.claude_settings_now()
        if has_claude:
            assert "removed the pre-approval of the mailhub tools" in r.stdout, r.stdout
        with open(rig.codex_config, encoding="utf-8") as f:
            text = f.read()
        assert "default_tools_approval_mode" not in text and "gpt-6-luna" in text, text
        assert real_configs() == before
    check("uninstall: removes the server entries, their pre-approval and ~/.orgtree/hubtool, keeps the "
          "identities and every other setting", _uninstall)


VERBOSE = "-v" in sys.argv
REAL_SESSIONS = False
EVIDENCE = ""
SESSIONS = ("claude", "codex")


def main() -> int:
    global REAL_SESSIONS, EVIDENCE, SESSIONS
    ap = argparse.ArgumentParser()
    ap.add_argument("--hub")
    ap.add_argument("--stub-clients", action="store_true")
    ap.add_argument("--real-sessions", action="store_true",
                    help="also run a real Claude Code and Codex session (needs --hub; spends model usage)")
    ap.add_argument("--evidence", default="", help="folder for the session transcripts")
    ap.add_argument("--sessions", default="claude,codex", help="which real sessions to run")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("-v", action="store_true")
    a = ap.parse_args()
    REAL_SESSIONS, EVIDENCE = a.real_sessions, a.evidence
    SESSIONS = tuple(x.strip() for x in a.sessions.split(","))
    print("orgtree · one-line hubtool installer, end to end (" + ("Windows PowerShell" if WINDOWS else "POSIX sh") + ")")
    rig = Rig(a.hub, a.stub_clients, a.keep)
    try:
        run(rig)
    finally:
        rig.close()
    for n in NOTES:
        print(f"  note     {n}")
    if FAIL:
        for label, tb in FAIL:
            print(f"\n✗ {label}\n{tb}")
        print(f"install e2e: {PASS} passed · {len(FAIL)} FAILED")
        return 1
    print(f"install e2e: all {PASS} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
