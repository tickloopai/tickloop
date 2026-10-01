use longbridge::{
    calendar::CalendarEventsResponse,
    fundamental::types::{FinancialConsensus, USKeyFinancialMetrics},
};
use serde_json::json;
use tickloop_ticks::cli::parse_probe_date;
use tickloop_ticks::probe::{
    CallOutcome, CallReport, ProbeReport, calendar_findings, consensus_findings, excerpt,
    field_names, key_metrics_findings, markdown,
};

fn consensus() -> FinancialConsensus {
    serde_json::from_value(json!({
        "currency": "USD",
        "current_index": 1,
        "current_period": "qf",
        "opt_periods": ["qf", "saf", "af"],
        "list": [
            {
                "fiscal_year": 2026,
                "fiscal_period": "Q2",
                "period_text": "Q2 FY2026",
                "details": [
                    {"key":"eps","name":"EPS","description":"","actual":"","estimate":"0.55",
                     "comp_value":"","comp_desc":"","comp":"","is_released":false},
                    {"key":"revenue","name":"Revenue","description":"","actual":"","estimate":"26400000000",
                     "comp_value":"","comp_desc":"","comp":"","is_released":false}
                ]
            },
            {
                "fiscal_year": 2026,
                "fiscal_period": "Q1",
                "period_text": "Q1 FY2026",
                "details": [
                    {"key":"eps","name":"EPS","description":"","actual":"0.27","estimate":"0.41",
                     "comp_value":"-0.14","comp_desc":"低于预期","comp":"down","is_released":true},
                    {"key":"revenue","name":"Revenue","description":"","actual":"19335000000","estimate":"21110000000",
                     "comp_value":"-1775000000","comp_desc":"低于预期","comp":"down","is_released":true}
                ]
            }
        ]
    }))
    .unwrap()
}

fn key_metrics() -> USKeyFinancialMetrics {
    serde_json::from_value(json!({
        "currency": "USD",
        "report": "quarterly",
        "empty_fields": ["ebit"],
        "list": [
            {"ff_period":"Q1","ff_year":2026,"fp_end":"2026-03-31","report_txt":"Q1 2026",
             "rpt_date":"2026-04-22","fields":[{"key":"revenue","value":"19335000000"}]},
            {"ff_period":"Q4","ff_year":2025,"fp_end":"2025-12-31","report_txt":"Q4 2025",
             "rpt_date":"2026-01-28","fields":[{"key":"revenue","value":"25707000000"}]}
        ]
    }))
    .unwrap()
}

fn calendar() -> CalendarEventsResponse {
    serde_json::from_value(json!({
        "date": "2025-08-18",
        "next_date": "2026-10-23",
        "list": [
            {
                "date": "2026-07-22",
                "count": 1,
                "infos": [
                    {"counter_id":"ST/US/TSLA","market":"US","content":"特斯拉 2026 Q2 业绩",
                     "counter_name":"特斯拉","date_type":"盘后","date":"2026.07.22","data_kv":[],
                     "type":"financial","datetime":"1784275200","star":3,"id":"evt-0"}
                ]
            },
            {
                "date": "2026-10-22",
                "count": 2,
                "infos": [
                    {"counter_id":"ST/US/TSLA","market":"US","content":"特斯拉 2026 Q3 业绩",
                     "counter_name":"特斯拉","date_type":"盘后","date":"2026.10.22","data_kv":[
                        {"key":"预期 EPS","value":"0.52","type":"estimate_eps","value_raw":"0.52"}
                     ],"type":"financial","datetime":"1792200000","star":3,"id":"evt-1",
                     "financial_market_time":"盘后",
                     "ext":{"financial_report":{"market_time":"after_market","fiscal_year":2026,
                        "period":3,"period_type":"Q"}}},
                    {"counter_id":"ST/US/AAPL","market":"US","content":"苹果 2026 Q4 业绩",
                     "counter_name":"苹果","date_type":"盘后","date":"2026.10.22","data_kv":[],
                     "type":"financial","datetime":"1792200000","star":3,"id":"evt-2"}
                ]
            }
        ]
    }))
    .unwrap()
}

