//! The agent hub's session operations as clients send them over `ultron.session-ops`.

use serde_json::json;
use session_core::core::Op;

#[test]
fn session_core_agent_ops_round_trip_as_json() {
    for (op, wire) in [
        (Op::Agents, json!({"op": "agents"})),
        (
            Op::KillAgent { id: "t1".into() },
            json!({"op": "killAgent", "id": "t1"}),
        ),
    ] {
        assert_eq!(op.to_json(), wire);
        assert_eq!(Op::from_json(&wire).unwrap(), op);
    }
    assert_eq!(
        Op::from_json(&json!({"op": "killAgent"})).unwrap_err(),
        "id must be a string"
    );
}
