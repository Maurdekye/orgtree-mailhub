"""The one-line hubtool installers (install-hubtool.ps1 / install-hubtool.sh),
checked without running them. tests/install_hubtool_e2e.py runs them for real
in a throwaway home.

    * THE CHECKSUM IS A LOCKFILE. Each installer refuses any hubtool.py whose
      SHA-256 it does not name, so a hubtool.py change without
      `python tools/hubtool-assets.py --stamp` ships an installer that refuses
      its own release.
    * THE INSTALLERS IMPORT HUBTOOL. They read the stored default hub through
      hubtool's own functions; renaming those breaks every install.
    * BOTH SCRIPTS MUST PARSE, and the README must name the URLs they are
      released under.

    python tests/test_install_hubtool.py
"""

from __future__ import annotations

import importlib.util
import os
import shutil
import subprocess
import sys
import traceback

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.normpath(os.path.join(_HERE, ".."))
sys.path.insert(0, _REPO)

_spec = importlib.util.spec_from_file_location(
    "hubtool_assets", os.path.join(_REPO, "tools", "hubtool-assets.py"))
assert _spec and _spec.loader
assets = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(assets)

PASS = 0
FAIL: list[tuple[str, str]] = []
SKIP: list[str] = []
RELEASE = "https://github.com/Maurdekye/orgtree-mailhub/releases/latest/download"


def check(label, fn) -> None:
    global PASS
    try:
        fn()
    except Exception:                                            # noqa: BLE001
        FAIL.append((label, traceback.format_exc()))
        print(f"  FAIL     {label}")
        return
    PASS += 1
    print(f"  ok {PASS:3d}  {label}")


def read(name: str) -> str:
    with open(os.path.join(_REPO, name), encoding="utf-8") as f:
        return f.read()


def _installers_name_this_hubtool():
    files = {n: assets.working(n) for n in assets.ASSETS}
    bad = assets.problems(files)
    assert not bad, "; ".join(bad) + " (run python tools/hubtool-assets.py --stamp)"


def _hubtool_is_stored_with_lf():
    with open(os.path.join(_REPO, ".gitattributes"), encoding="utf-8") as f:
        attrs = f.read().split()
    assert "hubtool.py" in attrs, (
        "hubtool.py must be checked out with LF everywhere: the release asset "
        "and the checksum are over the committed (LF) bytes")


def _the_functions_the_installers_import_exist():
    import hubtool
    for name in ("_stored_default_hub", "_LOCAL_HUB"):
        for inst in assets.INSTALLERS:
            if f"hubtool.{name}" in read(inst):
                assert hasattr(hubtool, name), f"{inst} uses hubtool.{name}"
    assert "defaulthub" in read("install-hubtool.ps1")
    assert "defaulthub" in read("install-hubtool.sh")


def _sh_parses():
    sh = shutil.which("sh")
    if not sh:
        SKIP.append("sh -n (no sh on PATH)")
        return
    r = subprocess.run([sh, "-n", os.path.join(_REPO, "install-hubtool.sh")],
                       capture_output=True, text=True)
    assert r.returncode == 0, r.stderr


def _powershell_parses():
    ps = shutil.which("powershell") or shutil.which("pwsh")
    if not ps:
        SKIP.append("PowerShell parse (no powershell/pwsh on PATH)")
        return
    path = os.path.join(_REPO, "install-hubtool.ps1").replace("'", "''")
    script = ("$e = $null; [void][System.Management.Automation.Language.Parser]"
              f"::ParseFile('{path}', [ref]$null, [ref]$e); "
              "$e | ForEach-Object { $_.ToString() }")
    r = subprocess.run([ps, "-NoProfile", "-NonInteractive", "-Command", script],
                       capture_output=True, text=True)
    assert r.returncode == 0 and not r.stdout.strip(), r.stdout + r.stderr


def _readme_names_the_released_urls():
    readme = read("README.md")
    for name in assets.INSTALLERS:
        assert f"{RELEASE}/{name}" in readme, f"README lacks {RELEASE}/{name}"
    for name in assets.INSTALLERS:
        assert RELEASE in read(name), f"{name} does not download from {RELEASE}"


def _no_secret_or_exit_in_the_ps1():
    # `irm | iex` runs in the caller's own session: an `exit` would close
    # their PowerShell window
    text = read("install-hubtool.ps1")
    code = [ln for ln in text.splitlines() if not ln.lstrip().startswith("#")]
    assert not any(ln.strip().startswith("exit") for ln in code), \
        "install-hubtool.ps1 must return, never exit"


def main() -> int:
    print("orgtree · the one-line hubtool installers")
    check("both installers name the hubtool.py beside them (the lockfile)",
          _installers_name_this_hubtool)
    check("hubtool.py is checked out with LF on every platform",
          _hubtool_is_stored_with_lf)
    check("the hubtool functions the installers import still exist",
          _the_functions_the_installers_import_exist)
    check("install-hubtool.sh parses (sh -n)", _sh_parses)
    check("install-hubtool.ps1 parses (PowerShell parser)", _powershell_parses)
    check("the README names the release URLs the installers use",
          _readme_names_the_released_urls)
    check("install-hubtool.ps1 never calls exit (it runs in the caller's "
          "session)", _no_secret_or_exit_in_the_ps1)
    for s in SKIP:
        print(f"  skipped  {s}")
    if FAIL:
        for label, tb in FAIL:
            print(f"\n✗ {label}\n{tb}")
        print(f"install-hubtool: {PASS} passed · {len(FAIL)} FAILED")
        return 1
    print(f"install-hubtool: all {PASS} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
