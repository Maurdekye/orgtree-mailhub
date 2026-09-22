"""MH02 MCP envelope contract — the neutral profile, executed on both sides.

`tests/fixtures/mh02-mcp-envelope.json` is the CONTRACT: language-neutral
cases that say what the mail hub's MCP stdio envelope must do. This file is
only a pair of DRIVERS for it. Implementation selection sits outside the
assertions, exactly as it does for the MH01 HTTP profile: one adapter runs the
pinned Python `serve()`, the other runs the Rust `mailhub-protocol` crate
through its test-only pipe driver, and the same `case_errors()` function judges
both observations.

What it proves and what it does not:

- It proves that the PINNED PYTHON SOURCE at commit 6477321… behaves the way
  the profile says, and that the Rust crate reproduces the same envelope.
- It proves NOTHING about the eight tools. Every handler here is synthetic:
  the oracle is given a fake `dispatch` and the Rust side an injected
  dispatcher, so no identity is minted, no store is opened, no hub is
  contacted and no listener is armed. The MH01 source registry's MCP
  conversion blocker stays open, narrowed only by the envelope.
- It is not the R10 Python-free gate. Running the oracle needs an interpreter
  precisely because the reference implementation is still Python.

Isolation. The oracle does NOT import `hubtool`. It reads the pinned blob out
of Git, extracts the exact `serve` function and the exact `TOOLS` literal from
the AST, checks their digests against the profile, and compiles that one
function alone into a namespace holding only `TOOLS`, a `cast` that returns its
argument, a fake `dispatch`, the real `json`, a fake `sys` wrapping in-memory
streams, and a `urllib` namespace carrying nothing but `error.URLError`. The
module's main guard, CLI, identity helpers and listener are never compiled, so
there is no code path from here to a profile, a store or a socket. While the
extracted function runs, an audit hook refuses EVERY audited operation, and a
control proves the hook fires.

    <python> tests/mh02_mcp_contract.py [-v]
    <python> tests/mh02_mcp_contract.py --target both --rust-driver <path>

Environment overrides: MH02_REPO (which checkout), MH02_FIXTURES (which
profile), MH02_RUST_DRIVER (the built `envelope_probe` executable) and
MH02_TARGET (`python`, `rust` or `both`). A missing Rust driver is an
ENVIRONMENT BLOCKER that fails the run; it is never a skip and never a pass.
"""

from __future__ import annotations

import ast
import contextlib
import copy
import hashlib
import io
import json
import os
import queue
import re
import subprocess
import sys
import threading
import traceback
import unittest
import urllib.error
from pathlib import Path
from types import SimpleNamespace

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

_HERE = Path(__file__).resolve().parent
_REPO = Path(os.environ.get("MH02_REPO") or _HERE.parent)
FIXTURES = Path(os.environ.get("MH02_FIXTURES") or (_HERE / "fixtures/mh02-mcp-envelope.json"))
VERBOSE = "-v" in sys.argv


def _cli(argv):
    """Read the two options that must be known BEFORE anything is collected.

    They are parsed here rather than under `__main__` because collection
    happens at import time: a `--rust-driver` read after the driver has already
    been looked for is a flag that silently does nothing.
    """
    driver = os.environ.get("MH02_RUST_DRIVER") or ""
    target = os.environ.get("MH02_TARGET") or "both"
    remaining = []
    iterator = iter(argv)
    for argument in iterator:
        if argument == "--rust-driver":
            driver = next(iterator, "")
        elif argument.startswith("--rust-driver="):
            driver = argument.split("=", 1)[1]
        elif argument == "--target":
            target = next(iterator, "")
        elif argument.startswith("--target="):
            target = argument.split("=", 1)[1]
        elif argument != "--narrative":
            remaining.append(argument)
    return driver, target, remaining


RUST_DRIVER, TARGET, _UNITTEST_ARGV = _cli(sys.argv[1:])
if TARGET not in ("python", "rust", "both"):
    raise SystemExit("--target must be python, rust or both, not %r" % TARGET)


# ── the pinned source ───────────────────────────────────────────────────────
# Reading Git is an explicit harness operation. It happens here, outside the
# guard, and it is the ONLY way the source enters this process: the product
# module is never imported and its file is never executed.

def _git_show(ref: str, path: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(_REPO), "show", f"{ref}:{path}"])


def _sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


class Anchors:
    """The exact `serve` and `TOOLS` nodes, with their digests."""

    def __init__(self, commit: str):
        self.commit = commit
        self.blob = _git_show(commit, "hubtool.py")
        self.text = self.blob.decode("utf-8")
        self.lines = self.text.split("\n")
        self.tree = ast.parse(self.text)
        self.serve = self._one_serve()
        self.tools_node = self._one_tools()
        self.cards = ast.literal_eval(self.tools_node.value)

    def _one_serve(self):
        found = [n for n in self.tree.body
                 if isinstance(n, ast.FunctionDef) and n.name == "serve"]
        if len(found) != 1:
            raise AssertionError("expected exactly one top-level serve(), found %d" % len(found))
        return found[0]

    def _one_tools(self):
        found = [n for n in self.tree.body
                 if isinstance(n, ast.AnnAssign) and getattr(n.target, "id", None) == "TOOLS"]
        if len(found) != 1:
            raise AssertionError("expected exactly one top-level TOOLS, found %d" % len(found))
        return found[0]

    def segment(self, node) -> str:
        return "\n".join(self.lines[node.lineno - 1:node.end_lineno])

    def digests(self) -> dict:
        return {
            "path": "hubtool.py",
            "sha256": hashlib.sha256(self.blob).hexdigest(),
            "bytes": len(self.blob),
            "serve": {"lineno": self.serve.lineno, "end_lineno": self.serve.end_lineno,
                      "segment_sha256": _sha(self.segment(self.serve)),
                      "unparsed_sha256": _sha(ast.unparse(self.serve))},
            "tools": {"lineno": self.tools_node.lineno,
                      "end_lineno": self.tools_node.end_lineno,
                      "segment_sha256": _sha(self.segment(self.tools_node)),
                      "unparsed_sha256": _sha(ast.unparse(self.tools_node))},
        }


