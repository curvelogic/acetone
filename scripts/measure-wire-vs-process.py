#!/usr/bin/env python3
"""Measure the daemon's wire latency against one process per command.

The harness behind docs/protocol.md's "Measured: wire vs process-per-command"
section (ADR-0074, acetone-d5is.5). It builds a fresh 20-node repository,
then times `status` and a small read `query` N times each:

  * wire    — one warm connection to `acetone serve`, one request per op;
  * process — one `acetone` process spawn (and repository open) per op.

and prints the medians and their ratio.

Usage:
    cargo build --release -p acetone-cli
    python3 scripts/measure-wire-vs-process.py target/release/acetone [N]

Stdlib only; it reuses the frame client in examples/acetone_daemon_client.py.
"""

import os
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "examples"))
from acetone_daemon_client import AcetoneClient  # noqa: E402

NODES = 20
QUERY = "MATCH (n:Demo) RETURN count(n) AS n"


def run(binary, repo, *args):
    subprocess.run([binary, "--repo", repo, *args], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def median_ms(fn, n):
    samples = []
    for _ in range(n):
        start = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - start) * 1000.0)
    return statistics.median(samples)


def main():
    binary = os.path.abspath(sys.argv[1])
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 50
    with tempfile.TemporaryDirectory() as tmp:
        repo = os.path.join(tmp, "repo")
        sock = os.path.join(tmp, "acetone.sock")
        subprocess.run([binary, "init", repo], check=True, stdout=subprocess.DEVNULL)
        run(binary, repo, "declare-label", "Demo", "--key", "id")
        run(binary, repo, "commit", "-m", "schema")
        daemon = subprocess.Popen([binary, "serve", "--repo", repo, "--socket", sock],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(200):
                if os.path.exists(sock):
                    break
                time.sleep(0.05)
            client = AcetoneClient(sock)
            client.query("UNWIND range(1, $n) AS i CREATE (:Demo {id: i})",
                         parameters={"n": NODES})
            client.commit("seed")
            # Warm both paths once before timing.
            client.status()
            client.query(QUERY)
            run(binary, repo, "status")
            run(binary, repo, "query", QUERY)

            ops = {
                "status": (lambda: client.status(),
                           lambda: run(binary, repo, "status")),
                "query": (lambda: client.query(QUERY),
                          lambda: run(binary, repo, "query", QUERY)),
            }
            print(f"N={n} per operation, release binary, {NODES}-node repo")
            for name, (wire, proc) in ops.items():
                w = median_ms(wire, n)
                p = median_ms(proc, n)
                print(f"{name:<7}: wire {w:8.2f} ms | process {p:8.2f} ms | x {p / w:4.1f}")
            client.close()
        finally:
            daemon.terminate()
            daemon.wait()


if __name__ == "__main__":
    main()
