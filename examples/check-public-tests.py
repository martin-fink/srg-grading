#!/usr/bin/env python3
"""Check the example's integrity policy locally, without a cluster or compiler."""
import json
from pathlib import Path
import sys
import tempfile
from unittest.mock import patch


examples = Path(__file__).resolve().parent
sys.path.insert(0, str(examples / "shared-grader"))
import integrity
import public
import private


original = (examples / "scripted-template/tests/public.json").read_bytes()
context = {"phase": "private", "public_points": 20}

with tempfile.TemporaryDirectory(prefix="grading-integrity-example-") as temporary:
    tests = Path(temporary) / "public.json"
    with patch.object(integrity, "PUBLIC_TESTS", tests):
        tests.write_bytes(original)
        with patch.object(public, "run", return_value=True), patch.object(private, "run", return_value=True):
            assert public.grade()["points"] == 20
            assert private.grade(context)["points"] == 20
            tests.touch()
            assert public.grade()["points"] == 20
        with patch.object(private, "run", return_value=False):
            assert private.grade(context)["points"] == 10
        print("PASS: original tests score normally; touching does not invalidate the checksum")

        inflated = json.loads(original)
        inflated[0]["points"] = 1000000
        changed_expected = json.loads(original)
        changed_expected[0]["expected"] = "cheating\n"
        variants = {
            "inflated points": json.dumps(inflated).encode(),
            "changed expected output": json.dumps(changed_expected).encode(),
            "removed cases": b"[]",
            "invalid JSON": b"broken",
            "whitespace changes": original + b"\n",
            "missing file": None,
        }
        for label, contents in variants.items():
            if contents is None:
                tests.unlink()
            else:
                tests.write_bytes(contents)
            with patch.object(public, "run") as public_run, patch.object(private, "run") as private_run:
                for result in (public.grade(), private.grade(context)):
                    assert result["points"] == 0, label
                    assert result["reason"], label
                public_run.assert_not_called()
                private_run.assert_not_called()
            print(f"PASS: {label} awards zero in both phases without executing student code")

print("Public-test integrity example passed.")
