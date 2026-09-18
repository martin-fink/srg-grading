#!/usr/bin/env python3
"""Local recipe/compilation smoke test; no cluster or database required."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


examples = Path(__file__).resolve().parent
compiler = shutil.which("cc")
if compiler is None:
    raise SystemExit("A C compiler named cc is required (for example, use nix develop).")


def run(command, **kwargs):
    return subprocess.run(command, check=True, text=True, capture_output=True, **kwargs)


with tempfile.TemporaryDirectory(prefix="grading-cache-example-") as temporary:
    root = Path(temporary)
    output = root / "output"
    recipe = examples / "shared-grader/cache"
    prepared = run(["bash", str(recipe / "prepare.sh")], env={
        **os.environ, "CC": compiler, "RECIPE_DIR": str(recipe), "OUTPUT_DIR": str(output),
    })
    print(prepared.stdout.strip())
    seed = output / "launcher/launcher.o"
    seed.chmod(0o444)
    original_digest = hashlib.sha256(seed.read_bytes()).hexdigest()
    original_mtime = seed.stat().st_mtime_ns
    source = root / "main.c"
    shutil.copyfile(examples / "scripted-template/src/main.c", source)
    program = root / "program"

    def compile_and_check(cases):
        run([compiler, "-Dmain=student_main", str(source), str(seed), "-o", str(program)])
        for case in cases:
            actual = run([str(program)], input=case["input"]).stdout
            if actual != case["expected"]:
                raise SystemExit(f"Unexpected program output: {actual!r}")
        if hashlib.sha256(seed.read_bytes()).hexdigest() != original_digest:
            raise SystemExit("Cache seed changed during grading")
        if seed.stat().st_mtime_ns != original_mtime:
            raise SystemExit("Cache seed was rebuilt during grading")

    cases = json.loads((examples / "scripted-template/tests/public.json").read_text())
    cases += json.loads((examples / "shared-grader/private/cases.json").read_text())
    compile_and_check(cases)
    print("PASS: public and private echo cases use the prepared object")
    source.touch()
    compile_and_check(cases)
    print("PASS: touching student source preserves correct output and reuses the seed")
    source.write_text('#include <stdio.h>\nint main(void) { puts("changed"); return 0; }\n')
    compile_and_check([{"input": "", "expected": "changed\n"}])
    print("PASS: changing student code changes output while reusing the seed")
    seed.unlink()
    missing = subprocess.run(
        [compiler, "-Dmain=student_main", str(source), str(seed), "-o", str(program)],
        capture_output=True, text=True,
    )
    if missing.returncode == 0:
        raise SystemExit("Compilation unexpectedly succeeded without its cache dependency")
    print("PASS: missing seed fails compilation")

print("Local cache example passed. Cluster preparation/reuse needs the registration check in shared-grader/README.md.")
