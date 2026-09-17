import json
from pathlib import Path
from run import run

context = json.loads(Path("/grading/input.json").read_text())
assert context["phase"] == "private"
public_points = context["public_points"]
cases = json.loads((Path(__file__).parent / "private/cases.json").read_text())
failed = any(not run(case) for case in cases)
points = public_points // 2 if failed else public_points
print(json.dumps({
    "schema_version": 1,
    "points": points,
    "reason": "Additional inputs failed; public score halved." if failed else "",
}))
