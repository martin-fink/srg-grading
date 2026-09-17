import json
from pathlib import Path


def grade(results, source):
    invalidated = any(not test["passed"] for test in results["private_tests"])
    adjustment = 0
    reason = ""
    if invalidated:
        reason = "The solution failed additional inputs; instructor review required."
    elif "TODO" in source and results["public_points"] >= 2:
        adjustment = -2
        reason = "Remove unfinished TODO markers from the submitted solution."
    return {
        "schema_version": 1,
        "adjustment": adjustment,
        "invalidated": invalidated,
        "reason": reason,
    }


if __name__ == "__main__":
    # Student execution happens in separate Pods; only outcomes/source enter this process.
    results = json.loads(Path("/public/results.json").read_text())
    source = Path("/submission/src/solution.sh").read_text()
    print(json.dumps(grade(results, source)))