#[test]
fn field_names_report_the_nested_paths_the_response_actually_carries() {
    let names = field_names(&json!({
        "currency": "USD",
        "list": [{"fiscal_year": 2026, "details": [{"key": "eps", "actual": "0.27"}]}]
    }));
    assert_eq!(
        names,
        vec![
            "currency",
            "list",
            "list[].details",
            "list[].details[].actual",
            "list[].details[].key",
            "list[].fiscal_year",
        ]
    );
}

#[test]
fn consensus_findings_show_released_periods_and_populated_estimates_and_actuals() {
    let findings = consensus_findings(&consensus()).join("\n");
    assert!(
        findings.contains("`details[].key`: eps, revenue"),
        "{findings}"
    );
    assert!(findings.contains("current_index"), "{findings}");
    assert!(
        findings.contains(
            "| Q1 FY2026 | 2026 | Q1 | yes | 0.41 | 0.27 | -0.14 | 21110000000 | 19335000000 |"
        ),
        "{findings}"
    );
    assert!(
        findings.contains("| Q2 FY2026 | 2026 | Q2 | no | 0.55 | — | — | 26400000000 | — |"),
        "{findings}"
    );
    assert!(findings.contains("1 of 2 periods released"), "{findings}");
}

#[test]
fn key_metrics_findings_list_the_report_dates_available_per_period() {
    let findings = key_metrics_findings(&key_metrics()).join("\n");
    assert!(findings.contains("`report` quarterly"), "{findings}");
    assert!(findings.contains("`empty_fields`: ebit"), "{findings}");
    assert!(
        findings.contains("| Q1 2026 | 2026 | Q1 | 2026-03-31 | 2026-04-22 |"),
        "{findings}"
    );
    assert!(
        findings.contains("`fields[]` keys: key, value"),
        "{findings}"
    );
}

#[test]
fn calendar_findings_name_the_next_report_date_its_session_marker_and_the_window() {
    let findings = calendar_findings(&calendar(), "TSLA.US", "2026-09-21").join("\n");
    assert!(
        findings.contains("next `TSLA.US` report: 2026-10-22, `date_type` 盘后"),
        "{findings}"
    );
    assert!(
        findings.contains("`financial_market_time` 盘后"),
        "{findings}"
    );
    assert!(
        findings.contains("`data_kv[].type`: estimate_eps"),
        "{findings}"
    );
    assert!(
        findings.contains("window 2025-08-18 onwards; `next_date` 2026-10-23"),
        "{findings}"
    );
    assert!(findings.contains("3 events in the window"), "{findings}");
    assert!(
        findings.contains("2 distinct symbols in the window, first 12: AAPL.US, TSLA.US"),
        "{findings}"
    );
}

