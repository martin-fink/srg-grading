import json
from integrity import INTEGRITY_FAILURE, verified_public_cases
from run import run

def grade():
    cases = verified_public_cases()
    if cases is None:
        return INTEGRITY_FAILURE
    points = sum(case["points"] for case in cases if run(case))
    return {"schema_version": 1, "points": points}


if __name__ == "__main__":
    print(json.dumps(grade()))
