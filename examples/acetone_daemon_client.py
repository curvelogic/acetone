#!/usr/bin/env python3
"""A minimal, dependency-free acetone daemon client — in a language that is
not Rust.

`acetone serve` (ADR-0074) speaks a length-prefixed JSON frame protocol over
a local unix domain socket, deliberately so that any language can drive a
full session without an acetone library. This script is the worked example:
stdlib only, ~100 lines, and it runs read AND write queries.

Wire protocol (ADR-0074):
  * Every frame is a 4-byte big-endian unsigned length, then that many bytes
    of UTF-8 JSON.
  * On connect the daemon sends a hello frame {"acetone": {"protocol": N,
    "version": "..."}}; the client replies {"acetone": {"protocol": N}}.
  * A request is {"id": <any>, "verb": "query", "params": {"cypher": "..."}}.
  * The reply is zero or more {"id", "row": {"columns", "values"}} frames,
    then zero or more {"id", "advisory": "..."} frames, then EXACTLY ONE
    terminal {"id", "ok": {...}} or {"id", "error": {"kind", "message"}}.
    A write's terminal ok carries a "write" object with the mutation counts.

Usage:
    acetone serve --repo path/to/repo --socket /tmp/acetone.sock &
    python3 acetone_daemon_client.py /tmp/acetone.sock
"""

import json
import socket
import struct
import sys

PROTOCOL = 1


class AcetoneClient:
    """One connection to an `acetone serve` daemon. One request at a time
    (open more connections for concurrency — the daemon multiplexes them)."""

    def __init__(self, socket_path):
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(socket_path)
        hello = self._read_frame()
        peer = hello.get("acetone", {}).get("protocol")
        if peer != PROTOCOL:
            raise RuntimeError(f"daemon speaks protocol {peer}, this client speaks {PROTOCOL}")
        self._send_frame({"acetone": {"protocol": PROTOCOL}})
        self._next_id = 0

    def query(self, cypher):
        """Run one openCypher query (read or write). Returns (columns, rows,
        summary): rows is a list of value-lists, summary is the terminal `ok`
        payload (with a `write` object for a write). Raises on a typed error."""
        self._next_id += 1
        req_id = self._next_id
        self._send_frame({"id": req_id, "verb": "query", "params": {"cypher": cypher}})
        columns, rows = None, []
        while True:
            frame = self._read_frame()
            if "row" in frame:
                columns = frame["row"]["columns"]
                rows.append(frame["row"]["values"])
            elif "advisory" in frame:
                print(f"  (advisory) {frame['advisory']}", file=sys.stderr)
            elif "ok" in frame:
                return columns or [], rows, frame["ok"]
            elif "error" in frame:
                err = frame["error"]
                raise RuntimeError(f"{err.get('kind', 'error')}: {err.get('message', '')}")
            else:
                raise RuntimeError(f"unexpected frame: {frame}")

    def close(self):
        self._sock.close()

    # --- framing ---------------------------------------------------------
    def _send_frame(self, obj):
        body = json.dumps(obj).encode("utf-8")
        self._sock.sendall(struct.pack(">I", len(body)) + body)

    def _read_exactly(self, n):
        buf = bytearray()
        while len(buf) < n:
            chunk = self._sock.recv(n - len(buf))
            if not chunk:
                raise RuntimeError("daemon closed the connection mid-frame")
            buf.extend(chunk)
        return bytes(buf)

    def _read_frame(self):
        (length,) = struct.unpack(">I", self._read_exactly(4))
        return json.loads(self._read_exactly(length).decode("utf-8"))


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    client = AcetoneClient(sys.argv[1])
    try:
        # A write: create a node. The terminal ok carries the mutation counts.
        _, _, summary = client.query("CREATE (:Demo {id: 'from-python'})")
        print("wrote:", summary.get("write"))

        # A read: the write is visible on the same live workspace.
        columns, rows, _ = client.query("MATCH (d:Demo) RETURN d.id")
        print("read:", columns, "->", rows)
    finally:
        client.close()


if __name__ == "__main__":
    main()
