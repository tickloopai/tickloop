use tickloop_ids::{Id, IdContext, Kind, ModelRef, ResolveError, base62, not_found};
use uuid::Uuid;

enum Task {}

impl Kind for Task {
    const NOUN: &'static str = "scheduled task";
    const SOURCE: &'static str = "CronList gives each task's id.";
}

enum Prefixed {}

impl Kind for Prefixed {
    const NOUN: &'static str = "task";
    const MODEL_PREFIX: &'static str = "shell:";
}

#[test]
fn new_ids_are_v7_and_stored_as_hyphenated_uuids() {
    let id = Id::<Task>::new();
    assert_eq!(id.uuid().get_version_num(), 7);
    let stored = serde_json::to_value(id).unwrap();
    assert_eq!(stored, serde_json::json!(id.uuid().to_string()));
    let read: Id<Task> = serde_json::from_value(stored).unwrap();
    assert_eq!(read, id);
    let from_public: Id<Task> = serde_json::from_value(serde_json::json!(id.public())).unwrap();
    assert_eq!(from_public, id);
    assert!(serde_json::from_value::<Id<Task>>(serde_json::json!("x")).is_err());
}

#[test]
fn public_and_model_text_are_base62_and_the_prefix_is_the_kinds() {
    let ids = IdContext::new();
    let id = Id::<Prefixed>::new();
    assert_eq!(id.public(), base62::encode(id.uuid()));
    assert_eq!(ids.model_text(id), format!("shell:{}", id.public()));
    assert_eq!(ids.model_text(id.cast::<Task>()), id.public());
}

#[test]
fn resolves_every_accepted_form() {
    let ids = IdContext::new();
    let id = Id::<Prefixed>::new();
    for text in [
        ids.model_text(id),
        id.public(),
        id.uuid().to_string(),
        id.uuid().to_string().to_uppercase(),
        format!("shell:{}", id.uuid()),
        format!(" {} ", id.public()),
    ] {
        assert_eq!(
            ids.resolve(&ModelRef::<Prefixed>::new(text.clone())),
            Ok(id),
            "{text}"
        );
    }
    let foreign = Id::<Task>::new();
    assert_eq!(
        ids.resolve(&ModelRef::<Task>::from(ids.model_text(foreign))),
        Ok(foreign)
    );
}

#[test]
fn unknown_or_malformed_values_are_echoed_with_where_to_find_the_id() {
    let ids = IdContext::new();
    let Err(ResolveError::Malformed { message }) = ids.resolve(&ModelRef::<Task>::from("abc"))
    else {
        panic!("abc is no id");
    };
    assert_eq!(
        message,
        "No scheduled task \"abc\" found. CronList gives each task's id."
    );
    assert_eq!(not_found::<Prefixed>("x"), "No task \"x\" found.");
    let long = "y".repeat(100);
    assert_eq!(
        not_found::<Prefixed>(&long),
        format!("No task \"{}…\" found.", "y".repeat(64))
    );
}

#[test]
fn state_errors_name_the_object_in_model_text() {
    let ids = IdContext::new();
    let id = Id::<Prefixed>::from_uuid(Uuid::now_v7());
    assert_eq!(
        ids.state_error(id, "is not running (status: completed)"),
        format!(
            "Task {} is not running (status: completed)",
            ids.model_text(id)
        )
    );
}

#[test]
fn random_tokens_are_random_from_the_first_character() {
    let first = tickloop_ids::random_token();
    assert_eq!(first.len(), 32);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let prefixes: std::collections::HashSet<String> = (0..64)
        .map(|_| tickloop_ids::random_token()[..6].to_owned())
        .collect();
    assert!(prefixes.len() > 60);
}
