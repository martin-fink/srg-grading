"""Keep scoring stdout separate from bounded compiler/runtime diagnostics."""
import json
import subprocess
import sys
import tempfile

with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
    try:
        process = subprocess.run(sys.argv[1:], stdout=stdout, stderr=stderr, check=False)
        exit_code = process.returncode
    except OSError as error:
        stderr.write(str(error).encode("utf-8", errors="replace"))
        exit_code = 127
    stdout.seek(0)
    stderr.seek(0)
    print(json.dumps({
        "stdout": stdout.read(65537).decode("utf-8", errors="replace"),
        "stderr": stderr.read(65537).decode("utf-8", errors="replace"),
        "exit_code": exit_code,
    }), flush=True)