#[test]
fn an_anchor_date_is_accepted_as_a_plain_calendar_date_and_nothing_else() {
    assert_eq!(
        parse_probe_date("2026-07-20").unwrap(),
        time::Date::from_calendar_date(2026, time::Month::July, 20).unwrap()
    );
    for invalid in ["2026-07", "2026-13-01", "2026-02-30", "20260720", "today"] {
        assert!(parse_probe_date(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn calendar_findings_keep_past_report_sessions_that_precede_the_anchor_date() {
    let findings = calendar_findings(&calendar(), "TSLA.US", "2026-09-21").join("\n");
    assert!(
        findings.contains("2 `TSLA.US` report events, 1 before 2026-09-21"),
        "{findings}"
    );
    assert!(findings.contains("| 2026-07-22 | 盘后 |"), "{findings}");
    assert!(
        findings.contains("| 2026-10-22 | 盘后 | 盘后 | after_market | 2026 Q3 | 2026.10.22 |"),
        "{findings}"
    );
}

#[test]
fn an_untyped_response_is_excerpted_as_fenced_json_and_marked_when_truncated() {
    let lines = excerpt(&json!({"IS": [{"rpt_date": "2026-04-22"}]}), 1000);
    assert_eq!(lines.first().unwrap(), "```json");
    assert_eq!(lines.last().unwrap(), "```");
    assert!(
        lines[1].contains("\"rpt_date\": \"2026-04-22\""),
        "{lines:?}"
    );
    assert!(!lines[1].contains("truncated"), "{lines:?}");

    let truncated = excerpt(&json!({"IS": [{"rpt_date": "2026-04-22"}]}), 12);
    assert!(truncated[1].ends_with("… truncated"), "{truncated:?}");
}

#[test]
fn calendar_findings_say_so_when_the_symbol_has_no_event_in_the_window() {
    let findings = calendar_findings(&calendar(), "NVDA.US", "2026-09-21").join("\n");
    assert!(findings.contains("no `NVDA.US` report event"), "{findings}");
}

#[test]
fn the_report_records_every_call_with_its_status_fields_and_exact_error() {
    let report = ProbeReport {
        symbol: "TSLA.US".into(),
        data_center: "us".into(),
        probed_at: "2026-09-21T09:00:00Z".into(),
        calls: vec![
            CallReport::succeeded(
                "consensus",
                "GET /v1/quote/financial-consensus-detail",
                "symbol=TSLA.US",
                &serde_json::to_value(consensus()).unwrap(),
                consensus_findings(&consensus()),
            ),
            CallReport::failed(
                "us_key_financial_metrics",
                "GET /v1/us/stock-info/fin-keyfactor",
                "symbol=TSLA.US, report=quarterly",
                "OpenAPI error: code=301600 message=no permission",
            ),
        ],
    };
    let text = markdown(&report);

    assert!(text.starts_with("# Earnings Tick probe"), "{text}");
    assert!(text.contains("TSLA.US"), "{text}");
    assert!(text.contains("| consensus | ok |"), "{text}");
    assert!(
        text.contains("| us_key_financial_metrics | failed |"),
        "{text}"
    );
    assert!(
        text.contains("OpenAPI error: code=301600 message=no permission"),
        "{text}"
    );
    assert!(text.contains("`list[].details[].estimate`"), "{text}");
    let decisions = text
        .split_once("## Column decisions for ticket 02")
        .expect("column decisions close the report")
        .1;
    assert!(decisions.contains("EPS estimate"), "{decisions}");
    assert!(decisions.contains("confirmed"), "{decisions}");
    assert!(
        decisions.contains(
            "blocked — us_key_financial_metrics: OpenAPI error: code=301600 message=no permission"
        ),
        "{decisions}"
    );
    assert!(
        decisions.contains(
            "- `us_key_financial_metrics` is unavailable on this authorization, so ticket 02 must take the report date from the `finance_calendar` event for the period"
        ),
        "{decisions}"
    );
    assert!(
        decisions.contains("- `consensus` orders periods newest first"),
        "{decisions}"
    );
    assert!(
        !decisions.contains("`trading_days` rejects a long range"),
        "a call that was not probed contributes no decision note:\n{decisions}"
    );
    assert!(text.trim_end().ends_with('|'), "{text}");
}

#[test]
fn a_call_that_returned_nothing_is_still_recorded_as_a_successful_empty_response() {
    let report = CallReport::succeeded(
        "trading_days",
        "GET trading days",
        "market=US",
        &json!({"trading_days": [], "half_trading_days": []}),
        vec!["- 0 trading days returned".into()],
    );
    match report.outcome {
        CallOutcome::Succeeded { fields, findings } => {
            assert_eq!(fields, vec!["half_trading_days", "trading_days"]);
            assert_eq!(findings, vec!["- 0 trading days returned"]);
        }
        CallOutcome::Failed { .. } => panic!("an empty response is not a failure"),
    }
}
