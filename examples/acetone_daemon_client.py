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
    # The demo imports/writes `Demo {id}` nodes; import needs a clean
    # workspace, so declare the label and commit before serving.
    acetone --repo path/to/repo declare-label Demo --key id
    acetone --repo path/to/repo commit -m setup
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

    def query(self, cypher, autodeclare=False):
        """Run one openCypher query (read or write). Returns (columns, rows,
        summary): rows is a list of value-lists, summary is the terminal `ok`
        payload (with a `write` object for a write). `autodeclare=True` opts a
        write into relationship-type coinage. Raises on a typed error."""
        params = {"cypher": cypher}
        if autodeclare:
            params["autodeclare"] = True
        return self._request({"verb": "query", "params": params})

    def status(self):
        """The workspace state (branch, head, dirty, node/edge counts)."""
        _, _, summary = self._request({"verb": "status"})
        return summary

    def schema_apply(self, document, dry_run=False):
        """Apply a schema document (JSON text) streamed as chunk frames — a
        payload verb: the document's bytes cross the wire, never a path
        (ADR-0074 §4). Returns the terminal `ok` summary."""
        self._next_id += 1
        req_id = self._next_id
        self._send_frame(
            {"id": req_id, "verb": "schema-apply", "params": {"dry_run": dry_run}}
        )
        self._send_frame({"id": req_id, "chunk": document})
        self._send_frame({"id": req_id, "chunk_end": True})
        _, _, summary = self._read_response()
        return summary

    def import_source(self, source, fmt, label=None, edge=None, message=None):
        """Import a source (CSV/JSON/NDJSON text) streamed as chunk frames — a
        payload verb, no path over the wire. Requires a clean workspace (it
        commits). Returns the terminal `ok` summary."""
        params = {"format": fmt}
        for key, value in (("label", label), ("edge", edge), ("message", message)):
            if value is not None:
                params[key] = value
        self._next_id += 1
        req_id = self._next_id
        self._send_frame({"id": req_id, "verb": "import", "params": params})
        self._send_frame({"id": req_id, "chunk": source})
        self._send_frame({"id": req_id, "chunk_end": True})
        _, _, summary = self._read_response()
        return summary

    def _request(self, body):
        self._next_id += 1
        self._send_frame({"id": self._next_id, **body})
        return self._read_response()

    def _read_response(self):
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
        # A payload verb first, while the workspace is clean (import commits):
        # stream two NDJSON rows as Demo nodes.
        imported = client.import_source(
            '{"id": "imported-1"}\n{"id": "imported-2"}\n', "ndjson", label="Demo"
        )
        print("imported:", imported)

        # A write: create a node. The terminal ok carries the mutation counts.
        _, _, summary = client.query("CREATE (:Demo {id: 'from-python'})")
        print("wrote:", summary.get("write"))

        # A read: the write is visible on the same live workspace.
        columns, rows, _ = client.query("MATCH (d:Demo) RETURN d.id")
        print("read:", columns, "->", rows)

        # A payload verb: apply a schema document streamed as chunk frames.
        applied = client.schema_apply('{"labels": [{"name": "Note", "key": ["id"]}]}')
        print("schema-applied:", applied)
        client.query("CREATE (:Note {id: 'n1'})")  # the new label is usable

        # Coin: a CREATE naming an undeclared relationship type, with
        # autodeclare, mints the type as it writes.
        _, _, summary = client.query(
            "MATCH (a:Demo {id: 'from-python'}) "
            "CREATE (a)-[:`relates to`]->(:Demo {id: 'sibling'})",
            autodeclare=True,
        )
        print("coined+wrote:", summary.get("write"))

        # Inspect the workspace state.
        print("status:", client.status())
    finally:
        client.close()


if __name__ == "__main__":
    main()
