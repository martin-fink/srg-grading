"""Expected public-test contents pinned in the instructor-owned grader."""
import hashlib
import json
from pathlib import Path


PUBLIC_TESTS = Path("/submission/tests/public.json")
PUBLIC_TESTS_SHA256 = "f2404c721732a97831de8bee225d9d108cd6e2ce437b1808250fa2b21cb64ffb"
INTEGRITY_FAILURE = {
    "schema_version": 1,
    "points": 0,
    "reason": "Public tests are missing, unreadable, or modified; awarded zero points.",
}


def verified_public_cases():
    try:
        contents = PUBLIC_TESTS.read_bytes()
    except OSError:
        return None
    if hashlib.sha256(contents).hexdigest() != PUBLIC_TESTS_SHA256:
        return None
    # Parse the same bytes that were verified, without reopening student files.
    return json.loads(contents)
