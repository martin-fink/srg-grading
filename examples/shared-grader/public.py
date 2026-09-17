import json
from pathlib import Path
from run import run

cases = json.loads(Path("/submission/tests/public.json").read_text())
points = sum(case["points"] for case in cases if run(case))
print(json.dumps({"schema_version": 1, "points": points}))
