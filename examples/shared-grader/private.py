import json
from pathlib import Path
from integrity import INTEGRITY_FAILURE, verified_public_cases
from run import run

def grade(context):
    assert context["phase"] == "private"
    if verified_public_cases() is None:
        return INTEGRITY_FAILURE
    public_points = context["public_points"]
    cases = json.loads((Path(__file__).parent / "private/cases.json").read_text())
    failed = any(not run(case) for case in cases)
    points = public_points // 2 if failed else public_points
    return {
        "schema_version": 1,
        "points": points,
        "reason": "Additional inputs failed; public score halved." if failed else "",
    }


if __name__ == "__main__":
    context = json.loads(Path("/grading/input.json").read_text())
    print(json.dumps(grade(context)))
