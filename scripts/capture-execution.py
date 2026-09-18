"""Keep scoring stdout separate from bounded compiler/runtime diagnostics."""
import ctypes
import json
import subprocess
import sys
import tempfile

# Fail closed: student processes must not inspect the supervisor's descriptors or
# memory even though they share its unprivileged UID. exec resets dumpability for
# the child only. Signals remain possible and are rejected via the Pod exit code.
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(4, 0, 0, 0, 0) != 0:  # PR_SET_DUMPABLE
    raise OSError(ctypes.get_errno(), "cannot protect execution supervisor")

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
