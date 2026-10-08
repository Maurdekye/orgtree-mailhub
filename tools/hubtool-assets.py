"""The one-line hubtool installer's release assets.

The README's one-line commands fetch install-hubtool.ps1 / install-hubtool.sh
from the LATEST GitHub release, and the installer fetches hubtool.py from the
same place and refuses it unless its SHA-256 is the one written into the
installer. So every release of this repository must carry all three files,
and the installers must name the hubtool.py they ship with.

    python tools/hubtool-assets.py --stamp
        write sha256(hubtool.py) into both installers (after changing
        hubtool.py; tests/test_install_hubtool.py fails until you do)
    python tools/hubtool-assets.py --check
        exit 1 unless both installers name the working tree's hubtool.py
    python tools/hubtool-assets.py <out-dir> [<git-rev>]
        write the release assets exactly as committed at <git-rev> (default
        HEAD) plus SHA256SUMS, after checking that the installers at that
        revision name that revision's hubtool.py; then upload them:
            gh release upload <tag> <out-dir>/*

Standard library only.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys

REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
HUBTOOL = "hubtool.py"
INSTALLERS = ("install-hubtool.ps1", "install-hubtool.sh")
ASSETS = (HUBTOOL,) + INSTALLERS
# the one line in each installer that names the accepted hubtool.py
PATTERNS = {
    "install-hubtool.ps1": re.compile(r"^(\s*\$HubtoolSha256 = ')([^']*)(')", re.M),
    "install-hubtool.sh": re.compile(r"^(HUBTOOL_SHA256=)(\S*)()", re.M),
}


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical(data: bytes) -> bytes:
    """The bytes git stores for a text file: a Windows checkout may hold CRLF
    where the blob (and so the release asset) has LF."""
    return data.replace(b"\r\n", b"\n")


def named_sha(name: str, text: str) -> str:
    m = PATTERNS[name].search(text)
    if not m:
        raise SystemExit(f"{name}: no checksum line found")
    return m.group(2)


def working(name: str) -> bytes:
    with open(os.path.join(REPO, name), "rb") as f:
        return f.read()


def committed(name: str, rev: str) -> bytes:
    return subprocess.run(["git", "-C", REPO, "show", f"{rev}:{name}"],
                          check=True, capture_output=True).stdout


def problems(files: dict[str, bytes]) -> list[str]:
    want = sha256(canonical(files[HUBTOOL]))
    out = []
    for name in INSTALLERS:
        got = named_sha(name, files[name].decode("utf-8"))
        if got != want:
            out.append(f"{name} names hubtool.py {got!r}, but hubtool.py is {want}")
    return out


def stamp() -> int:
    want = sha256(canonical(working(HUBTOOL)))
    for name in INSTALLERS:
        path = os.path.join(REPO, name)
        with open(path, encoding="utf-8", newline="") as f:
            text = f.read()
        new = PATTERNS[name].sub(lambda m: m.group(1) + want + m.group(3), text, count=1)
        if new != text:
            with open(path, "w", encoding="utf-8", newline="") as f:
                f.write(new)
            print(f"{name}: now names hubtool.py {want}")
        else:
            print(f"{name}: already names hubtool.py {want}")
    return 0


def check() -> int:
    bad = problems({n: working(n) for n in ASSETS})
    for p in bad:
        print(p)
    if bad:
        print("run: python tools/hubtool-assets.py --stamp")
        return 1
    print("both installers name the working tree's hubtool.py")
    return 0


def build(out_dir: str, rev: str) -> int:
    files = {n: committed(n, rev) for n in ASSETS}
    bad = problems(files)
    if bad:
        for p in bad:
            print(p)
        print(f"refusing: the installers at {rev} do not name its hubtool.py")
        return 1
    os.makedirs(out_dir, exist_ok=True)
    sums = []
    for name, data in files.items():
        with open(os.path.join(out_dir, name), "wb") as f:
            f.write(data)
        sums.append(f"{sha256(data)}  {name}\n")
    with open(os.path.join(out_dir, "SHA256SUMS"), "w", encoding="utf-8", newline="\n") as f:
        f.writelines(sums)
    print(f"wrote {', '.join(ASSETS)} and SHA256SUMS from {rev} to {out_dir}")
    print(f"upload them: gh release upload <tag> {out_dir}/*")
    return 0


def main(argv: list[str]) -> int:
    if argv[:1] == ["--stamp"]:
        return stamp()
    if argv[:1] == ["--check"]:
        return check()
    if argv and not argv[0].startswith("-"):
        return build(argv[0], argv[1] if len(argv) > 1 else "HEAD")
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
