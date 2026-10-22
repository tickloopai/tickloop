use std::fs;
use std::path::Path;

use tickloop_agent_config::loop_skill::{
    DEFAULT_INSTRUCTIONS_PROMPT, IntervalUnit, LoopFiles, LoopInstructionsSource, LoopInterval,
    LoopRequest, MAINTENANCE_INSTRUCTIONS, MAX_LOOP_FILE_BYTES, parse_loop_arguments,
};
use tickloop_agent_config::skills::{LOOP_SKILL, SkillCaller, SkillError, SkillRegistry};
use tickloop_agent_protocol::SkillSource;

fn interval(amount: u32, unit: IntervalUnit) -> LoopInterval {
    LoopInterval { amount, unit }
}

fn fixed(amount: u32, unit: IntervalUnit, prompt: &str) -> LoopRequest {
    LoopRequest::FixedInterval {
        interval: interval(amount, unit),
        prompt: prompt.into(),
    }
}

fn self_paced(prompt: &str) -> LoopRequest {
    LoopRequest::SelfPaced {
        prompt: prompt.into(),
    }
}

fn write_loop_file(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn an_interval_leads_as_a_bare_token_or_trails_as_an_every_clause() {
    for (arguments, request) in [
        (
            "5m /review-pr 1234",
            fixed(5, IntervalUnit::Minutes, "/review-pr 1234"),
        ),
        (
            "  30s   poll the queue ",
            fixed(30, IntervalUnit::Seconds, "poll the queue"),
        ),
        (
            "1d write the digest",
            fixed(1, IntervalUnit::Days, "write the digest"),
        ),
        (
            "check the deploy every 20m",
            fixed(20, IntervalUnit::Minutes, "check the deploy"),
        ),
        (
            "run tests every 5 minutes",
            fixed(5, IntervalUnit::Minutes, "run tests"),
        ),
        (
            "check the deploy Every 2 Hours",
            fixed(2, IntervalUnit::Hours, "check the deploy"),
        ),
        (
            "back up every 1 day",
            fixed(1, IntervalUnit::Days, "back up"),
        ),
        (
            "2h summarize every 5m",
            fixed(2, IntervalUnit::Hours, "summarize every 5m"),
        ),
    ] {
        assert_eq!(parse_loop_arguments(arguments), request, "{arguments}");
    }
}

#[test]
fn without_an_interval_the_whole_input_is_a_self_paced_prompt() {
    for prompt in [
        "check the deploy",
        "check every PR",
        "watch every 5 builds",
        "5 minutes of review",
        "0m is not an interval",
        "5M is not an interval",
        "every",
    ] {
        assert_eq!(parse_loop_arguments(prompt), self_paced(prompt), "{prompt}");
    }
}

#[test]
fn an_interval_alone_or_nothing_runs_the_loop_instructions() {
    for (arguments, expected) in [
        ("", None),
        ("   ", None),
        ("15m", Some(interval(15, IntervalUnit::Minutes))),
        ("every 2 hours", Some(interval(2, IntervalUnit::Hours))),
        ("every 90s", Some(interval(90, IntervalUnit::Seconds))),
    ] {
        assert_eq!(
            parse_loop_arguments(arguments),
            LoopRequest::DefaultInstructions { interval: expected },
            "{arguments:?}"
        );
    }
    assert_eq!(interval(90, IntervalUnit::Seconds).seconds(), 90);
    assert_eq!(interval(2, IntervalUnit::Hours).seconds(), 7200);
    assert_eq!(interval(2, IntervalUnit::Hours).to_string(), "2h");
}

#[test]
fn the_project_loop_file_comes_before_the_config_root_one() {
    let folder = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let project_file = folder.path().join(".tickloop/loop.md");
    let user_file = config_root.path().join("loop.md");
    let files = LoopFiles::new(Some(folder.path()), Some(config_root.path()));

    write_loop_file(&user_file, "\nUser tasks\n\n");
    let instructions = files.instructions().unwrap();
    assert_eq!(
        instructions.source,
        LoopInstructionsSource::User(user_file.clone())
    );
    assert_eq!(instructions.text, "User tasks");
    assert!(!instructions.truncated);

    write_loop_file(&project_file, "Project tasks");
    let instructions = files.instructions().unwrap();
    assert_eq!(
        instructions.source,
        LoopInstructionsSource::Project(project_file.clone())
    );
    assert_eq!(instructions.text, "Project tasks");
    assert!(
        instructions
            .prompt()
            .contains(&format!("from {}", project_file.display()))
    );

    write_loop_file(&project_file, "  \n");
    assert_eq!(
        files.instructions().unwrap().source,
        LoopInstructionsSource::User(user_file),
        "a blank loop.md counts as absent"
    );
}

#[test]
fn without_any_loop_file_the_maintenance_instructions_run() {
    let folder = tempfile::tempdir().unwrap();
    fs::create_dir_all(folder.path().join(".tickloop/loop.md")).unwrap();
    for files in [
        LoopFiles::new(Some(folder.path()), Some(folder.path())),
        LoopFiles::new(None, None),
    ] {
        let instructions = files.instructions().unwrap();
        assert_eq!(instructions.source, LoopInstructionsSource::Maintenance);
        assert_eq!(instructions.text, MAINTENANCE_INSTRUCTIONS.trim());
        assert!(
            instructions
                .prompt()
                .contains("built-in maintenance instructions")
        );
    }
    for duty in [
        "Continue unfinished work from this conversation",
        "Tend to the pull request of the current branch",
        "review comments, fix failed CI runs, resolve merge conflicts",
        "When nothing else is pending, run a cleanup pass",
        "Do not start new features or initiatives",
        "proceed only when they continue work this conversation already authorized",
    ] {
        assert!(MAINTENANCE_INSTRUCTIONS.contains(duty), "{duty}");
    }
}

#[test]
fn a_long_loop_file_is_cut_at_a_line_break_and_says_so() {
    let folder = tempfile::tempdir().unwrap();
    let line = "é".repeat(49) + "\n";
    let content = line.repeat(MAX_LOOP_FILE_BYTES / line.len() + 10);
    write_loop_file(&folder.path().join(".tickloop/loop.md"), &content);

    let instructions = LoopFiles::new(Some(folder.path()), None)
        .instructions()
        .unwrap();
    assert!(instructions.truncated);
    let (kept, note) = instructions.text.split_once("\n\n> ").unwrap();
    assert!(kept.len() <= MAX_LOOP_FILE_BYTES);
    assert!(content.starts_with(kept));
    assert!(kept.ends_with(&"é".repeat(49)));
    assert_eq!(
        note,
        format!("loop.md was truncated to {MAX_LOOP_FILE_BYTES} bytes. Keep it concise.")
    );

    let unbroken = "x".repeat(MAX_LOOP_FILE_BYTES + 1);
    write_loop_file(&folder.path().join(".tickloop/loop.md"), &unbroken);
    let instructions = LoopFiles::new(Some(folder.path()), None)
        .instructions()
        .unwrap();
    assert!(
        instructions
            .text
            .starts_with(&"x".repeat(MAX_LOOP_FILE_BYTES))
    );
    assert!(
        !instructions
            .text
            .contains(&"x".repeat(MAX_LOOP_FILE_BYTES + 1))
    );
}

#[test]
fn an_unreadable_loop_file_is_an_error_instead_of_other_work() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join(".tickloop/loop.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    let error = LoopFiles::new(Some(folder.path()), None)
        .instructions()
        .unwrap_err();
    assert_eq!(error.path, path);

    let registry = SkillRegistry::discover(folder.path(), None);
    assert!(matches!(
        registry.expand("loop", "", SkillCaller::User),
        Err(SkillError::LoopInstructionsUnavailable(_))
    ));
    assert!(
        registry
            .expand("loop", "5m check the deploy", SkillCaller::User)
            .is_ok()
    );
}

#[test]
fn loop_is_a_built_in_skill_the_user_and_the_model_can_invoke() {
    let folder = tempfile::tempdir().unwrap();
    for registry in [
        SkillRegistry::discover(folder.path(), None),
        SkillRegistry::builtin(),
    ] {
        assert!(registry.contains("loop"));
        assert!(!registry.is_user_only("loop"));
        assert!(registry.model_listing().contains("- loop [Built-in]: "));
        let expansion = registry
            .expand("/loop", "5m check the deploy", SkillCaller::User)
            .unwrap();
        assert_eq!(expansion.source, SkillSource::BuiltIn);
        assert!(expansion.allowed_tools.is_empty());
        assert!(
            registry
                .expand("loop", "check the deploy", SkillCaller::Model)
                .is_ok()
        );
    }
    assert!(!SkillRegistry::builtin_without_ticks().contains("loop"));
}

#[test]
fn the_expansion_gives_the_form_what_to_run_and_the_scheduled_prompt() {
    let folder = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(folder.path(), Some(config_root.path()));
    let expand = |arguments: &str| {
        registry
            .expand("loop", arguments, SkillCaller::User)
            .unwrap()
            .content
    };

    let content = expand("check the deploy every 20m");
    assert!(content.starts_with(LOOP_SKILL.split_once("\n---\n").unwrap().1.trim()));
    assert!(content.ends_with(
        "## This /loop\n\nInput: check the deploy every 20m\nForm: fixed interval, every 20m\nRun now: check the deploy\nScheduled prompt: check the deploy"
    ));
    assert!(!content.contains("Loop instructions ("));

    assert!(expand("check the deploy").ends_with(
        "Form: self-paced\nRun now: check the deploy\nScheduled prompt: /loop check the deploy"
    ));

    let content = expand("");
    assert!(content.contains(
        "Input: (none)\nForm: no prompt, self-paced\nRun now: the loop instructions below\nScheduled prompt: /loop\n\nLoop instructions (built-in maintenance instructions; no loop.md exists):\n\n"
    ));
    assert!(content.ends_with(MAINTENANCE_INSTRUCTIONS.trim()));

    write_loop_file(&config_root.path().join("loop.md"), "Check the release PR.");
    let content = expand("15m");
    assert!(content.contains(&format!(
        "Form: no prompt, fixed interval, every 15m\nRun now: the loop instructions below\nScheduled prompt: {DEFAULT_INSTRUCTIONS_PROMPT}\n\nLoop instructions (from {}):\n\nCheck the release PR.",
        config_root.path().join("loop.md").display()
    )));
}

#[test]
fn the_loop_skill_teaches_the_three_forms_with_the_scheduling_tools() {
    for contract in [
        "## Fixed interval",
        "## Self-paced",
        "## No prompt",
        "\"This /loop\"",
        "If CronCreate or ScheduleWakeup is not among your tools",
        "invoke that Skill with the Skill tool",
        "Call CronCreate with `cron` (the expression), `prompt` (the scheduled prompt) and `recurring: true`",
        "the host states the interval it actually uses",
        "the task id",
        "after 7 days",
        "CronDelete",
        "`x` on its row in the Monitor",
        "Then run it once now",
        "use the nearest interval that does and tell the user which one you picked",
        "`delaySeconds` (60 to 3600)",
        "`reason`",
        "`stop: true` and no other fields",
        "after about 20 minutes",
        "Esc stops it while it waits",
        "Esc does not stop a fixed-interval task",
        "`.tickloop/loop.md`",
        "`loop.md` in the Tickloop config root",
        "edits to loop.md take effect on the next run",
    ] {
        assert!(LOOP_SKILL.contains(contract), "{contract}");
    }
    for foreign in ["Claude", "session", "/babysit", "cache"] {
        assert!(!LOOP_SKILL.contains(foreign), "{foreign}");
        assert!(!MAINTENANCE_INSTRUCTIONS.contains(foreign), "{foreign}");
    }
}
