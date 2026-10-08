# Attachment upload limit

The default is 1 GiB (1,073,741,824 bytes). Set `HUB_MAX_FILE_BYTES` to a positive
integer number of bytes to change the standalone hub's startup default.

`GET /healthz` advertises the current `max_attachment_bytes` on both listeners.
Clients should check it before uploading. Uploads stream to disk; the hub checks
both Content-Length, when supplied, and bytes actually received. Oversize or
interrupted uploads remove their partial file and publish no attachment row.

An embedding host can set `HUB_RUNTIME_CONFIG_FILE` to a local JSON file:

```json
{"max_attachment_bytes": 1073741824}
```

The host must replace this file atomically. Its value overrides the environment
default without restarting the hub. Each upload snapshots the limit when it
starts; a change applies to subsequent uploads. A missing file uses the startup
default; an invalid file returns 503 instead of silently widening the limit.

`python tests/test_hub.py` covers the environment override, public advertisement,
live updates, oversize and interrupted cleanup, and a 32 MiB streamed upload with
less than 8 MiB of traced Python allocation at peak.
