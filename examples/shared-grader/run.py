"""Instructor-owned compilation and execution policy for this example exercise."""
import json
from pathlib import Path
import subprocess


def run(case):
    Path("/tmp/input").write_text(case["input"])
    response = subprocess.check_output([
        "/platform/grading-run", "--stdin", "/tmp/input", "--",
        "/bin/sh", "-c",
        "/bin/cc src/main.c -o /workspace/program && /workspace/program",
    ], text=True)
    execution = json.loads(response)
    return (
        execution["exit_code"] == 0
        and not execution["truncated"]
        and execution["stdout"] == case["expected"]
    )
