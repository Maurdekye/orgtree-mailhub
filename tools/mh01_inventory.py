"""Offline source inventory. Never imports product modules or opens their stores."""
from __future__ import annotations
import argparse
import ast
import contextlib
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import subprocess

# The repository this census is ABOUT. Defaults to the checkout the script sits
# in; --repo-root/--inventory let the same probe run from elsewhere, so the
# replay recipe is portable instead of depending on where the file lives.
ROOT = Path(__file__).resolve().parents[1]
BASE = "6e856eec8ccfbcf8d16451123903b9d2ce16450b"
OUTPUT = "docs/mh01-source-inventory.json"
MH01_ADDITIONS = {OUTPUT, "docs/mh01-contract.md", "tools/mh01_inventory.py",
                  "tests/mh01_contract.py", "tests/test_mh01_inventory.py",
                  "tests/fixtures/mh01-wire.json"}

# MH02 protocol preparation: an isolated Rust library for the MCP stdio
# envelope, its neutral profile and its shared runner. None of these is product
# runtime source -- there is no launcher, listener, packaging or workspace entry
# for any of them -- so they are registered here as artifacts rather than being
# added to the censused source denominator.
#
# EVERY PATH IS SPELLED OUT, ON PURPOSE. A `native/` prefix rule would admit any
# future file under that directory without a reviewer ever seeing it, and the
# whole value of this register is that an unregistered source file is REFUSED.
# Adding a file to that crate means adding its exact path here, which is a
# reviewable line in a diff.
MH02_ADDITIONS = {
    "native/mailhub-protocol/Cargo.toml",
    "native/mailhub-protocol/Cargo.lock",
    "native/mailhub-protocol/.gitignore",
    "native/mailhub-protocol/src/lib.rs",
    "native/mailhub-protocol/src/envelope.rs",
    "native/mailhub-protocol/tests/envelope_contract.rs",
    "native/mailhub-protocol/examples/envelope_probe.rs",
    "tests/fixtures/mh02-mcp-envelope.json",
    "tests/mh02_mcp_contract.py",
    "docs/mh02-protocol-prep.md",
}

ADDITIONS = MH01_ADDITIONS | MH02_ADDITIONS

# Files that existed at BASE and have been changed since, ON PURPOSE, each
# pinned to the EXACT bytes its change was reviewed with. MH01 and MH02 only
# ever ADDED files; this is the register for the other case, and until f2
# there was none, so a deliberate one-line fix to a censused file was
# indistinguishable from drift.
#
# Same spelled-out discipline as ADDITIONS, one turn stricter: a path here
# does not become free to drift, it becomes free to hold ONE NAMED CONTENT.
# An extra line, a different pattern, a reordering -- anything the register
# does not name hashes to something else and is refused exactly as before,
# and a file nobody registered is refused whatever it now contains.
#
# The substitution replaces a file's hash and size and NOTHING else.
# Everything build() reads OUT of a file -- routes, refusals, schemas, SQL,
# environment reads -- is still extracted from the bytes in hand and still
# has to match the freeze, so registering a path cannot wave through a
# change in product behaviour. This is for files the census weighs but
# reads nothing out of.
MODIFICATIONS = {
    # f2. compose.yaml builds with `build: .`, so the whole worktree is the
    # Docker build context. The MH02 crate's .gitignore keeps its target/
    # out of Git, but .dockerignore is a separate file with separate
    # effect and named no part of native/, so a checkout where the crate
    # had been built sent that directory to the daemon: 204.08 MB against
    # 160.25 kB with the pattern in place, by BuildKit's own figure.
    # `native/**/target/` excludes generated output at any depth beneath
    # native/ and no committed source. Image contents are unchanged -- the
    # Dockerfile copies only requirements.txt and mailhub/, and both
    # context variants export a byte-identical tree for those paths.
    # Known and accepted: Docker cleans the trailing slash, so the pattern
    # would also exclude a FILE named exactly `target` beneath native/.
    # `.dockerignore` has no directory-only syntax and no product path is
    # named that.
    ".dockerignore": {
        "sha256": "236948072ffe36cd558b04e8761a429e395db6eced6bef1be79b19b8030f2577",
        "bytes": 149,
    },
}

def git(*args):
    return subprocess.check_output(["git", "-C", str(ROOT), *args])

def source_files():
    names = git("ls-tree", "-r", "--name-only", BASE).decode().splitlines()
    return {name: git("show", BASE + ":" + name) for name in names}

def family(path):
    if path.startswith("tests/"): return "legacy-characterization-tests"
    if path in {"hubtool.py", "install-hook.py", "session-start.sh"}: return "chat-client-and-onboarding"
    if path.startswith("mailhub/"): return "hub-runtime-or-operator-view"
    if path.startswith("docs/") or path == "README.md": return "normative-or-operational-documentation"
    if path in {"Dockerfile", "compose.yaml", ".env.example", "requirements.txt", "expose-hub.ps1", "tools/verify-docker.py"}: return "packaging-configuration-and-exposure"
    return "repository-metadata"

