"""Keep scoring stdout separate from bounded compiler/runtime diagnostics."""
import ctypes
import json
import subprocess
import sys
import os
import selectors
import signal

# Fail closed: student processes must not inspect the supervisor's descriptors or
# memory even though they share its unprivileged UID. exec resets dumpability for
# the child only. Signals remain possible and are rejected via the Pod exit code.
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(4, 0, 0, 0, 0) != 0:  # PR_SET_DUMPABLE
    raise OSError(ctypes.get_errno(), "cannot protect execution supervisor")

LIMIT = 65536
buffers = {"stdout": bytearray(), "stderr": bytearray()}
exit_code = 127
failure = None
try:
    process = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ, "stdout")
        selector.register(process.stderr, selectors.EVENT_READ, "stderr")
        while selector.get_map():
            for key, _ in selector.select():
                chunk = os.read(key.fd, 8192)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                buffer = buffers[key.data]
                remaining = LIMIT - len(buffer)
                buffer.extend(chunk[:remaining])
                if len(chunk) > remaining:
                    failure = "output_limit"
                    break
            if failure:
                break
    if failure:
        os.killpg(process.pid, signal.SIGKILL)
    exit_code = process.wait() if not failure else 125
    if failure:
        process.wait()
except OSError as error:
    buffers["stderr"].extend(str(error).encode("utf-8", errors="replace")[:LIMIT])
print(json.dumps({
    "stdout": buffers["stdout"].decode("utf-8", errors="replace"),
    "stderr": buffers["stderr"].decode("utf-8", errors="replace"),
    "exit_code": exit_code,
    "failure": failure,
}), flush=True)
