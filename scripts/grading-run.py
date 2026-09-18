#!/bin/python3
"""Run one command in a fresh student sandbox through the trusted executor."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import sys
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--stdin", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--timeout-seconds", type=int, default=30)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command is required after --")
    if not 1 <= args.timeout_seconds <= 86400:
        parser.error("timeout must be 1..86400 seconds")
    data = args.stdin.read_text() if args.stdin else ""
    if len(data.encode()) > 65536:
        parser.error("stdin exceeds 64 KiB")
    control = Path("/control")
    request_id = str(uuid.uuid4())
    with (control / "client.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        temporary = control / f"{request_id}.tmp"
        temporary.write_text(json.dumps({"id": request_id, "command": command, "stdin": data, "timeout_seconds": args.timeout_seconds}))
        os.replace(temporary, control / "request.json")
        while True:
            try:
                response = json.loads((control / "response.json").read_text())
                if response["id"] == request_id:
                    break
            except (FileNotFoundError, json.JSONDecodeError):
                pass
            time.sleep(0.1)
    encoded = json.dumps(response)
    if args.output:
        args.output.write_text(encoded)
    else:
        print(encoded)


if __name__ == "__main__":
    main()