def comparisons(tree, variable):
    out = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Compare) and isinstance(node.left, ast.Name) and node.left.id == variable:
            for expr in node.comparators:
                try: value = ast.literal_eval(expr)
                except (ValueError, TypeError): continue
                for v in value if isinstance(value, (tuple, list)) else [value]:
                    if isinstance(v, str): out.append({"value": v, "line": node.lineno})
    return sorted(out, key=lambda row: (row["line"], row["value"]))

# ------------------------------------------------ per-route wire obligations
# The census above says a route EXISTS. MH01 also has to freeze what each one
# demands and answers, because that -- not the decorator -- is what a Rust port
# must reproduce. Everything here is read out of the pinned handler body, so a
# source change moves the frozen record instead of silently disagreeing with it.

def _handlers(tree):
    return {n.name: n for n in ast.walk(tree)
            if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))}

def _raised_statuses(fn):
    """Explicit HTTPException(<status>, ...) codes raised in this handler."""
    out = set()
    for n in ast.walk(fn):
        if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) \
                and n.func.id == "HTTPException" and n.args \
                and isinstance(n.args[0], ast.Constant) and isinstance(n.args[0].value, int):
            out.add(n.args[0].value)
    return sorted(out)

def _detail_of(call):
    """The `detail` a raise site sends, as a frozen shape rather than a string.

    A status code alone does not pin a refusal: reviewer finding F1 showed an
    implementation that answers every refusal with the same wrong sentence still
    satisfying a status-only profile. The detail is the rest of the envelope, so
    it is extracted here from the pinned AST and asserted by the wire profile.

    Three kinds, because the source really does have three. `literal` is one
    constant. `concatenation` is adjacent constants the parser folded or an
    explicit `+` of them -- still fully known statically. `template` is an
    f-string, where the constant runs are contractual and the interpolations are
    per-request values a profile must not hard-code; those become `{}` holes and
    the driver matches the surrounding literal text instead of the whole string.
    """
    if len(call.args) < 2:
        return None
    node = call.args[1]
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return {"kind": "literal", "text": node.value, "parts": [node.value]}
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        parts, stack = [], [node]
        while stack:                       # left-to-right flatten of a `+` chain
            cur = stack.pop()
            if isinstance(cur, ast.BinOp) and isinstance(cur.op, ast.Add):
                stack.extend([cur.right, cur.left])
            elif isinstance(cur, ast.Constant) and isinstance(cur.value, str):
                parts.append(cur.value)
            else:
                return {"kind": "dynamic", "text": None, "parts": []}
        return {"kind": "concatenation", "text": "".join(parts), "parts": ["".join(parts)]}
    if isinstance(node, ast.JoinedStr):
        parts, template = [], ""
        for v in node.values:
            if isinstance(v, ast.Constant) and isinstance(v.value, str):
                parts.append(v.value)
                template += v.value
            else:
                template += "{}"
        return {"kind": "template", "text": template,
                "parts": [p for p in parts if p.strip()]}
    return {"kind": "dynamic", "text": None, "parts": []}

def _refusals(fn):
    """Every explicit refusal in this handler: status AND detail envelope."""
    out = []
    for n in ast.walk(fn):
        if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) \
                and n.func.id == "HTTPException" and n.args \
                and isinstance(n.args[0], ast.Constant) and isinstance(n.args[0].value, int):
            out.append({"status": n.args[0].value, "line": n.lineno,
                        "detail": _detail_of(n)})
    return sorted(out, key=lambda r: (r["status"], r["line"]))

def _calls_named(fn, name):
    return any(isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id == name
               for n in ast.walk(fn))

def _returned_keys(fn):
    """Top-level literal keys of every dict this handler returns."""
    out = set()
    for n in ast.walk(fn):
        if isinstance(n, ast.Return) and isinstance(n.value, ast.Dict):
            for k in n.value.keys:
                if isinstance(k, ast.Constant) and isinstance(k.value, str):
                    out.add(k.value)
    return sorted(out)

def _reads_auth_header(fn):
    """Does this handler parse the X-Org-Auth header itself?"""
    return any(isinstance(n, ast.Constant) and isinstance(n.value, str)
               and n.value.lower() == "x-org-auth" for n in ast.walk(fn))

def _query_params(fn):
    """Handler arguments other than the Request -- FastAPI serves these as query params."""
    out = []
    for a in fn.args.args:
        ann = ast.unparse(a.annotation) if a.annotation else None
        if a.arg in {"request", "self"} or ann == "Request": continue
        out.append({"name": a.arg, "annotation": ann})
    return out

def http_contract(files, registrations):
    tree = ast.parse(files["mailhub/app.py"].decode("utf-8"))
    handlers = _handlers(tree)
    rows = []
    for reg in registrations:
        if reg["kind"] not in {"get", "post"}: continue
        fn = handlers[reg["handler"]]
        rows.append({
            "method": reg["kind"].upper(),
            "path": reg["argument"],
            "handler": reg["handler"],
            "line": reg["line"],
            # Say exactly what was measured. `_auth` is the shared credential
            # gate, but /api/register deliberately does NOT use it: it parses
            # X-Org-Auth inline to find the secret for the one slug being
            # registered. Calling that route "unauthenticated" because it
            # misses the helper would be wrong, and calling the operator UI
            # authenticated because it is on the same app would be worse --
            # the public/operator split in public.py rests on this column.
            "credential_check": ("auth-helper" if _calls_named(fn, "_auth")
                                 else "inline-header" if _reads_auth_header(fn)
                                 else "none"),
            "refusal_statuses": _raised_statuses(fn),
            "refusals": _refusals(fn),
            "response_keys": _returned_keys(fn),
            "query_params": _query_params(fn),
        })
    return sorted(rows, key=lambda r: (r["path"], r["method"]))

