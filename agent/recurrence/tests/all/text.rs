use tickloop_agent_recurrence::Recurrence;

fn canonical(text: &str) -> String {
    Recurrence::parse(text).unwrap().to_string()
}

#[test]
fn canonical_text_normalises_spacing_case_units_and_order() {
    for (input, expected) in [
        ("every 30s", "every 30s"),
        ("every 90s", "every 90s"),
        ("every 120S", "every 2m"),
        ("every 3600s", "every 1h"),
        ("every 86400s", "every 1d"),
        ("every 30m", "every 30m"),
        ("every 90m", "every 90m"),
        ("every 60m", "every 1h"),
        ("every 1440m", "every 1d"),
        ("every 48h", "every 2d"),
        ("every 007m", "every 7m"),
        ("  EVERY\t2H ", "every 2h"),
        ("daily 09:00", "daily 09:00"),
        ("daily 09:00 +00:00", "daily 09:00"),
        ("daily 09:00 -00:00", "daily 09:00"),
        ("daily 09:00 +08:00", "daily 09:00 +08:00"),
        ("Daily 23:59 -05:30", "daily 23:59 -05:30"),
        ("weekly fri,mon 09:00", "weekly mon,fri 09:00"),
        (
            "weekly SUN,sat,Wed 18:30 +05:45",
            "weekly wed,sat,sun 18:30 +05:45",
        ),
        (
            "at 2026-10-20T08:00:00+08:00",
            "at 2026-10-20T08:00:00+08:00",
        ),
        ("at 2026-10-20T00:00:00+00:00", "at 2026-10-20T00:00:00Z"),
        ("at 2026-10-20t00:00:00.250z", "at 2026-10-20T00:00:00.250Z"),
    ] {
        assert_eq!(canonical(input), expected, "{input}");
    }
}

#[test]
fn canonical_text_parses_back_to_an_equal_rule() {
    for text in [
        "every 1s",
        "every 59s",
        "every 61s",
        "every 1m",
        "every 59m",
        "every 25h",
        "every 366d",
        "daily 00:00",
        "daily 12:34 -14:00",
        "daily 12:34 +14:00",
        "weekly mon,tue,wed,thu,fri,sat,sun 07:30 +03:30",
        "weekly sun 00:00",
        "at 2026-10-20T08:00:00.123456789-07:00",
        "at 2016-12-31T23:59:60Z",
    ] {
        let parsed = Recurrence::parse(text).unwrap();
        let printed = parsed.to_string();
        assert_eq!(printed, text);
        assert_eq!(Recurrence::parse(&printed).unwrap(), parsed);
        assert_eq!(printed.parse::<Recurrence>().unwrap(), parsed);
    }
}

#[test]
fn serde_uses_the_canonical_string() {
    let recurrence = Recurrence::parse("weekly fri,mon 09:00 +08:00").unwrap();
    let json = serde_json::to_string(&recurrence).unwrap();
    assert_eq!(json, "\"weekly mon,fri 09:00 +08:00\"");
    assert_eq!(
        serde_json::from_str::<Recurrence>(&json).unwrap(),
        recurrence
    );

    let error = serde_json::from_str::<Recurrence>("\"every 0m\"").unwrap_err();
    assert!(error.to_string().contains("out of range"), "{error}");
    assert!(serde_json::from_str::<Recurrence>("30").is_err());
}