#: Every global name the extracted function loads AT RUNTIME. Anything else
#: appearing here means the source grew a dependency this oracle does not
#: supply, and the run must stop rather than silently pick it up from the real
#: module.
SERVE_GLOBALS = {"TOOLS", "cast", "dispatch", "json", "sys", "urllib"}
#: Names that appear only inside annotations. `hubtool.py` carries
#: `from __future__ import annotations`, so these are never evaluated — which
#: is exactly why the oracle compiles with that same future flag. Without it,
#: `def reply(id_: Any, ...)` would look up a name this namespace deliberately
#: does not hold, and the extraction would die at definition time.
SERVE_ANNOTATION_ONLY = {"Any"}
#: The only builtins the extracted function is given.
SERVE_BUILTINS = {"str": str, "dict": dict, "Exception": Exception, "ValueError": ValueError}


def _annotation_nodes(node):
    """Every subtree that is an annotation rather than executable code."""
    out = []
    for sub in ast.walk(node):
        if isinstance(sub, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if sub.returns is not None:
                out.append(sub.returns)
            args = sub.args
            for arg in (list(args.args) + list(args.kwonlyargs) + list(args.posonlyargs)
                        + [a for a in (args.vararg, args.kwarg) if a]):
                if arg.annotation is not None:
                    out.append(arg.annotation)
        elif isinstance(sub, ast.AnnAssign) and sub.annotation is not None:
            out.append(sub.annotation)
    return out


def _names_in(nodes) -> set:
    found = set()
    for node in nodes:
        found.update(sub.id for sub in ast.walk(node) if isinstance(sub, ast.Name))
    return found


def _loaded_globals(node, include_annotations=False) -> set:
    """Free names the function reads, ignoring its own locals and parameters."""
    bound = set()
    for sub in ast.walk(node):
        if isinstance(sub, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if sub is not node:
                bound.add(sub.name)
            for arg in list(sub.args.args) + list(sub.args.kwonlyargs) + list(sub.args.posonlyargs):
                bound.add(arg.arg)
        elif isinstance(sub, ast.Name) and isinstance(sub.ctx, (ast.Store, ast.Del)):
            bound.add(sub.id)
        elif isinstance(sub, ast.ExceptHandler) and sub.name:
            bound.add(sub.name)
    read = {sub.id for sub in ast.walk(node)
            if isinstance(sub, ast.Name) and isinstance(sub.ctx, ast.Load)}
    if not include_annotations:
        read -= _names_in(_annotation_nodes(node))
    return {name for name in read - bound if name not in SERVE_BUILTINS}


def shape_errors(anchors: Anchors) -> list:
    """The AST must have the shape this oracle was written against."""
    errors = []
    serve = anchors.serve
    body = serve.body
    if not (body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant)):
        errors.append("serve() no longer opens with its docstring")
    defs = [n for n in body if isinstance(n, ast.FunctionDef)]
    if [n.name for n in defs] != ["reply"]:
        errors.append("serve() no longer defines exactly one inner function named reply")
    loops = [n for n in body if isinstance(n, ast.For)]
    if len(loops) != 1:
        errors.append("serve() no longer has exactly one top-level loop")
    else:
        iterated = loops[0].iter
        if not (isinstance(iterated, ast.Attribute) and iterated.attr == "stdin"
                and isinstance(iterated.value, ast.Name) and iterated.value.id == "sys"):
            errors.append("serve()'s loop no longer iterates sys.stdin")
    loaded = _loaded_globals(serve)
    if loaded != SERVE_GLOBALS:
        errors.append("serve() reads the globals %s, not the expected %s"
                      % (sorted(loaded), sorted(SERVE_GLOBALS)))
    annotation_only = (_names_in(_annotation_nodes(serve)) - SERVE_GLOBALS
                       - set(SERVE_BUILTINS))
    if annotation_only != SERVE_ANNOTATION_ONLY:
        errors.append("serve()'s annotations name %s, not the expected %s; the oracle "
                      "compiles with the source's `from __future__ import annotations` "
                      "flag precisely so these are never evaluated"
                      % (sorted(annotation_only), sorted(SERVE_ANNOTATION_ONLY)))
    if not isinstance(anchors.tools_node.value, ast.List):
        errors.append("TOOLS is no longer a list literal")
    elif not all(isinstance(e, ast.Dict) for e in anchors.tools_node.value.elts):
        errors.append("TOOLS is no longer a list of dict literals")
    if not isinstance(anchors.cards, list) or len(anchors.cards) != 8:
        errors.append("TOOLS no longer holds exactly eight cards")
    for card in anchors.cards if isinstance(anchors.cards, list) else []:
        if set(card) != {"name", "description", "inputSchema"}:
            errors.append("tool card %r no longer carries exactly name/description/inputSchema"
                          % card.get("name"))
    return errors


def source_errors(profile: dict, anchors: Anchors) -> list:
    """The pinned digests must still describe the source that is really there."""
    errors = []
    if profile.get("source_commit") != anchors.commit:
        errors.append("the profile pins %r but the source was read at %r"
                      % (profile.get("source_commit"), anchors.commit))
    want, got = profile.get("source"), anchors.digests()
    if want != got:
        for key in sorted(set(want or {}) | set(got)):
            if (want or {}).get(key) != got.get(key):
                errors.append("pinned source %s is %r, the source has %r"
                              % (key, (want or {}).get(key), got.get(key)))
    if profile.get("tools") != anchors.cards:
        errors.append("the profile's tool cards differ from the pinned TOOLS literal")
    if profile.get("server") != {"protocolVersion": "2024-11-05",
                                 "capabilities": {"tools": {}},
                                 "serverInfo": {"name": "mailhub", "version": "1.0"}}:
        errors.append("the profile's server metadata is not the source's")
    return errors


# ── the guard ───────────────────────────────────────────────────────────────


class GuardViolation(BaseException):
    """Deliberately NOT an Exception: `serve()` has an `except Exception` arm
    around the dispatcher, and a guard breach that the subject under test could
    swallow into an error string would prove nothing."""


class HarnessFault(BaseException):
    """Same reasoning: a fault in the harness must never read as a product
    error string."""


_GUARD = threading.local()


def _audit(event, args):                                          # noqa: ARG001
    if getattr(_GUARD, "armed", False):
        raise GuardViolation(
            "the guarded oracle attempted the audited operation %r; the extracted "
            "serve() must touch no file, socket, process or import" % (event,))


sys.addaudithook(_audit)


@contextlib.contextmanager
def guarded():
    _GUARD.armed = True
    try:
        yield
    finally:
        _GUARD.armed = False


# ── the Python oracle ───────────────────────────────────────────────────────


class _Stdout:
    """Records one entry per write, so framing can be judged rather than
    reconstructed by splitting on newlines."""

    def __init__(self):
        self.chunks = []
        self.flushes = 0

    def write(self, text):
        if not isinstance(text, str):
            raise HarnessFault("serve() wrote a non-string to stdout")
        self.chunks.append(text)
        return len(text)

    def flush(self):
        self.flushes += 1


class _Stdin:
    """`sys.stdin` iteration with universal newlines, counting what was read.

    The count is the whole point: after a terminal outcome the source stops
    pulling lines, and the number it never reached is contractual.
    """

    def __init__(self, text):
        self.lines = list(io.StringIO(text, newline=None))
        self.consumed = 0

    def __iter__(self):
        return self

    def __next__(self):
        if self.consumed >= len(self.lines):
            raise StopIteration
        self.consumed += 1
        return self.lines[self.consumed - 1]


_URLLIB = SimpleNamespace(error=SimpleNamespace(URLError=urllib.error.URLError))


def _make_dispatch(outcomes, calls):
    pending = list(outcomes)

    def dispatch(tool, arguments):
        calls.append({"tool": tool, "arguments": arguments})
        if not pending:
            raise HarnessFault("the scripted dispatcher was called more often than declared")
        outcome = pending.pop(0)
        kind = outcome["kind"]
        if kind == "text":
            return outcome["text"]
        if kind == "url_error":
            raise _URLLIB.error.URLError(outcome["reason"])
        if kind == "exception":
            raise Exception(outcome["message"])
        raise HarnessFault("unknown dispatch outcome kind %r" % kind)

    return dispatch, pending


class Oracle:
    """The extracted `serve`, compiled once and run in a fresh namespace."""

    def __init__(self, anchors: Anchors):
        import __future__

        self.anchors = anchors
        module = ast.Module(body=[anchors.serve], type_ignores=[])
        ast.fix_missing_locations(module)
        # The source carries `from __future__ import annotations`, so its
        # annotations are never evaluated. Compiling with the same flag keeps
        # that true; without it, `def reply(id_: Any, ...)` would look up a
        # name this namespace deliberately does not hold.
        self.code = compile(module, "<hubtool.serve@%s>" % anchors.commit[:12], "exec",
                            flags=__future__.annotations.compiler_flag, dont_inherit=True)

    def observe(self, case: dict) -> dict:
        calls = []
        dispatch, pending = _make_dispatch(case.get("dispatch") or [], calls)
        stdin, stdout = _Stdin(case["input"]), _Stdout()
        namespace = {
            "__builtins__": dict(SERVE_BUILTINS),
            "TOOLS": copy.deepcopy(self.anchors.cards),
            "cast": lambda _type, value: value,
            "dispatch": dispatch,
            "json": json,
            "sys": SimpleNamespace(stdin=stdin, stdout=stdout),
            "urllib": _URLLIB,
        }
        exec(self.code, namespace)                                # noqa: S102
        serve = namespace["serve"]
        outcome, terminal = "completed", None
        with guarded():
            try:
                serve()
            except (GuardViolation, HarnessFault):
                raise
            except BaseException as error:                        # noqa: BLE001
                outcome = "terminated"
                terminal = {"kind": type(error).__name__, "message": str(error)}
        frames = [chunk[:-1] if chunk.endswith("\n") else chunk for chunk in stdout.chunks]
        return {
            "target": "python",
            "frames": frames,
            "raw": "".join(stdout.chunks),
            "calls": calls,
            "outcome": outcome,
            "terminal": terminal,
            "lines_total": len(stdin.lines),
            "lines_processed": stdin.consumed,
            "lines_unprocessed": len(stdin.lines) - stdin.consumed,
            "dispatch_unused": len(pending),
            "flushes": stdout.flushes,
        }


# ── the Rust driver ─────────────────────────────────────────────────────────


class DriverTimeout(Exception):
    pass


class DriverFault(Exception):
    pass


_EOF = object()


class DriverSession:
    """One owned child process speaking the job protocol over its own pipes.

    Every run is bounded in time and in output, both pipes are closed, and the
    process is proven gone on the way out — on success, on an assertion failure
    and on a timeout alike, because the teardown lives in `__exit__`.
    """

    OUTPUT_LIMIT = 16 * 1024 * 1024

    def __init__(self, path, timeout=60.0):
        self.path = str(path)
        self.timeout = timeout
        self.proc = None
        self.pid = None
        self.returncode = None
        self.killed = False
        self.cleanup_errors = []
        self.stderr = b""
        self._out = queue.Queue()
        self._bytes = 0
        self._threads = []

    def __enter__(self):
        self.proc = subprocess.Popen(
            [self.path],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            cwd=str(_REPO),
        )
        self.pid = self.proc.pid
        for target in (self._pump_stdout, self._pump_stderr):
            thread = threading.Thread(target=target, daemon=True)
            thread.start()
            self._threads.append(thread)
        return self

    def __exit__(self, *_exc):
        self.close()
        return False

    def _pump_stdout(self):
        try:
            for raw in self.proc.stdout:
                self._bytes += len(raw)
                if self._bytes > self.OUTPUT_LIMIT:
                    self._out.put(DriverFault("the driver exceeded its %d byte output budget"
                                              % self.OUTPUT_LIMIT))
                    return
                self._out.put(raw.decode("utf-8", "replace"))
        except (OSError, ValueError):
            pass
        finally:
            self._out.put(_EOF)

    def _pump_stderr(self):
        try:
            self.stderr = self.proc.stderr.read() or b""
        except (OSError, ValueError):
            pass

    def request(self, job, timeout=None):
        payload = json.dumps(job, ensure_ascii=True) + "\n"
        try:
            self.proc.stdin.write(payload.encode("utf-8"))
            self.proc.stdin.flush()
        except OSError as error:
            raise DriverFault("the driver closed its input: %s" % error) from error
        try:
            item = self._out.get(timeout=self.timeout if timeout is None else timeout)
        except queue.Empty:
            raise DriverTimeout("the driver produced no answer within %.3gs"
                                % (self.timeout if timeout is None else timeout)) from None
        if item is _EOF:
            raise DriverFault("the driver closed its output before answering")
        if isinstance(item, Exception):
            raise item
        answer = json.loads(item)
        if "error" in answer:
            raise DriverFault("the driver refused the job: %s" % answer["error"])
        return answer

    def close(self):
        if self.proc is None:
            return
        errors = []
        try:
            if self.proc.stdin and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except OSError as error:
            errors.append("closing stdin: %s" % error)
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.killed = True
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                errors.append("the driver survived its kill")
        for thread in self._threads:
            thread.join(timeout=5)
            if thread.is_alive():
                errors.append("a reader thread did not stop")
        for stream in (self.proc.stdout, self.proc.stderr):
            try:
                if stream and not stream.closed:
                    stream.close()
            except OSError as error:
                errors.append("closing a pipe: %s" % error)
        self.returncode = self.proc.poll()
        if self.returncode is None:
            errors.append("the driver process is still running after close()")
        if self.stderr.strip():
            errors.append("the driver wrote to stderr: %r" % self.stderr[:400])
        self.cleanup_errors = errors
        self.proc = None

    def receipt(self):
        return {"pid": self.pid, "returncode": self.returncode, "killed": self.killed,
                "stdout_bytes": self._bytes, "cleanup_errors": list(self.cleanup_errors)}


def rust_observation(answer: dict) -> dict:
    observation = {
        "target": "rust",
        "frames": list(answer.get("frames") or []),
        "raw": answer.get("raw", ""),
        "calls": list(answer.get("calls") or []),
        "outcome": answer.get("outcome"),
        "terminal": answer.get("terminal"),
        "lines_total": answer.get("lines_total"),
        "lines_processed": answer.get("lines_processed"),
        "lines_unprocessed": answer.get("lines_unprocessed"),
        "dispatch_unused": answer.get("dispatch_unused"),
    }
    if answer.get("outcome") == "unrepresentable":
        observation["unrepresentable"] = answer.get("unrepresentable")
    return observation


# ── judging an observation ──────────────────────────────────────────────────
# One function, both targets. Every finding is prefixed by a LABEL, so a
# control can require that a specific behavioural assertion broke rather than
# settling for "something failed".


def resolve(value, tools, placeholder):
    """Expand the one documented placeholder: the eight tool cards."""
    if value == placeholder:
        return copy.deepcopy(tools)
    if isinstance(value, list):
        return [resolve(item, tools, placeholder) for item in value]
    if isinstance(value, dict):
        return {key: resolve(item, tools, placeholder) for key, item in value.items()}
    return value


def case_errors(case: dict, observation: dict, profile: dict) -> list:
    expect = resolve(case["expect"], profile["tools"], profile["tools_placeholder"])
    errors = []

    outcome = observation.get("outcome")
    if outcome != expect["outcome"]:
        errors.append("outcome: expected %r, observed %r" % (expect["outcome"], outcome))
    if outcome == "unrepresentable":
        errors.append("unrepresentable: this implementation declined to model the input (%s)"
                      % ((observation.get("unrepresentable") or {}).get("detail"),))

    # Framing first, and defensively: a defect that merges two replies onto one
    # line produces something that is not a JSON document at all, and that has
    # to read as a named framing failure rather than crashing the judge.
    observed_frames = []
    for index, frame in enumerate(observation["frames"]):
        if "\n" in frame or "\r" in frame:
            errors.append("framing: frame[%d] is not a single line" % index)
        try:
            observed_frames.append(json.loads(frame))
        except ValueError as error:
            errors.append("framing: frame[%d] is not one JSON document (%s)" % (index, error))
    rebuilt = "".join(frame + "\n" for frame in observation["frames"])
    if observation["raw"] != rebuilt:
        errors.append("framing: the stream is not exactly one newline-terminated frame per reply")

    wanted = expect["frames"]
    if len(observed_frames) != len(wanted):
        label = ("frames-after-terminal"
                 if expect["outcome"] == "terminated" and len(observed_frames) > len(wanted)
                 else "frame-count")
        errors.append("%s: expected %d reply frame(s), observed %d"
                      % (label, len(wanted), len(observed_frames)))
    for index, (want, got) in enumerate(zip(wanted, observed_frames)):
        if want != got:
            errors.append("frame[%d]: expected %s, observed %s"
                          % (index, json.dumps(want, sort_keys=True, ensure_ascii=False),
                             json.dumps(got, sort_keys=True, ensure_ascii=False)))

    want_calls = expect["calls"]
    got_calls = observation["calls"]
    if len(want_calls) != len(got_calls):
        errors.append("calls: expected %d handler call(s), observed %d"
                      % (len(want_calls), len(got_calls)))
    for index, (want, got) in enumerate(zip(want_calls, got_calls)):
        if want["tool"] != got["tool"]:
            errors.append("call[%d].tool: expected %r, observed %r"
                          % (index, want["tool"], got["tool"]))
        if want["arguments"] != got["arguments"]:
            errors.append("call[%d].arguments: expected %r, observed %r"
                          % (index, want["arguments"], got["arguments"]))

    want_terminal = expect.get("terminal")
    got_terminal = observation.get("terminal")
    if want_terminal != got_terminal:
        errors.append("terminal: expected %r, observed %r" % (want_terminal, got_terminal))

    if "lines_total" in expect and observation.get("lines_total") != expect["lines_total"]:
        errors.append("lines-total: expected %r, observed %r"
                      % (expect["lines_total"], observation.get("lines_total")))
    if observation.get("lines_unprocessed") != expect["lines_unprocessed"]:
        errors.append("lines-unprocessed: expected %r line(s) never processed, observed %r"
                      % (expect["lines_unprocessed"], observation.get("lines_unprocessed")))
    if observation.get("dispatch_unused"):
        errors.append("dispatch: %d declared handler outcome(s) were never consumed"
                      % observation["dispatch_unused"])
    return errors


# ── profile integrity ───────────────────────────────────────────────────────


def _method_name(prefix, label):
    return "test_%s_%s" % (prefix, re.sub(r"\W+", "_", label).strip("_"))


def registration_errors(cases) -> list:
    """Every declared case must reach the runner as its OWN executable test.

    Inherited from the MH01 round-3 finding: `_method_name` collapses runs of
    non-word characters, so two DISTINCT legal ids can generate one method name
    and the second `setattr` silently replaces the first. The manifest cannot
    see it — it compares raw id sets and a count, and both stay correct — so a
    declared obligation would never run while the suite reported green.
    """
    errors, by_id, by_name = [], {}, {}
    for index, case in enumerate(cases):
        cid = case["id"]
        if cid in by_id:
            errors.append("case id %r is declared twice, at positions %d and %d"
                          % (cid, by_id[cid], index))
        else:
            by_id[cid] = index
        for prefix in ("python", "rust"):
            name = _method_name(prefix, cid)
            owner = by_name.get(name)
            if owner is not None and owner != cid:
                errors.append("cases %r and %r both install as %r, so the later silently "
                              "replaces the earlier and its assertions never run"
                              % (owner, cid, name))
            else:
                by_name.setdefault(name, cid)
    return errors


def manifest_errors(profile: dict) -> list:
    errors = []
    manifest = profile.get("case_manifest")
    if not isinstance(manifest, dict):
        errors.append("the profile carries no case manifest, so nothing states how many "
                      "cases were meant to exist")
        return errors
    cases = profile.get("cases") or []
    ids = [case["id"] for case in cases]
    if manifest.get("count") != len(cases):
        errors.append("the manifest declares %r cases but the profile carries %d"
                      % (manifest.get("count"), len(cases)))
    if manifest.get("ids") != sorted(ids):
        missing = sorted(set(manifest.get("ids") or []) - set(ids))
        extra = sorted(set(ids) - set(manifest.get("ids") or []))
        errors.append("the manifest's id list disagrees with the cases (missing %s, extra %s)"
                      % (missing, extra))
    declared_obligations = sum(1 for case in cases if "unimplemented" in case)
    if manifest.get("obligation_count") != declared_obligations:
        errors.append("the manifest declares %r obligation cases but %d carry one"
                      % (manifest.get("obligation_count"), declared_obligations))
    return errors


def obligation_errors(profile: dict) -> list:
    errors = []
    declared = {row["id"]: row for row in profile.get("obligations") or []}
    for row in declared.values():
        for field in ("id", "implementation", "summary", "exercised_by", "disposition"):
            if not row.get(field):
                errors.append("obligation %r records no %s" % (row.get("id"), field))
    used = set()
    for case in profile.get("cases") or []:
        for target, oid in (case.get("unimplemented") or {}).items():
            used.add(oid)
            if oid not in declared:
                errors.append("case %r names the undeclared obligation %r" % (case["id"], oid))
            elif declared[oid]["implementation"] != target:
                errors.append("case %r names obligation %r for %r, which is declared for %r"
                              % (case["id"], oid, target, declared[oid]["implementation"]))
    for oid, row in declared.items():
        if row.get("exercised_by") == "profile case" and oid not in used:
            errors.append("obligation %r claims a profile case exercises it, and none does" % oid)
    return errors


def profile_errors(profile: dict, anchors: Anchors) -> list:
    errors = list(source_errors(profile, anchors))
    errors.extend(manifest_errors(profile))
    errors.extend(registration_errors(profile.get("cases") or []))
    errors.extend(obligation_errors(profile))
    if profile.get("schema") != "orgtree.mailhub-mcp-envelope/v1":
        errors.append("the profile does not declare the expected schema")
    if not profile.get("tools_placeholder"):
        errors.append("the profile declares no tool-card placeholder")
    # Secrets never enter a fixture. Scan the CASES only; the source digests
    # are long hex strings by design.
    if re.search(r"\b[0-9a-f]{32,}\b", json.dumps(profile.get("cases") or [])):
        errors.append("the profile contains what looks like a literal credential or digest")
    return errors


def coverage_errors(profile: dict) -> list:
    """Every branch the source really has must be exercised by some case."""
    errors = []
    families = {}
    for case in profile.get("cases") or []:
        families.setdefault(case["id"].split(".", 1)[0], []).append(case["id"])
    for required in ("initialize", "tools-list", "tools-call", "arguments", "name", "params",
                     "outcome", "malformed-arguments", "unknown-method", "id", "framing",
                     "terminal", "obligation"):
        if not families.get(required):
            errors.append("no case exercises the %r family" % required)
    names = {call["tool"]
             for case in profile.get("cases") or []
             for call in case["expect"]["calls"]}
    declared = {card["name"] for card in profile.get("tools") or []}
    if not declared <= names:
        errors.append("these declared tools are never forwarded by any case: %s"
                      % sorted(declared - names))
    outcomes = {kind
                for case in profile.get("cases") or []
                for kind in (entry["kind"] for entry in case.get("dispatch") or [])}
    if outcomes != {"text", "url_error", "exception"}:
        errors.append("the three handler outcomes are not all exercised: %s" % sorted(outcomes))
    if not any(case["expect"]["outcome"] == "terminated" for case in profile["cases"]):
        errors.append("no case exercises the source's terminal outcome")
    if not any(case["expect"]["frames"] == [] and case["expect"]["outcome"] == "completed"
               for case in profile["cases"]):
        errors.append("no case exercises a silent, no-reply outcome")
    return errors


# ── mutations ───────────────────────────────────────────────────────────────
# Bad EXPECTATIONS: the checker must reject each one. Bad IMPLEMENTATIONS: the
# observation is broken the way a real defective port would break it, and a
# NAMED behavioural assertion must be the thing that catches it.


def _clone(value):
    return copy.deepcopy(value)


def _find(cases, cid):
    return _clone(next(case for case in cases if case["id"] == cid))


def _redeclare(profile):
    """Re-derive the manifest, so a mutated profile is one a well-behaved
    author could have written. The point is that the manifest AGREES and the
    defect survives anyway."""
    cases = profile["cases"]
    profile["case_manifest"] = {
        **profile["case_manifest"],
        "count": len(cases),
        "ids": sorted(case["id"] for case in cases),
        "obligation_count": sum(1 for case in cases if "unimplemented" in case),
    }
    return profile


def _collide_case(profile):
    wide = _clone(profile)
    twin = _find(wide["cases"], "initialize.string-id")
    twin["id"] = "initialize_string_id"
    wide["cases"].insert(0, twin)
    return _redeclare(wide)


def _duplicate_case(profile):
    wide = _clone(profile)
    wide["cases"].insert(0, _clone(wide["cases"][0]))
    return _redeclare(wide)


def _drop_case(profile):
    thin = _clone(profile)
    thin["cases"] = [case for case in thin["cases"] if case["id"] != "terminal.string"]
    return thin


def _drop_tool_card(profile):
    thin = _clone(profile)
    thin["tools"] = thin["tools"][:-1]
    return thin


IMPLEMENTATION_CONTROLS = []


def implementation_control(label, case_id, expected_label):
    def register(patch):
        IMPLEMENTATION_CONTROLS.append((label, case_id, expected_label, patch))
        return patch
    return register


@implementation_control("a top-level error member instead of a result",
                        "outcome.generic-exception", "frame[0]")
def _bad_error_member(observation, profile):
    frame = json.loads(observation["frames"][0])
    text = frame["result"]["content"][0]["text"]
    frame = {"jsonrpc": "2.0", "id": frame["id"], "error": {"code": -32603, "message": text}}
    return _reframe(observation, [frame])


@implementation_control("a reply to an unknown notification",
                        "unknown-method.missing-id-is-silent", "frame-count")
def _bad_notification_reply(observation, profile):
    return _reframe(observation, [{"jsonrpc": "2.0", "id": None, "result": {}}])


@implementation_control("id 0 treated as absent and dropped",
                        "unknown-method.answered-with-zero-id", "frame-count")
def _bad_zero_id_dropped(observation, profile):
    return _reframe(observation, [])


@implementation_control("one tool card omitted from tools/list",
                        "tools-list.string-id", "frame[0]")
def _bad_missing_card(observation, profile):
    frame = json.loads(observation["frames"][0])
    frame["result"]["tools"] = frame["result"]["tools"][:-1]
    return _reframe(observation, [frame])


@implementation_control("a tool card's description quietly reworded",
                        "tools-list.string-id", "frame[0]")
def _bad_reworded_card(observation, profile):
    frame = json.loads(observation["frames"][0])
    frame["result"]["tools"][0]["description"] = "Join the mail hub."
    return _reframe(observation, [frame])


@implementation_control("the unreachable-hub sentence reworded",
                        "outcome.unreachable-hub", "frame[0]")
def _bad_error_text(observation, profile):
    frame = json.loads(observation["frames"][0])
    frame["result"]["content"][0]["text"] = json.dumps({"error": "hub unreachable"})
    return _reframe(observation, [frame])


@implementation_control("the handler call suppressed while the reply is kept",
                        "tools-call.forwards-hub-send", "calls")
def _bad_suppressed_call(observation, profile):
    broken = _clone(observation)
    broken["calls"] = []
    return broken


@implementation_control("the handler called with the wrong arguments",
                        "arguments.two-char-string-element", "call[0].arguments")
def _bad_arguments(observation, profile):
    broken = _clone(observation)
    broken["calls"][0]["arguments"] = {"ab": True}
    return broken


@implementation_control("a frame processed after the source's terminal outcome",
                        "terminal.string", "frames-after-terminal")
def _bad_processes_after_terminal(observation, profile):
    broken = _reframe(observation, [{"jsonrpc": "2.0", "id": "after",
                                     "result": profile["server"]}])
    broken["lines_unprocessed"] = 0
    broken["lines_processed"] = broken["lines_total"]
    return broken


@implementation_control("the terminal outcome swallowed and the loop continued",
                        "terminal.integer", "outcome")
def _bad_swallowed_terminal(observation, profile):
    broken = _clone(observation)
    broken["outcome"] = "completed"
    broken["terminal"] = None
    broken["lines_unprocessed"] = 0
    return broken


@implementation_control("two frames merged onto one line",
                        "outcome.two-calls-in-order", "framing")
def _bad_framing(observation, profile):
    broken = _clone(observation)
    broken["frames"] = ["%s %s" % (observation["frames"][0], observation["frames"][1])]
    broken["raw"] = broken["frames"][0] + "\n"
    return broken


def _reframe(observation, frames):
    broken = _clone(observation)
    broken["frames"] = [json.dumps(frame, ensure_ascii=False) for frame in frames]
    broken["raw"] = "".join(frame + "\n" for frame in broken["frames"])
    return broken


# ── loading and collection ──────────────────────────────────────────────────


def load_profile():
    return json.loads(FIXTURES.read_text(encoding="utf-8"))


PROFILE = load_profile()
ANCHORS = Anchors(PROFILE["source_commit"])
CASES = PROFILE["cases"]
ORACLE = Oracle(ANCHORS)

PYTHON_OBSERVATIONS = {}
RUST_OBSERVATIONS = {}
RUST_IDENTITY = None
RUST_RECEIPT = None
RUST_BLOCKER = None


def collect_python():
    for case in CASES:
        PYTHON_OBSERVATIONS[case["id"]] = ORACLE.observe(case)


def collect_rust():
    global RUST_IDENTITY, RUST_RECEIPT, RUST_BLOCKER
    if TARGET == "python":
        # A partial run must never read as a pass: the rust tests stay
        # installed and fail, naming the exclusion.
        RUST_BLOCKER = ("the rust target was excluded by --target=python, so its half of "
                        "the profile did not run")
        return
    driver = Path(RUST_DRIVER) if RUST_DRIVER else None
    if driver is None:
        RUST_BLOCKER = ("no Rust driver was named: pass --rust-driver or set "
                        "MH02_RUST_DRIVER to the built `envelope_probe` executable")
        return
    if not driver.is_file():
        RUST_BLOCKER = "the named Rust driver %s does not exist" % driver
        return
    try:
        with DriverSession(driver) as session:
            RUST_IDENTITY = session.request({"op": "identify"})
            for case in CASES:
                answer = session.request({"op": "run", "id": case["id"],
                                          "input": case["input"],
                                          "dispatch": case.get("dispatch") or []})
                RUST_OBSERVATIONS[case["id"]] = rust_observation(answer)
        RUST_RECEIPT = session.receipt()
        if session.cleanup_errors:
            RUST_BLOCKER = "the driver did not clean up: %s" % "; ".join(session.cleanup_errors)
    except (DriverTimeout, DriverFault, OSError) as error:
        RUST_BLOCKER = "%s: %s" % (type(error).__name__, error)


def identity_errors():
    errors = []
    if not RUST_IDENTITY:
        return ["the driver never identified itself"]
    if RUST_IDENTITY.get("source_commit") != PROFILE["source_commit"]:
        errors.append("the driver is built against %r, the profile pins %r"
                      % (RUST_IDENTITY.get("source_commit"), PROFILE["source_commit"]))
    if RUST_IDENTITY.get("protocol_version") != PROFILE["server"]["protocolVersion"]:
        errors.append("the driver reports protocol %r, the source declares %r"
                      % (RUST_IDENTITY.get("protocol_version"),
                         PROFILE["server"]["protocolVersion"]))
    if RUST_IDENTITY.get("server_info") != PROFILE["server"]["serverInfo"]:
        errors.append("the driver reports serverInfo %r, the source declares %r"
                      % (RUST_IDENTITY.get("server_info"), PROFILE["server"]["serverInfo"]))
    embedded = RUST_IDENTITY.get("tools_json")
    canonical = json.dumps(ANCHORS.cards, indent=2, ensure_ascii=False)
    if embedded != canonical:
        errors.append("the cards embedded in the crate are not byte-identical to the ones "
                      "re-derived from the pinned TOOLS literal")
    return errors


# ── the test class ──────────────────────────────────────────────────────────


def _expect_empty(errors):
    if errors:
        raise AssertionError("\n  - " + "\n  - ".join(errors))


class MH02Contract(unittest.TestCase):
    """The frozen envelope profile, one test per case per target."""


def _install_tests():
    installed_python = []
    installed_rust = []

    def add(prefix, label, fn):
        fn.__name__ = _method_name(prefix, label)
        fn.__doc__ = label
        setattr(MH02Contract, fn.__name__, fn)

    # ── registry ────────────────────────────────────────────────────────────
    add("registry", "the profile matches the pinned source and declares every case once",
        lambda self: _expect_empty(profile_errors(PROFILE, ANCHORS)))
    add("registry", "the profile covers every branch the source really has",
        lambda self: _expect_empty(coverage_errors(PROFILE)))
    add("registry", "the extracted AST still has the shape this oracle was written against",
        lambda self: _expect_empty(shape_errors(ANCHORS)))

    # ── one test per case, per target ───────────────────────────────────────
    for case in CASES:
        unimplemented = (case.get("unimplemented") or {})

        def python_case(self, c=case):
            observation = PYTHON_OBSERVATIONS.get(c["id"])
            self.assertIsNotNone(observation, "the oracle produced no observation")
            _expect_empty(case_errors(c, observation, PROFILE))

        add("python", case["id"], python_case)
        installed_python.append(case["id"])

        if "rust" in unimplemented:
            def rust_case(self, c=case, oid=unimplemented["rust"]):
                if RUST_BLOCKER:
                    self.fail("environment blocker: %s" % RUST_BLOCKER)
                errors = case_errors(c, RUST_OBSERVATIONS[c["id"]], PROFILE)
                self.assertTrue(
                    errors,
                    "this case is DECLARED unimplemented on rust under obligation %r, and the "
                    "implementation now satisfies it. That is a stale declaration, not a pass: "
                    "retire the obligation." % oid)
        else:
            def rust_case(self, c=case):
                if RUST_BLOCKER:
                    self.fail("environment blocker: %s" % RUST_BLOCKER)
                _expect_empty(case_errors(c, RUST_OBSERVATIONS[c["id"]], PROFILE))

        add("rust", case["id"], rust_case)
        installed_rust.append(case["id"])

    # Count what the class ACTUALLY carries, not what we meant to install.
    # registration_errors rejects the known collision before the run; this is
    # the independent check that no declared obligation went missing by some
    # other route, and it measures the class rather than trusting the loop.
    for prefix, intended in (("python", installed_python), ("rust", installed_rust)):
        actual = sum(1 for name in vars(MH02Contract) if name.startswith("test_%s_" % prefix))
        add("registry", "every declared case is installed as its own %s test" % prefix,
            lambda self, want=len(CASES), got=actual, p=prefix: self.assertEqual(
                want, got,
                "%d cases are declared but %d are installed as %s tests, so a declared "
                "obligation is not being executed" % (want, got, p)))

    # ── the driver is an environment requirement, never a skip ──────────────
    add("registry", "a built Rust driver is present and identifies as the pinned build",
        lambda self: (self.fail("environment blocker: %s" % RUST_BLOCKER) if RUST_BLOCKER
                      else _expect_empty(identity_errors())))

    # ── controls against a bad EXPECTATION ──────────────────────────────────
    first = CASES[0]
    terminal = _find(CASES, "terminal.string")

    def _mutate(case, path, value):
        broken = _clone(case)
        target = broken["expect"]
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = value
        return broken

    expectation_controls = [
        ("a case whose expected frame is wrong must fail",
         lambda: _expect_empty(case_errors(
             _mutate(first, ["frames", 0, "result", "protocolVersion"], "1999-01-01"),
             PYTHON_OBSERVATIONS[first["id"]], PROFILE))),
        ("a case that declares an extra frame must fail",
         lambda: _expect_empty(case_errors(
             _mutate(first, ["frames"], first["expect"]["frames"] * 2),
             PYTHON_OBSERVATIONS[first["id"]], PROFILE))),
        ("a case whose expected terminal message is wrong must fail",
         lambda: _expect_empty(case_errors(
             _mutate(terminal, ["terminal"], {"kind": "TypeError", "message": "nope"}),
             PYTHON_OBSERVATIONS[terminal["id"]], PROFILE))),
        ("a case whose unprocessed-line count is wrong must fail",
         lambda: _expect_empty(case_errors(
             _mutate(terminal, ["lines_unprocessed"], 0),
             PYTHON_OBSERVATIONS[terminal["id"]], PROFILE))),
        ("a case that expects the wrong handler call must fail",
         lambda: _expect_empty(case_errors(
             _mutate(_find(CASES, "tools-call.forwards-hub-send"), ["calls", 0, "tool"],
                     "hub_read"),
             PYTHON_OBSERVATIONS["tools-call.forwards-hub-send"], PROFILE))),
        ("a duplicated case id must be caught",
         lambda: _expect_empty(profile_errors(_duplicate_case(PROFILE), ANCHORS))),
        ("a generated-name collision must be caught",
         lambda: _expect_empty(profile_errors(_collide_case(PROFILE), ANCHORS))),
        ("deleting a single case from the profile must be caught",
         lambda: _expect_empty(profile_errors(_drop_case(PROFILE), ANCHORS))),
        ("a profile whose manifest is stripped must be caught",
         lambda: _expect_empty(profile_errors(
             {key: value for key, value in PROFILE.items() if key != "case_manifest"},
             ANCHORS))),
        ("a profile pinned to a different source digest must be caught",
         lambda: _expect_empty(profile_errors(
             {**PROFILE, "source": {**PROFILE["source"], "sha256": "0" * 64}}, ANCHORS))),
        ("a profile pinned to a different commit must be caught",
         lambda: _expect_empty(source_errors({**PROFILE, "source_commit": "0" * 40}, ANCHORS))),
        ("a profile whose tool cards were edited must be caught",
         lambda: _expect_empty(profile_errors(_drop_tool_card(PROFILE), ANCHORS))),
        ("dropping a whole case family must be caught by coverage",
         lambda: _expect_empty(coverage_errors(
             {**PROFILE, "cases": [c for c in CASES if not c["id"].startswith("terminal.")]}))),
        ("an obligation nobody exercises must be caught",
         lambda: _expect_empty(obligation_errors({
             **PROFILE,
             "obligations": PROFILE["obligations"] + [
                 {"id": "invented", "implementation": "rust", "summary": "x",
                  "exercised_by": "profile case", "disposition": "x"}]}))),
        ("a case naming an undeclared obligation must be caught",
         lambda: _expect_empty(obligation_errors({
             **PROFILE,
             "cases": CASES + [{"id": "x", "expect": {"calls": []},
                                "unimplemented": {"rust": "not-declared"}}]}))),
    ]
    for label, thunk in expectation_controls:
        def control(self, t=thunk):
            with self.assertRaises(AssertionError):
                t()
        add("control", label, control)

    # ── controls against a bad IMPLEMENTATION ───────────────────────────────
    for label, case_id, expected_label, patch in IMPLEMENTATION_CONTROLS:
        def implementation(self, l=label, cid=case_id, want=expected_label, p=patch):
            case = _find(CASES, cid)
            broken = p(PYTHON_OBSERVATIONS[cid], PROFILE)
            errors = case_errors(case, broken, PROFILE)
            self.assertTrue(errors, "%r produced no failure at all" % l)
            self.assertTrue(
                any(error.startswith(want + ":") for error in errors),
                "%r had to break the %r assertion; it broke %s instead"
                % (l, want, [error.split(":")[0] for error in errors]))
        add("implementation", label, implementation)

    # ── the guard, the driver and cleanup ───────────────────────────────────
    add("guard", "the audit guard refuses a file open inside the oracle's region",
        lambda self: _guard_control(self))
    add("guard", "the oracle namespace carries no import, no os and no socket",
        lambda self: _namespace_control(self))
    add("cleanup", "a driver session leaves no process behind on success",
        lambda self: _cleanup_success(self))
    add("cleanup", "a driver session leaves no process behind when an assertion fails",
        lambda self: _cleanup_on_failure(self))
    add("cleanup", "a driver session leaves no process behind when a read times out",
        lambda self: _cleanup_on_timeout(self))
    add("cleanup", "the run created no temporary root and owns no other process",
        lambda self: _no_temporary_roots(self))


def _guard_control(self):
    with self.assertRaises(GuardViolation):
        with guarded():
            open(__file__, "rb").close()                          # noqa: SIM115
    # And the guard must be off again afterwards, or every later read dies.
    with open(__file__, "rb") as handle:
        self.assertTrue(handle.read(1))


def _namespace_control(self):
    namespace = {
        "__builtins__": dict(SERVE_BUILTINS),
        "TOOLS": [], "cast": lambda _t, v: v, "dispatch": lambda *_a: "",
        "json": json, "sys": SimpleNamespace(), "urllib": _URLLIB,
    }
    for forbidden in ("__import__", "open", "eval", "exec", "compile", "input", "getattr"):
        self.assertNotIn(forbidden, namespace["__builtins__"],
                         "%r must not be reachable from the extracted function" % forbidden)
    self.assertEqual(set(namespace["__builtins__"]), set(SERVE_BUILTINS))
    self.assertFalse(hasattr(namespace["urllib"], "request"),
                     "the oracle's urllib namespace must carry no URL opener")
    self.assertEqual(sorted(vars(namespace["urllib"])), ["error"])


def _require_driver(self):
    if RUST_BLOCKER:
        self.fail("environment blocker: %s" % RUST_BLOCKER)
    return Path(RUST_DRIVER)


def _cleanup_success(self):
    driver = _require_driver(self)
    with DriverSession(driver, timeout=30.0) as session:
        self.assertIn("source_commit", session.request({"op": "identify"}))
        pid = session.pid
    self.assertIsNotNone(session.returncode, "the driver was still running after close()")
    self.assertFalse(session.killed, "a clean close must not need a kill")
    self.assertEqual([], session.cleanup_errors)
    self.assertTrue(pid)


def _cleanup_on_failure(self):
    driver = _require_driver(self)
    session = DriverSession(driver, timeout=30.0)
    with self.assertRaises(AssertionError):
        with session:
            session.request({"op": "identify"})
            raise AssertionError("a deliberate assertion failure inside the session")
    self.assertIsNotNone(session.returncode, "an assertion failure must still close the driver")
    self.assertEqual([], session.cleanup_errors)


def _cleanup_on_timeout(self):
    driver = _require_driver(self)
    session = DriverSession(driver, timeout=30.0)
    with self.assertRaises(DriverTimeout):
        with session:
            # A budget no answer can meet, so the read really does time out.
            session.request({"op": "identify"}, timeout=0.000001)
    self.assertIsNotNone(session.returncode, "a timeout must still close the driver")
    self.assertNotIn("still running", " ".join(session.cleanup_errors))


def _no_temporary_roots(self):
    # This harness creates no temporary directory at all, and the assertion is
    # made against its own source rather than asserted in prose.
    tree = ast.parse(Path(__file__).read_text(encoding="utf-8"))
    imported = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            imported.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            imported.add(node.module.split(".")[0])
    for forbidden in ("tempfile", "shutil", "socket", "sqlite3", "http", "urllib.request"):
        self.assertNotIn(forbidden, imported,
                         "this harness must not reach for %r" % forbidden)
    if RUST_RECEIPT:
        self.assertIsNotNone(RUST_RECEIPT["returncode"],
                             "the collection driver is still running")
        self.assertEqual([], RUST_RECEIPT["cleanup_errors"])


collect_python()
collect_rust()
_install_tests()


# ── narrative entry point ───────────────────────────────────────────────────


def summary():
    obligations = [case["id"] for case in CASES if "unimplemented" in case]
    return {
        "schema": "orgtree.mailhub-mcp-envelope-run/v1",
        "source_commit": PROFILE["source_commit"],
        "source_sha256": PROFILE["source"]["sha256"],
        "cases": len(CASES),
        "targets": ["python", "rust"],
        "declared_obligations": sorted({case["unimplemented"]["rust"] for case in CASES
                                        if "unimplemented" in case}),
        "obligation_cases": obligations,
        "implementation_controls": len(IMPLEMENTATION_CONTROLS),
        "rust_driver": RUST_DRIVER or None,
        "rust_blocker": RUST_BLOCKER,
        "rust_receipt": RUST_RECEIPT,
    }


def main():
    print(json.dumps(summary(), indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    # `--narrative` prints the machine-readable run summary; the default is
    # unittest, because that is what the shared runner can count.
    if "--narrative" in sys.argv:
        raise SystemExit(main())
    unittest.main(argv=[sys.argv[0]] + [a for a in _UNITTEST_ARGV if a != "-v"],
                  verbosity=2 if VERBOSE else 1)