# ------------------------------------ interpreter and framework dependencies
# R10 must prove the packaged backend runs with no usable Python. That proof is
# only as good as the list of places an interpreter is required today, so MH01
# freezes the list here and refuses to let an entry go missing quietly.

_PY_WITNESS = re.compile(r"\bpython[0-9.]*\b|\bpip\b|sys\.executable|\buvicorn\b|\bfastapi\b",
                         re.IGNORECASE)

# Families whose interpreter use is a BACKEND obligation. Documentation and the
# legacy characterization suite sit outside it deliberately: the docket rules
# that Python used by test tooling is not reclassified as an Orgtree backend
# dependency. Their witnesses are still censused, just not required to carry a
# role.
_BACKEND_FAMILIES = {"hub-runtime-or-operator-view", "chat-client-and-onboarding",
                     "packaging-configuration-and-exposure"}

# Each role is anchored by an exact substring of the pinned source, and the
# anchor is RESOLVED, not trusted: a marker matching nothing aborts the build,
# so this table cannot rot into a description of source that no longer exists.
PYTHON_ROLES = [
    {"role": "hub-service-entrypoint", "path": "Dockerfile",
     "marker": 'CMD ["python", "-m", "mailhub.serve"]',
     "disposition": "must-be-replaced",
     "obligation": "The packaged hub service must start from a native executable, not an interpreter module launch."},
    {"role": "hub-base-interpreter-image", "path": "Dockerfile",
     "marker": "FROM python:3.12-slim",
     "disposition": "must-be-replaced",
     "obligation": "The image must stop being an interpreter distribution; a Python base image leaves a usable Python inside the delivered backend, which is the precise thing R10 tests for."},
    {"role": "hub-dependency-install", "path": "Dockerfile",
     "marker": "RUN pip install --no-cache-dir",
     "disposition": "must-be-removed",
     "obligation": "No package-manager install step may remain in the hub image build."},
    {"role": "hub-dependency-manifest", "path": "requirements.txt",
     "marker": "",
     "disposition": "must-be-replaced",
     "obligation": "fastapi/uvicorn become Rust crate dependencies; the manifest must not survive as a runtime requirement."},
    {"role": "hub-asgi-framework", "path": "mailhub/app.py",
     "marker": "fastapi",
     "disposition": "must-be-replaced",
     "obligation": "Routing, query validation and the HTTPException status surface are FastAPI behaviour today; the port must reproduce the frozen statuses without it."},
    {"role": "hub-asgi-server", "path": "mailhub/serve.py",
     "marker": "uvicorn",
     "disposition": "must-be-replaced",
     "obligation": "Both listeners -- the full app and the FR-10 public split -- must be served by the Rust binary."},
    {"role": "hub-service-entrypoint-doc", "path": "mailhub/serve.py",
     "marker": "python -m mailhub.serve",
     "disposition": "must-be-replaced",
     "obligation": "The documented start command changes with the entrypoint; leaving it stale misdirects operators and R10 tracing."},
    {"role": "hub-container-healthcheck", "path": "compose.yaml",
     "marker": '"CMD", "python", "-c"',
     "disposition": "must-be-replaced",
     "obligation": "A SECOND, easily-missed interpreter launch: the 30s container healthcheck shells out to Python. Replacing the service binary and stripping Python from the image without replacing this turns every healthy hub unhealthy."},
    {"role": "chat-client-cli", "path": "hubtool.py",
     "marker": "python hub/hubtool.py",
     "disposition": "must-be-replaced",
     "obligation": "The CLI verbs are charter-preserved observable behaviour and are invoked through an interpreter today."},
    {"role": "chat-client-cli-parity-block", "path": "hubtool.py",
     "marker": "python hubtool.py",
     "disposition": "must-be-replaced",
     "obligation": "The same CLI surface, documented in the FR-09 cutover block; both spellings must move together or the advertised verbs diverge from the real ones."},
    {"role": "chat-client-mcp-registration", "path": "hubtool.py",
     "marker": "claude mcp add mailhub",
     "disposition": "must-be-replaced",
     "obligation": "The MCP server is registered as an interpreter command line; a native client changes the registration operators must run."},
    {"role": "session-bootstrap-hook", "path": "session-start.sh",
     "marker": "python $HUBTOOL",
     "disposition": "must-be-replaced",
     "obligation": "The SessionStart hook hands every new session interpreter command lines for register/listen/list/send."},
    {"role": "session-bootstrap-commentary", "path": "session-start.sh",
     "marker": "python.exe can open",
     "disposition": "commentary-only",
     "obligation": "Explanatory comment about path spelling. It launches nothing; recorded so the census stays fully dispositioned rather than carrying an unexplained hit."},
    {"role": "onboarding-installer", "path": "install-hook.py",
     "marker": "python hub/install-hook.py",
     "disposition": "must-be-replaced",
     "obligation": "Hook installation is a Python script; packaged onboarding cannot depend on an interpreter."},
    {"role": "docker-verification-tooling", "path": "tools/verify-docker.py",
     "marker": "python tools/verify-docker.py",
     "disposition": "test-tooling-not-a-backend-dependency",
     "obligation": "Explicitly OUT of the R10 assertion: the docket rules that Python used by test tooling is not reclassified as an Orgtree backend dependency. It must not be counted as a remaining backend requirement, and it must not be used to launch the product under test."},
]

