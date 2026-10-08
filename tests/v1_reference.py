"""The v1 Python hub, materialised from git history.

v2.0.0 removed the Python server from the tree. Two kinds of tests still use
it: the side-by-side comparisons (tests/v2/), where it is the reference v2
must answer like, and hubtool's own suite (tests/test_hubtool.py), where it
is the in-process hub the client talks to. Both import it from here: the
`mailhub` package exactly as it was at V1_COMMIT (the last v1 hub, with the
1 GiB attachment limit), extracted once into a cache folder.

    from v1_reference import v1_root
    sys.path.insert(0, v1_root())
    from mailhub import app
"""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import tarfile
import tempfile

V1_COMMIT = "79a7c51"
_REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))


def v1_root() -> str:
    """A folder holding v1's `mailhub` package (needs this repository's git
    history: a shallow clone without V1_COMMIT cannot provide it)."""
    cache = os.path.join(tempfile.gettempdir(), f"orgtree-mailhub-v1-{V1_COMMIT}")
    if os.path.isfile(os.path.join(cache, "mailhub", "app.py")):
        return cache
    tar = subprocess.run(["git", "-C", _REPO, "archive", "--format=tar", V1_COMMIT, "mailhub"],
                         capture_output=True, check=True).stdout
    partial = f"{cache}.partial-{os.getpid()}"
    shutil.rmtree(partial, ignore_errors=True)
    os.makedirs(partial)
    with tarfile.open(fileobj=io.BytesIO(tar)) as t:
        t.extractall(partial)
    try:
        os.replace(partial, cache)
    except OSError:                      # another run got there first
        shutil.rmtree(partial, ignore_errors=True)
    return cache
