use tickloop_harness_protocol::{ExecutionMode, PermissionPolicy};

fn policy(mode_before_plan: Option<ExecutionMode>, plan_without_auto: bool) -> PermissionPolicy {
    PermissionPolicy {
        mode: ExecutionMode::Plan,
        mode_before_plan,
        plan_without_auto,
        ..PermissionPolicy::default()
    }
}

/// Plan decides as the mode it was entered from: Auto's admission only after Auto and with Auto in
/// Plan left on, Full Access after Full Access, and Ask otherwise, including an unknown mode.
#[test]
fn plan_admits_as_the_mode_it_was_entered_from() {
    for (mode_before_plan, plan_without_auto, admission) in [
        (Some(ExecutionMode::Auto), false, ExecutionMode::Auto),
        (Some(ExecutionMode::Auto), true, ExecutionMode::AcceptEdits),
        (
            Some(ExecutionMode::AcceptEdits),
            false,
            ExecutionMode::AcceptEdits,
        ),
        (
            Some(ExecutionMode::FullAccess),
            false,
            ExecutionMode::FullAccess,
        ),
        (
            Some(ExecutionMode::FullAccess),
            true,
            ExecutionMode::FullAccess,
        ),
        (None, false, ExecutionMode::AcceptEdits),
    ] {
        let policy = policy(mode_before_plan, plan_without_auto);
        let label = format!("{mode_before_plan:?} plan_without_auto={plan_without_auto}");
        assert_eq!(policy.admission_mode(), admission, "{label}");
        assert_eq!(
            policy.uses_auto_admission(),
            admission == ExecutionMode::Auto,
            "{label}"
        );
    }
}

#[test]
fn modes_outside_plan_admit_as_themselves() {
    for mode in [
        ExecutionMode::Auto,
        ExecutionMode::AcceptEdits,
        ExecutionMode::FullAccess,
    ] {
        let policy = PermissionPolicy {
            mode,
            ..PermissionPolicy::default()
        };
        assert_eq!(policy.admission_mode(), mode);
        assert_eq!(policy.uses_auto_admission(), mode == ExecutionMode::Auto);
    }
}