# Schema creation carries no interpreter word on its own line, so the census
# regex would miss it entirely. It is anchored explicitly because the
# coordinator named schema initialization as a path that must stop requiring
# Python.
SCHEMA_BOOTSTRAP = {
    "role": "hub-schema-initialization", "path": "mailhub/db.py",
    "marker": "con.executescript(_SCHEMA)",
    "disposition": "must-be-replaced",
    "obligation": "Every hub schema create and upgrade runs through this Python function today, including the FR-06 ALTER TABLE fallback immediately below it. The port owns schema creation and migration natively.",
}

# The parent repository drives that same initializer through its own
# interpreter subprocess. That code is NOT in this repository and cannot be
# verified from this worktree, so it is carried as an attributed external claim
# -- never as a fact this probe checked.
EXTERNAL_WITNESSES = [
    {"role": "parent-schema-initialization-subprocess",
     "repository": "orgtree (parent)", "witness": "engine/mailhub_runtime.py:169",
     "verified_here": False,
     "claim": "_migrate_store invokes sys.executable -c 'import mailhub.db ...' to initialize the schema before import.",
     "attributed_to": "rust-program-astra, mailhub-parent-boundary-20260922.md, parent commit 24c03bd4992ff331ca87be3e62f298aaa5c53835",
     "disposition": "must-be-replaced-by-parent-slice",
     "obligation": "R09/R10 own this one; MH cannot close it from inside the product repository."},
    {"role": "parent-service-launch-subprocess",
     "repository": "orgtree (parent)", "witness": "engine/mailhub_runtime.py:335",
     "verified_here": False,
     "claim": "start() launches sys.executable -m mailhub.serve in the product checkout.",
     "attributed_to": "rust-program-astra, mailhub-parent-boundary-20260922.md, parent commit 24c03bd4992ff331ca87be3e62f298aaa5c53835",
     "disposition": "must-be-replaced-by-parent-slice",
     "obligation": "Replacing the product binary is not enough; the parent launcher must stop spawning an interpreter."},
]

def _resolve(files, path, marker):
    """Line numbers in the pinned file whose text contains `marker`.

    Matching is case-INSENSITIVE, because the census pattern is: `fastapi`
    appears as both `from fastapi.responses` and `FastAPI(title=...)`, and a
    case-sensitive marker silently leaves the constructor and the type
    annotation undispositioned. An empty marker claims the whole file --
    comments included, since a dependency manifest's own header is part of it.
    """
    if path not in files:
        # The file this role describes is not in the tree at all. That is a
        # denominator problem, which `check` already reports -- so record the
        # role as unresolved and let the build finish, rather than crashing
        # and losing every other finding.
        return None
    lines = files[path].decode("utf-8").splitlines()
    if marker == "":
        hits = [i for i, line in enumerate(lines, 1) if line.strip()]
    else:
        needle = marker.lower()
        hits = [i for i, line in enumerate(lines, 1) if needle in line.lower()]
    if not hits:
        raise SystemExit("dead python-role marker: %r has no line containing %r" % (path, marker))
    return hits

def python_dependencies(files):
    witnesses = []
    for path, raw in sorted(files.items()):
        try: text = raw.decode("utf-8")
        except UnicodeDecodeError: continue
        for i, line in enumerate(text.splitlines(), 1):
            if _PY_WITNESS.search(line):
                witnesses.append({"path": path, "line": i, "family": family(path),
                                  "text": line.strip()[:400]})
    roles, covered = [], set()
    for spec in [*PYTHON_ROLES, SCHEMA_BOOTSTRAP]:
        lines = _resolve(files, spec["path"], spec["marker"])
        if lines is None:
            roles.append({**spec, "lines": [], "missing_source": True})
            continue
        roles.append({**spec, "lines": lines})
        covered.update((spec["path"], n) for n in lines)
    # Fail-closed: an interpreter witness in a backend family that no role
    # accounts for is a silently omitted dependency -- precisely what R10 must
    # not inherit.
    uncovered = [w for w in witnesses
                 if w["family"] in _BACKEND_FAMILIES
                 and (w["path"], w["line"]) not in covered]
    return {"witness_pattern": _PY_WITNESS.pattern,
            "backend_families": sorted(_BACKEND_FAMILIES),
            "witnesses": witnesses,
            "roles": sorted(roles, key=lambda r: (r["path"], r["role"])),
            "external_witnesses": EXTERNAL_WITNESSES,
            "uncovered_backend_witnesses": uncovered}

