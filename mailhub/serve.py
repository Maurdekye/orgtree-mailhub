"""Hub entrypoint: the full app (UI + API) on HUB_PORT, and — when
HUB_PUBLIC is set — the FR-10 public listener (API-only, see public.py) on
the fixed internal port 7371. Compose maps that to a host port; the expose
tooling tunnels the HOST side, never 7370.

HUB_BIND (default 0.0.0.0) is which interface the FULL app binds. Inside
Docker this stays 0.0.0.0 and the compose port mapping controls exposure —
exactly as before. It exists for NON-Docker hosting (an embedding desktop
process), where there is no port mapping and loopback-only must be
expressible at the app itself. HUB_PUBLIC_BIND (default 0.0.0.0) is the
same knob for the public listener: every route it serves is authenticated
and remote reachability is its usual purpose, so the default stays wide —
but non-Docker hosting that fronts it with a proxy or tunnel can now pin
it to loopback the same way, instead of the interface being the one thing
the two listeners configure differently.

    python -m mailhub.serve
"""

from __future__ import annotations

import asyncio
import os

import uvicorn

from .app import app
from .public import PublicHub


def main() -> None:
    port = int(os.environ.get("HUB_PORT", "7370"))
    bind = (os.environ.get("HUB_BIND") or "").strip() or "0.0.0.0"
    servers = [uvicorn.Server(uvicorn.Config(app, host=bind, port=port))]
    if (os.environ.get("HUB_PUBLIC") or "").strip():
        public_bind = (os.environ.get("HUB_PUBLIC_BIND") or "").strip() \
            or "0.0.0.0"
        servers.append(uvicorn.Server(uvicorn.Config(
            PublicHub(app), host=public_bind, port=7371)))

    async def serve_all() -> None:
        await asyncio.gather(*(s.serve() for s in servers))

    if len(servers) == 1:
        uvicorn.run(app, host=bind, port=port)
        return
    asyncio.run(serve_all())


if __name__ == "__main__":
    main()
