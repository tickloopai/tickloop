use super::*;

fn definition(name: &str) -> ToolDefinition {
    definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .expect("goal tool is defined")
}

#[test]
fn definitions_name_the_three_goal_tools_with_closed_schemas() {
    let names = definitions()
        .into_iter()
        .map(|definition| {
            assert_eq!(definition.input_schema["type"], "object");
            assert_eq!(definition.input_schema["additionalProperties"], false);
            definition.name
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            GET_GOAL_TOOL_NAME,
            CREATE_GOAL_TOOL_NAME,
            UPDATE_GOAL_TOOL_NAME
        ]
    );
}

#[test]
fn update_goal_schema_offers_only_complete_and_blocked() {
    let schema = definition(UPDATE_GOAL_TOOL_NAME).input_schema;
    assert_eq!(
        schema["properties"]["status"]["enum"],
        json!(["complete", "blocked"])
    );
    assert_eq!(schema["required"], json!(["status"]));
}

#[test]
fn create_goal_schema_and_description_state_its_limits() {
    let create = definition(CREATE_GOAL_TOOL_NAME);
    assert_eq!(
        create.input_schema["properties"]["objective"]["maxLength"],
        MAX_OBJECTIVE_CHARACTERS
    );
    assert_eq!(create.input_schema["required"], json!(["objective"]));
    assert!(create.description.contains("explicitly asks"));
    assert!(create.description.contains("unfinished goal"));
}

#[test]
fn get_goal_takes_no_arguments() {
    assert_eq!(parse_get_goal(&json!({})), Ok(()));
    assert!(
        parse_get_goal(&json!({"id": "goal"}))
            .unwrap_err()
            .starts_with("invalid GetGoal input:")
    );
}

#[test]
fn create_goal_trims_the_objective_and_keeps_an_explicit_budget() {
    assert_eq!(
        parse_create_goal(&json!({"objective": "  Ship it  ", "token_budget": 5000})),
        Ok(NewGoal {
            objective: "Ship it".into(),
            token_budget: Some(5000),
        })
    );
    assert_eq!(
        parse_create_goal(&json!({"objective": "Ship it"})),
        Ok(NewGoal {
            objective: "Ship it".into(),
            token_budget: None,
        })
    );
}

#[test]
fn create_goal_rejects_empty_oversized_and_unbudgeted_input() {
    let at_limit = "字".repeat(MAX_OBJECTIVE_CHARACTERS);
    assert!(parse_create_goal(&json!({"objective": at_limit})).is_ok());

    for (input, reason) in [
        (json!({"objective": "  "}), "objective is empty"),
        (
            json!({"objective": "字".repeat(MAX_OBJECTIVE_CHARACTERS + 1)}),
            "exceeds 4000 characters",
        ),
        (
            json!({"objective": "Ship it", "token_budget": 0}),
            "must be positive",
        ),
        (
            json!({"objective": "Ship it", "token_budget": -1}),
            "invalid value",
        ),
        (json!({}), "missing field `objective`"),
        (
            json!({"objective": "Ship it", "status": "active"}),
            "unknown field `status`",
        ),
    ] {
        let error = parse_create_goal(&input).unwrap_err();
        assert!(error.starts_with("invalid CreateGoal input:"), "{error}");
        assert!(error.contains(reason), "{error}");
    }
}

#[test]
fn update_goal_reports_completion_with_evidence_or_a_blocker_with_reason() {
    assert_eq!(
        parse_update_goal(&json!({"status": "complete", "evidence": " tests pass "})),
        Ok(GoalReport::Complete {
            evidence: "tests pass".into(),
        })
    );
    assert_eq!(
        parse_update_goal(&json!({"status": "blocked", "reason": "no credentials"})),
        Ok(GoalReport::Blocked {
            reason: "no credentials".into(),
        })
    );
}

#[test]
fn update_goal_rejects_other_statuses_and_mismatched_fields() {
    for (input, reason) in [
        (json!({"status": "paused"}), "unknown variant `paused`"),
        (json!({"status": "active"}), "unknown variant `active`"),
        (
            json!({"status": "complete"}),
            "status `complete` requires evidence",
        ),
        (
            json!({"status": "complete", "evidence": " "}),
            "status `complete` requires evidence",
        ),
        (
            json!({"status": "complete", "evidence": "done", "reason": "x"}),
            "status `complete` does not take reason",
        ),
        (
            json!({"status": "blocked"}),
            "status `blocked` requires reason",
        ),
        (
            json!({"status": "blocked", "reason": "stuck", "evidence": "x"}),
            "status `blocked` does not take evidence",
        ),
        (json!({}), "missing field `status`"),
    ] {
        let error = parse_update_goal(&input).unwrap_err();
        assert!(error.starts_with("invalid UpdateGoal input:"), "{error}");
        assert!(error.contains(reason), "{error}");
    }
}