# ------------------------------------------- operation, state and protocol families
# Reviewer finding F2: a file/function/SQL census is not the registry MH01 owes.
# Counting 26 files says nothing about a queue that lives only in memory, a file
# that carries process ownership, or an artifact written outside the blob root.
# Those are the families a port silently loses, so each is dispositioned here
# against an exact source anchor. Same fail-closed rule as the interpreter
# register: an anchor that resolves to nothing aborts the build, so this table
# cannot decay into a description of source that has moved on.
#
# `unknowns` is load-bearing. MH01 is forbidden from arming a listener or
# driving live MCP/onboarding paths, so several of these are frozen from source
# and explicitly NOT exercised. Recording that here is what makes them block
# conversion instead of surfacing as a surprise in MH02.
STATE_FAMILIES = [
    {"family": "receipt-retry-queue", "path": "hubtool.py",
     "marker": "_RC_RETRY: dict[str, list[dict[str, Any]]] = {}",
     "category": "queue", "durability": "memory-only",
     "authority": "one listener process per identity, keyed per hub",
     "semantics": "A receipt POST that fails re-queues under its hub key and is retried on a later "
                  "receipt cycle. The queue is bounded at 200 per hub, KEEPING THE NEWEST and "
                  "discarding the oldest display states first.",
     "loss": "A listener restart drops the queue entirely. This loses DISPLAY state (a sender's "
             "ladder stays at a lower rung) and never loses a message -- the two must not be "
             "conflated in an MH03 fault schedule.",
     "disposition": "must-be-ported",
     "obligation": "The port needs an equivalent bounded per-hub retry buffer with the same "
                   "newest-wins truncation and the same best-effort contract. Making it durable "
                   "would be a behaviour CHANGE and needs its own ruling, not a silent upgrade.",
     "unknowns": ["Not exercised: driving it needs a live listener and a failing hub, both "
                  "forbidden in MH01.",
                  "Truncation ordering under concurrent cycles is unverified -- _RC_LOCK is held "
                  "for the mutation but interleaving with _call is not."]},
    {"family": "listener-process-ownership", "path": "hubtool.py",
     "marker": '.listening',
     # The release is anchored too. An earlier revision of this register said the
     # lock was "never removed on exit", which the source contradicts at the
     # `finally`. Anchoring the removal means that claim can no longer drift: if
     # a future source drops the cleanup, this marker dies and the build aborts.
     "release_marker": "os.remove(lock)",
     "category": "process-custody", "durability": "on-disk, beside the identity file",
     "authority": "exactly one live listener per identity name",
     "semantics": "O_CREAT|O_EXCL mints the lock and writes the owning pid. If it already exists "
                  "the holder pid is read and probed: a LIVE holder other than self refuses the "
                  "second listener; an unreadable, zero or dead holder is treated as stale and the "
                  "lock is TAKEN OVER by rewriting the pid.",
     # Best-effort, and stated as such. An earlier revision claimed a death that
     # does not unwind was the ONLY way a stale lock survives; the source has two
     # more paths, and overstating the cleanup is the same class of error as
     # denying it.
     "loss": "Removal is ATTEMPTED, not guaranteed. listen() unlinks the lock in a `finally`, "
             "but that finally belongs to the main loop's try, and the unlink itself is "
             "wrapped in `except OSError: pass`. So the lock survives three ways: a death "
             "that does not unwind (SIGKILL, power loss, os._exit); an unlink that FAILS, "
             "whose OSError is swallowed; and any exit between taking the lock and entering "
             "that try -- _hubs(d) raising, or the hubs0[0] index on an empty hub list -- "
             "because the finally has not been armed yet. In each case the next start reads "
             "the dead pid and takes the lock over. The live-holder refusal path also "
             "returns before the try, which is CORRECT there: a refused second listener "
             "must leave the real holder's lock in place.",
     "disposition": "must-be-ported",
     "obligation": "The port must keep single-writer custody per identity. The stale-takeover path "
                   "is the dangerous one: it decides ownership from a pid alone.",
     "unknowns": ["PID REUSE IS UNCHARACTERIZED AND BLOCKS CONVERSION: _pid_alive trusts an integer "
                  "with no start-time or identity check, so an unrelated process inheriting the pid "
                  "reads as the live holder and wrongly refuses the real listener.",
                  "Windows probes via tasklist and POSIX via os.kill(pid, 0); the two disagree for "
                  "a pid owned by another user, and only Windows is exercised.",
                  "Not exercised: arming a listener is forbidden in MH01."]},
    {"family": "fetched-attachment-output", "path": "hubtool.py",
     "marker": "def fetch_attachment(",
     "category": "output-artifact", "durability": "on-disk, OUTSIDE the hub blob root",
     "authority": "the calling process, in its own working directory",
     # The two fallbacks sit at DIFFERENT stages and do not chain. An earlier
     # revision wrote them as one ladder (name -> aid -> file.bin), which the
     # source contradicts: a header that sanitizes away yields file.bin, never
     # the aid.
     "semantics": "Writes into outdir or os.getcwd(). The name is taken from the hub's "
                  "Content-Disposition and basename-stripped; if that header carries no "
                  "parseable filename the ATTACHMENT ID becomes the name instead. Whichever "
                  "of the two it is, it is then sanitized to [\\w .()+-] with surrounding "
                  "dots and spaces trimmed, and if sanitizing empties it the name becomes "
                  "'file.bin' DIRECTLY -- not the attachment id, which is a separate, "
                  "earlier fallback on a different condition. Collisions are suffixed "
                  "-2, -3, ... rather than overwritten. Hubs on the identity's list are "
                  "tried IN ORDER until one holds the id.",
     "loss": "None on the hub side; these are client-side copies. But they are durable files the "
             "hub's own retention never reclaims.",
     "disposition": "must-be-ported",
     "obligation": "Sanitization and collision suffixing are security-relevant and contractual -- a "
                   "port that writes the server-supplied name unsanitized introduces a path-traversal "
                   "bug the pinned source does not have.",
     "unknowns": ["Not exercised: needs a live hub serving a real Content-Disposition.",
                  "The sanitizer is not proven against a name that normalizes to empty on a "
                  "non-Windows filesystem."]},
    {"family": "onboarding-settings-mutation", "path": "install-hook.py",
     "marker": "SETTINGS = os.path.expanduser",
     "category": "configuration-state", "durability": "on-disk, in the user's home",
     "authority": "whoever runs the installer; no locking",
     "semantics": "Rewrites ~/.claude/settings.json to wire a SessionStart hook. Before writing it "
                  "saves settings.json.bak.<unix-seconds>. It also SWEEPS hook entries whose command "
                  "mentions .claude/chatq, and is idempotent by detecting an existing "
                  "session-start.sh hub entry.",
     "loss": "Backups accumulate forever -- one per run, never reclaimed. Two concurrent installs "
             "last-writer-wins with no locking.",
     "disposition": "out-of-scope-for-the-hub-port",
     "obligation": "This is operator onboarding, not hub runtime. Recorded so the port does not "
                   "silently drop it and so R10 knows the file is touched; it is NOT part of the "
                   "hub service and must not be folded into it.",
     "unknowns": ["Not exercised: running it would mutate the real user's settings.json, which MH01 "
                  "is forbidden to touch.",
                  "The one-second backup granularity collides if run twice within a second; "
                  "unverified."]},
    {"family": "session-admission-environment", "path": "session-start.sh",
     "marker": 'ORGTREE_NODE',
     "category": "configuration-source", "durability": "none; per-session decision",
     "authority": "the shell hook, at session start",
     "semantics": "Exits 0 WITHOUT onboarding when ORGTREE_NODE is set, or when PWD is under "
                  "$HOME/orgtree/scratch/. Orgtree agent sessions coordinate through orgtree, not "
                  "the hub, so this is the gate that keeps them off it.",
     "loss": "n/a",
     "disposition": "must-be-ported",
     "obligation": "Recorded because the interpreter census reads os.environ.get calls in PYTHON "
                   "only, so a shell-level environment gate is invisible to it. Dropping this in a "
                   "port would onboard every orgtree agent onto the mail hub -- the precise "
                   "cross-system contamination the gate exists to prevent.",
     "unknowns": ["Not exercised: shell-level, and the census extracts Python environment reads."]},
    {"family": "mcp-jsonrpc-envelope", "path": "hubtool.py",
     "marker": '{"jsonrpc": "2.0", "id": id_, "result": result}',
     "category": "protocol-envelope", "durability": "none; stdio request/response",
     "authority": "one serve() process over stdin/stdout",
     "semantics": "Line-delimited JSON-RPC over stdio. EVERY reply is {jsonrpc, id, result} -- there "
                  "is no error member on any path. initialize answers protocolVersion 2024-11-05 "
                  "with capabilities {tools:{}} and serverInfo {name: mailhub, version: 1.0}. "
                  "tools/list returns TOOLS verbatim. tools/call wraps the dispatch string as "
                  "{content: [{type: text, text: ...}]}, and converts a URLError to the TEXT "
                  "{\"error\": \"hub unreachable: ...\"} and any other exception to {\"error\": str(e)} "
                  "-- both still inside a SUCCESS result. An unknown method with a non-null id gets "
                  "an empty result {}; a blank line or unparseable JSON is SKIPPED SILENTLY with no "
                  "reply at all, and a notification (id null) likewise gets none.",
     "loss": "A malformed request is dropped without acknowledgement; the caller sees a hang, not an "
             "error.",
     "disposition": "must-be-ported",
     "obligation": "The absence of a JSON-RPC error member is contractual in this source. A port "
                   "that 'correctly' emits {error: {code, message}} changes observable behaviour for "
                   "every existing client and needs a recorded ruling, not a silent fix.",
     "unknowns": ["Not exercised: driving serve() means speaking MCP to a real hub, forbidden here. "
                  "The envelope is frozen from source, and the frozen shape BLOCKS CONVERSION until "
                  "MH02 exercises it against a synthetic adapter.",
                  "Behaviour on a request that is valid JSON but not an object is unverified: "
                  "msg.get would raise AttributeError and escape the loop."]},
]

