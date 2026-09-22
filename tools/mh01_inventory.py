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
ADDITIONS = {OUTPUT, "docs/mh01-contract.md", "tools/mh01_inventory.py",
             "tests/mh01_contract.py", "tests/test_mh01_inventory.py",
             "tests/fixtures/mh01-wire.json"}

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
            "python_dependencies": python_dependencies(files)}

def check(snapshot, current, names):
    errors = []
    expected = {r["path"] for r in snapshot["files"]}
    if set(current) != expected: errors.append("source file denominator differs")
    if set(names) - expected - ADDITIONS: errors.append("unclassified source additions: " + ", ".join(sorted(set(names) - expected - ADDITIONS)))
    if build(current) != snapshot: errors.append("source hash or extracted contract differs from frozen inventory")
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
            "external_interpreter_claims": len(py.get("external_witnesses", []))}

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
