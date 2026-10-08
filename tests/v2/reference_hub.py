"""Run the v1 Python hub (the reference for the v2 port) on chosen ports.

The v1 server left the tree in v2.0.0; it is taken from git history at
tests/v1_reference.py's V1_COMMIT.

v1's `python -m mailhub.serve` fixed the public listener at 7371; this launcher
serves the same two ASGI apps (the full app and the FR-10 PublicHub around
it) on any two loopback ports, so a comparison never touches a real hub's
ports. Configuration is v1's own: HUB_* variables set before the import.

    python tests/v2/reference_hub.py --port P --public-port Q --data DIR
"""

from __future__ import annotations

import argparse
import asyncio
import os
import sys

_TESTS = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--public-port", type=int, required=True)
    ap.add_argument("--data", required=True)
    ap.add_argument("--name", default="diff-hub")
    ap.add_argument("--max-file-bytes", default="")
    args = ap.parse_args()
    os.environ["HUB_DATA"] = args.data
    os.environ["HUB_NAME"] = args.name
    os.environ.setdefault("HUB_RETENTION_DAYS", "30")
    if args.max_file_bytes:
        os.environ["HUB_MAX_FILE_BYTES"] = args.max_file_bytes
    sys.path.insert(0, _TESTS)
    from v1_reference import v1_root

    sys.path.insert(0, v1_root())
    import uvicorn

    from mailhub.app import app
    from mailhub.public import PublicHub

    servers = [
        uvicorn.Server(uvicorn.Config(app, host="127.0.0.1", port=args.port, log_level="warning", access_log=False)),
        uvicorn.Server(uvicorn.Config(PublicHub(app), host="127.0.0.1", port=args.public_port,
                                      log_level="warning", access_log=False)),
    ]

    async def serve_all() -> None:
        await asyncio.gather(*(s.serve() for s in servers))

    asyncio.run(serve_all())


if __name__ == "__main__":
    main()