def state_families(files):
    """Dispositioned operation/state/protocol families, each anchored in source."""
    rows = []
    for spec in STATE_FAMILIES:
        lines = _resolve(files, spec["path"], spec["marker"])
        row = {**spec, "lines": lines or [], "missing_source": lines is None}
        # A family may anchor the point where it RELEASES its state, not just
        # where it takes it. Same fail-closed rule: a release marker matching
        # nothing aborts the build rather than leaving a stale claim standing.
        if spec.get("release_marker"):
            released = _resolve(files, spec["path"], spec["release_marker"])
            row["release_lines"] = released or []
        rows.append(row)
    return {"intent": "Durable and protocol families a file/function census does not express. "
                      "Each is anchored to an exact source substring; an anchor resolving to "
                      "nothing aborts the build.",
            "categories": sorted({s["category"] for s in STATE_FAMILIES}),
            "families": sorted(rows, key=lambda r: r["family"]),
            "unresolved": [r["family"] for r in rows if r["missing_source"]],
            "blocking_unknowns": sorted(
                {r["family"] for r in rows
                 if any("BLOCK" in u.upper() for u in r["unknowns"])})}

def build(files):
    records, routes, sql, environments, schemas, functions = [], [], [], [], [], []
    tools, cli, rpc = [], [], []
    for path, raw in sorted(files.items()):
        record = {"path": path, "family": family(path), "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}
        records.append(record)
        if not path.endswith(".py"): continue
        text = raw.decode("utf-8")
        tree = ast.parse(text)
        for n in ast.walk(tree):
            if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)):
                functions.append({"path": path, "name": n.name, "line": n.lineno, "end": n.end_lineno})
                for d in n.decorator_list:
                    if isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and isinstance(d.func.value, ast.Name) and d.func.value.id == "app":
                        routes.append({"path": path, "line": d.lineno, "kind": d.func.attr, "argument": ast.literal_eval(d.args[0]), "handler": n.name})
            if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute):
                if n.func.attr in {"execute", "executemany", "executescript"} and n.args:
                    sql.append({"path": path, "line": n.lineno, "call": n.func.attr, "expression": ast.unparse(n.args[0]), "dynamic": not isinstance(n.args[0], ast.Constant)})
                if isinstance(n.func.value, ast.Attribute) and isinstance(n.func.value.value, ast.Name) and n.func.value.value.id == "os" and n.func.value.attr == "environ" and n.func.attr == "get":
                    environments.append({"path": path, "line": n.lineno, "expression": ast.unparse(n)})
            target = n.targets[0] if isinstance(n, ast.Assign) and len(n.targets) == 1 else n.target if isinstance(n, ast.AnnAssign) else None
            if isinstance(target, ast.Name) and target.id in {"_SCHEMA", "_DB_SCHEMA", "TOOLS"}:
                value = ast.literal_eval(n.value)
                if target.id == "TOOLS": tools = value
                else:
                    # Literal DDL only, into an in-memory connection. No product import, no disk database.
                    # closing(): a bare `with sqlite3.connect(...)` commits but never CLOSES, which leaked
                    # the connection and raised ResourceWarning under the 3.13 runtime.
                    with contextlib.closing(sqlite3.connect(":memory:")) as db:
                        db.executescript(value)
                        tables = {}
                        for row in db.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"):
                            name = row[0]
                            tables[name] = [dict(zip(["cid","name","type","notnull","default","pk"], r)) for r in db.execute('PRAGMA table_info("' + name + '")')]
                    schemas.append({"path": path, "line": n.lineno, "sql": value, "tables": tables})
        if path == "hubtool.py":
            cli, rpc = comparisons(tree, "verb"), comparisons(tree, "method")
    return {"schema": "orgtree.mailhub-source-inventory/v2", "source_commit": BASE,
            "scope": "All 26 baseline files; static census plus extracted route and interpreter-dependency contracts. Not runtime coverage, not execution proof, not migration approval.",
            "files": records, "functions": functions, "registrations": routes,
            "framework_routes": ["GET,HEAD /openapi.json", "GET,HEAD /docs", "GET,HEAD /docs/oauth2-redirect", "GET,HEAD /redoc"],
            "mcp_tools": tools, "cli_dispatch": cli, "rpc_dispatch": rpc,
            "literal_schemas": schemas, "sql_sites": sql, "environment_reads": environments,
            "http_contract": http_contract(files, routes),
            "state_families": state_families(files),
            "python_dependencies": python_dependencies(files)}

