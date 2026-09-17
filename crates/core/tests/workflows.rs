//! Instructor scores are arbitrary bounded transforms of a pinned public result.
use chrono::{Duration, Utc};
use grading_core::protocol::{PublicBaseline, ScriptScore};
use uuid::Uuid;

#[test]
fn private_scripts_can_choose_any_bounded_score() {
    let baseline = PublicBaseline {
        run_id: Uuid::new_v4(),
        points: 18,
        deadline: Utc::now() - Duration::hours(1),
    };
    for points in [18 / 2, 0, 18 - 2, 20] {
        ScriptScore {
            schema_version: 1,
            points,
            invalidated: false,
            reason: "Instructor policy".into(),
        }
        .validate(20, Some(&baseline))
        .unwrap();
    }
    for points in [-1, 21] {
        assert!(
            ScriptScore {
                schema_version: 1,
                points,
                invalidated: false,
                reason: "Reason".into()
            }
            .validate(20, Some(&baseline))
            .is_err()
        );
    }
    assert!(
        ScriptScore {
            schema_version: 1,
            points: 9,
            invalidated: false,
            reason: String::new()
        }
        .validate(20, Some(&baseline))
        .is_err()
    );
    assert!(
        ScriptScore {
            schema_version: 1,
            points: 18,
            invalidated: true,
            reason: "Review".into()
        }
        .validate(20, None)
        .is_err()
    );
}
