use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tickloop_harness_runtime::codemode::{Options, StoreWrites, run};

#[tokio::test]
async fn store_and_load_keep_json_values_and_report_successful_writes_only() {
    let stored = BTreeMap::from([
        ("cursor".to_owned(), json!({"page": 1})),
        ("stale".to_owned(), json!("old")),
    ]);
    let result = run(
        r#"
        const cursor = load("cursor");
        text(JSON.stringify([cursor, load("missing"), load("stale")]));
        store("cursor", {page: cursor.page + 1});
        store("empty", null);
        store("stale", undefined);
        text(String(load("stale")));
        cursor.page = 99;
        text(JSON.stringify(load("cursor")));
        "#,
        Options {
            store: stored.clone(),
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "[{\"page\":1},null,\"old\"]\nundefined\n{\"page\":2}"
    );
    assert_eq!(
        result.store_writes,
        StoreWrites {
            set: BTreeMap::from([
                ("cursor".to_owned(), json!({"page": 2})),
                ("empty".to_owned(), json!(null)),
            ]),
            delete: BTreeSet::from(["stale".to_owned()]),
        }
    );
    let failed = run(
        "store('cursor', 5); throw new Error('boom');",
        Options {
            store: stored,
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert!(failed.error.is_some());
    assert_eq!(failed.store_writes, StoreWrites::default());
    let exited = run(
        "store('cursor', 7); exit();",
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(exited.error, None);
    assert_eq!(exited.store_writes.set["cursor"], json!(7));
}

#[tokio::test]
async fn store_limits_count_key_and_json_characters() {
    let result = run(
        r#"
        const errors = [];
        for (const attempt of [
            () => store("big", "x".repeat(262144)),
            () => store(1, "number key"),
            () => store("function", () => 1),
            () => load(2),
        ]) {
            try { attempt(); errors.push("stored"); } catch (error) { errors.push(error.name); }
        }
        for (let index = 0; index < 4; index++) store("fill" + index, "x".repeat(262100));
        try { store("over", "x".repeat(100)); errors.push("stored"); } catch (error) { errors.push(error.name); }
        store("fill0", undefined);
        store("over", "x".repeat(100));
        text(errors.join(","));
        "#,
        Options {
            store: BTreeMap::from([("seed".to_owned(), json!("x".repeat(100)))]),
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "RangeError,TypeError,TypeError,TypeError,RangeError"
    );
    assert!(!result.store_writes.set.contains_key("fill0"));
    assert!(
        result.store_writes.delete.is_empty(),
        "keys created and deleted in one script leave no write"
    );
    assert!(result.store_writes.set.contains_key("over"));
    assert!(!result.store_writes.set.contains_key("big"));
}

#[tokio::test]
async fn tampered_builtins_cannot_bypass_store_limits() {
    let seed = BTreeMap::from([("seed".to_owned(), json!("kept"))]);
    for source in [
        r#"
        Map.prototype.has = () => true;
        Map.prototype.get = () => "x".repeat(1048576);
        for (let index = 0; index < 5; index++) store("fill" + index, "x".repeat(262100));
        "#,
        r#"
        Map.prototype.has = () => true;
        Map.prototype.get = () => "";
        for (let index = 0; index < 5000; index++) store("k" + index, index);
        "#,
    ] {
        let result = run(
            source,
            Options {
                store: seed.clone(),
                ..Default::default()
            },
            vec![],
            |_, _, _| async { unreachable!() },
        )
        .await;
        assert!(result.error.is_some(), "{source}");
        assert_eq!(result.store_writes, StoreWrites::default(), "{source}");
    }
    let result = run(
        r#"
        Array.prototype.toJSON = function () { return [["huge", JSON.stringify("x".repeat(300000))]]; };
        Object.defineProperty(Array.prototype, Symbol.iterator, {value: function* () { yield "injected"; }});
        store("small", 1);
        "#,
        Options {
            store: seed,
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.store_writes,
        StoreWrites {
            set: BTreeMap::from([("small".to_owned(), json!(1))]),
            delete: BTreeSet::new(),
        }
    );
}