def authorized(snapshot, current):
    """The freeze, with registered modifications substituted into it.

    A path is substituted only when the file in hand hashes to EXACTLY the
    bytes MODIFICATIONS names for it, so any other content leaves the
    frozen record standing and the rebuild disagrees with it. The original
    bytes leave it standing too, which is deliberate: censusing pinned BASE
    blobs has to stay green without the register knowing which of the two
    contents it is being handed.
    """
    if not MODIFICATIONS: return snapshot
    out = dict(snapshot)
    out["files"] = [dict(record) for record in snapshot["files"]]
    for record in out["files"]:
        named, raw = MODIFICATIONS.get(record["path"]), current.get(record["path"])
        if named is None or raw is None: continue
        if hashlib.sha256(raw).hexdigest() == named["sha256"] and len(raw) == named["bytes"]:
            record["sha256"], record["bytes"] = named["sha256"], named["bytes"]
    return out

def check(snapshot, current, names):
    errors = []
    expected = {r["path"] for r in snapshot["files"]}
    if set(current) != expected: errors.append("source file denominator differs")
    if set(names) - expected - ADDITIONS: errors.append("unclassified source additions: " + ", ".join(sorted(set(names) - expected - ADDITIONS)))
    stray = sorted(set(MODIFICATIONS) - expected)
    if stray: errors.append("authorized modifications naming paths outside the freeze: " + ", ".join(stray))
    # Neither the frozen content nor the authorized one: say so specifically,
    # because "differs from frozen inventory" reads as drift in a file nobody
    # was allowed to touch, and this one somebody was.
    frozen_hashes = {r["path"]: r["sha256"] for r in snapshot["files"]}
    unnamed = sorted(path for path, named in MODIFICATIONS.items()
                     if path in current
                     and hashlib.sha256(current[path]).hexdigest() not in {named["sha256"], frozen_hashes.get(path)})
    if unnamed: errors.append("registered files holding neither the frozen nor the authorized bytes: " + ", ".join(unnamed))
    if build(current) != authorized(snapshot, current): errors.append("source hash or extracted contract differs from frozen inventory")
    fams = snapshot.get("state_families")
    if fams is None:
        errors.append("frozen inventory predates the operation/state/protocol family register")
    else:
        if fams.get("unresolved"):
            errors.append("state families whose source anchor no longer resolves: "
                          + ", ".join(fams["unresolved"]))
        missing = {s["family"] for s in STATE_FAMILIES} - {r["family"] for r in fams.get("families", [])}
        if missing:
            errors.append("state families dropped from the frozen register: " + ", ".join(sorted(missing)))
    uncovered = snapshot.get("python_dependencies", {}).get("uncovered_backend_witnesses")
    if uncovered is None:
        errors.append("frozen inventory predates the interpreter-dependency register")
    elif uncovered:
        errors.append("interpreter dependencies with no recorded disposition: "
                      + ", ".join("%s:%s" % (w["path"], w["line"]) for w in uncovered))
    return errors

def summary(frozen):
    py = frozen.get("python_dependencies", {})
    return {"schema": "orgtree.mailhub-inventory-check/v2", "source_commit": frozen["source_commit"],
            "files": len(frozen["files"]),
            "http_routes": sum(r["kind"] in {"get", "post"} for r in frozen["registrations"]),
            "mcp_tools": len(frozen["mcp_tools"]),
            # Witnesses are not verbs: addhub/drophub each appear in two
            # branches, so 11 comparison sites represent 9 distinct commands.
            "cli_verb_witnesses": len(frozen["cli_dispatch"]),
            "cli_verbs": len({r["value"] for r in frozen["cli_dispatch"]}),
            "interpreter_roles": len(py.get("roles", [])),
            "interpreter_witnesses": len(py.get("witnesses", [])),
            "interpreter_roles_requiring_replacement":
                sum(r["disposition"].startswith("must-be") for r in py.get("roles", [])),
            "external_interpreter_claims": len(py.get("external_witnesses", [])),
            # Refusals, not just their statuses: a status code alone does not
            # pin a refusal, so the detail envelope is counted here too.
            "explicit_refusals": sum(len(r.get("refusals", []))
                                     for r in frozen.get("http_contract", [])),
            "state_families": len(frozen.get("state_families", {}).get("families", [])),
            "families_blocking_conversion":
                len(frozen.get("state_families", {}).get("blocking_unknowns", []))}

def main(argv=None):
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="explicitly regenerate the review candidate from pinned Git objects")
    parser.add_argument("--repo-root", default=None, help="checkout holding the pinned product source (default: this script's repository)")
    parser.add_argument("--inventory", default=None, help="path of the frozen inventory JSON (default: <repo-root>/" + OUTPUT + ")")
    args = parser.parse_args(argv)
    if args.repo_root: ROOT = Path(args.repo_root).resolve()
    dest = Path(args.inventory).resolve() if args.inventory else ROOT / OUTPUT
    if args.write:
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(json.dumps(build(source_files()), indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    frozen = json.loads(dest.read_text(encoding="utf-8"))
    # Normalize checkout CRLF only; Git's text checkout policy is not a product change.
    current = {r["path"]: (ROOT / r["path"]).read_bytes().replace(b"\r\n", b"\n") for r in frozen["files"] if (ROOT / r["path"]).is_file()}
    names = git("ls-files", "--cached", "--others", "--exclude-standard", "-z").decode().split("\0")
    errors = check(frozen, current, [n for n in names if n])
    print(json.dumps({**summary(frozen), "errors": errors}))
    return bool(errors)

if __name__ == "__main__": raise SystemExit(main())
