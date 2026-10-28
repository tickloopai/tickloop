//! A finished turn's debug details: the prompt cache summary the turn's line ends with, and the
//! full details its `[debug info]` button and `/debug` copy. A detail whose data is not available
//! is left out rather than shown as zero.

use std::collections::BTreeSet;

use tickloop_harness::protocol::{
    CacheBreakNote, ChatSnapshot, ChatStatus, ModelIdentity, ModelRequestKind, RequestSequence,
    RequestUsageRecord, RunId,
};

/// `1` shows the debug details and `0` or anything else hides them; `make dev` sets `1` unless
/// the caller set it already.
pub(super) const DEBUG_VARIABLE: &str = "TICKLOOP_DEBUG";
/// The control after a finished turn's line that copies the turn's debug details.
pub(super) const DEBUG_BUTTON: &str = "[debug info]";
/// The command that copies the latest finished turn's debug details from the keyboard.
pub(super) const DEBUG_COMMAND: &str = "/debug";
/// The likely cause the runtime gives the first request after a compaction, which rewrote the
/// prefix the cache held: an expected cold start, not a break.
const COMPACTION_CAUSE: &str = "compaction";
/// A turn reading less of its input from the cache than this share is shown as a warning.
const LOW_HIT_PERCENT: u64 = 50;

pub(super) fn enabled_by(value: Option<&str>) -> bool {
    value == Some("1")
}

/// How a turn used the prompt cache, as the copied details name it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheState {
    /// Every request that reported the cache was the first of its kind in the Chat, or the first
    /// after a compaction, so there was nothing to read yet.
    Cold,
    Normal,
    Low,
    /// The request prefix changed or the cache was lost.
    Break,
}

impl CacheState {
    fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Break => "break",
        }
    }
}

/// What a finished turn's line ends with: ` · cache 96% · avg 91%`.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct CacheSummary {
    /// `cache 96%`, `cache cold`, `cache n/a` or `cache 4% (break: tools)`.
    pub(super) cache: String,
    /// Whether the cache part shows a low hit rate or a break.
    pub(super) is_warning: bool,
    /// `avg 91%`: the whole Chat's hit rate, when the turn's own is a percentage.
    pub(super) average: Option<String>,
}

/// One finished Run of a Chat, with the model requests it made in the order it made them.
pub(super) struct TurnDebug<'a> {
    snapshot: &'a ChatSnapshot,
    run: RunId,
    requests: Vec<&'a RequestUsageRecord>,
}

impl<'a> TurnDebug<'a> {
    pub(super) fn of(snapshot: &'a ChatSnapshot, run: RunId) -> Self {
        Self {
            snapshot,
            run,
            requests: snapshot
                .request_usage
                .entries()
                .iter()
                .filter(|record| record.run_id == Some(run))
                .collect(),
        }
    }

    /// Whether the Run made no model request the ledger, which lives only as long as the Agent's
    /// process, still holds; such a turn has no debug details.
    pub(super) fn has_no_requests(&self) -> bool {
        self.requests.is_empty()
    }

    pub(super) fn cache_summary(&self) -> CacheSummary {
        let Some(cache) = self.cache() else {
            return CacheSummary {
                cache: "cache n/a".to_owned(),
                is_warning: false,
                average: None,
            };
        };
        let mut text = match cache.hit_percent {
            Some(percent) if cache.state != CacheState::Cold => format!("cache {percent}%"),
            _ => "cache cold".to_owned(),
        };
        if let Some(note) = cache.shown_break {
            text.push_str(&format!(" (break: {})", break_label(note)));
        }
        CacheSummary {
            cache: super::safe_line(&text),
            is_warning: matches!(cache.state, CacheState::Low | CacheState::Break),
            average: (cache.state != CacheState::Cold)
                .then(|| Tokens::of(self.snapshot.request_usage.entries().iter()).hit_percent())
                .flatten()
                .map(|percent| format!("avg {percent}%")),
        }
    }

    /// The details `[debug info]` copies, one `key: value` line each.
    pub(super) fn details(&self) -> String {
        let mut lines = vec![
            format!("chat: {}", self.snapshot.id.public()),
            format!("run: {}", self.run.public()),
        ];
        let chat_loop = &self.snapshot.chat_loop;
        if let Some(turn) = chat_loop
            .runs_started
            .filter(|count| *count > 0 && chat_loop.run_id == Some(self.run))
        {
            lines.push(format!("turn: {turn}"));
        }
        let models = self.models();
        if !models.is_empty() {
            lines.push(format!("model: {}", models.join(", ")));
        }
        lines.push(format!(
            "requests: {} (chat total {})",
            self.requests.len(),
            self.snapshot.request_usage.entries().len()
        ));
        if let Some(context) = super::context_usage(&ChatStatus::of(self.snapshot)) {
            lines.push(format!(
                "context: {}/{} ({}%)",
                context.used_tokens,
                context.window_tokens,
                floor_percent(context.used_tokens, context.window_tokens as u64)
            ));
        }
        let turn = Tokens::of(self.requests.iter().copied());
        if let Some(read) = turn.cache_read {
            let mut line = format!("cache this turn: read {read}");
            if let Some(input) = turn.input {
                line.push_str(&format!(" / input {input}"));
            }
            if let Some(percent) = turn.hit_percent() {
                line.push_str(&format!(" ({percent}%)"));
            }
            if let Some(write) = turn.cache_write {
                line.push_str(&format!(", write {write}"));
            }
            lines.push(line);
        }
        let chat = Tokens::of(self.snapshot.request_usage.entries().iter());
        if let Some(percent) = chat.hit_percent() {
            lines.push(format!(
                "cache chat: {percent}% over {} requests",
                chat.rated_requests
            ));
        }
        if let Some(cache) = self.cache() {
            lines.push(format!("cache state: {}", cache.state.name()));
        }
        if let Some(output) = turn.output {
            lines.push(format!("output: {output}"));
        }
        if let Some(compactions) = chat_loop.compactions.filter(|count| *count > 0) {
            lines.push(format!("compactions: {compactions}"));
        }
        lines.extend(
            self.requests
                .iter()
                .filter_map(|record| record.cache_break.as_ref())
                .map(|note| {
                    format!(
                        "cache break: {} ({}, gap {}s)",
                        note.first_changed.as_deref().unwrap_or("none"),
                        note.likely_cause,
                        note.gap_seconds
                    )
                }),
        );
        lines.push(format!(
            "tickloop: {}, AgentAccess {}.{}",
            tickloop_code_access::build_identity().unwrap_or(env!("CARGO_PKG_VERSION")),
            tickloop_code_access::PROTOCOL_MAJOR,
            tickloop_code_access::PROTOCOL_MINOR
        ));
        if let Some(folder) = &self.snapshot.folder {
            lines.push(format!("folder: {}", folder.display()));
        }
        lines
            .iter()
            .map(|line| super::safe_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How the turn used the prompt cache; `None` when no request of it reported the cache.
    fn cache(&self) -> Option<TurnCache<'a>> {
        let reporting = self
            .requests
            .iter()
            .filter(|record| record.usage.cache_read_input_tokens.is_some())
            .collect::<Vec<_>>();
        if reporting.is_empty() {
            return None;
        }
        let cold = cold_starts(self.snapshot);
        let hit_percent = Tokens::of(self.requests.iter().copied()).hit_percent();
        let shown_break = self
            .requests
            .iter()
            .filter_map(|record| record.cache_break.as_ref())
            .find(|note| note.likely_cause != COMPACTION_CAUSE);
        let state = if shown_break.is_some() {
            CacheState::Break
        } else if reporting
            .iter()
            .all(|record| cold.contains(&record.sequence))
        {
            CacheState::Cold
        } else if hit_percent.is_some_and(|percent| percent < LOW_HIT_PERCENT) {
            CacheState::Low
        } else {
            CacheState::Normal
        };
        Some(TurnCache {
            state,
            hit_percent,
            shown_break,
        })
    }

    fn models(&self) -> Vec<String> {
        let mut names = Vec::new();
        for record in &self.requests {
            let name = model_name(&record.model);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

struct TurnCache<'a> {
    state: CacheState,
    hit_percent: Option<u64>,
    /// The turn's first break that was not the expected cold start after a compaction.
    shown_break: Option<&'a CacheBreakNote>,
}

/// The requests that found nothing to read: the first of each kind in the Chat to report the
/// cache, and the first after each compaction, unless they read most of their input anyway, as
/// a Chat resumed soon after its previous process can.
fn cold_starts(snapshot: &ChatSnapshot) -> BTreeSet<RequestSequence> {
    let mut reported_kinds = BTreeSet::<ModelRequestKind>::new();
    let mut cold = BTreeSet::new();
    for record in snapshot.request_usage.entries() {
        let Some(read) = record.usage.cache_read_input_tokens else {
            continue;
        };
        let is_first = reported_kinds.insert(record.kind)
            || record
                .cache_break
                .as_ref()
                .is_some_and(|note| note.likely_cause == COMPACTION_CAUSE);
        let read_little = record
            .context_input_tokens()
            .is_none_or(|input| read.saturating_mul(2) < input);
        if is_first && read_little {
            cold.insert(record.sequence);
        }
    }
    cold
}

/// The prefix part a break names on the turn's line: `tools` for any tool, otherwise the part,
/// or the cause when the prefix did not change.
fn break_label(note: &CacheBreakNote) -> &str {
    match note.first_changed.as_deref() {
        Some(part) if part.starts_with("tool:") => "tools",
        Some(part) => part,
        None => &note.likely_cause,
    }
}

/// Token counts summed over requests, each `None` when none of the requests reported it. `input`
/// counts every input token whether the protocol reports cached tokens inside its input or beside
/// it.
struct Tokens {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    /// The requests that reported both their cache reads and their input, which alone say what
    /// share of their input was cached, with those reads and that input.
    rated_requests: usize,
    rated_read: u64,
    rated_input: u64,
}

impl Tokens {
    fn of<'r>(requests: impl Iterator<Item = &'r RequestUsageRecord> + Clone) -> Self {
        let total = |counter: fn(&RequestUsageRecord) -> Option<u64>| {
            requests
                .clone()
                .filter_map(counter)
                .reduce(u64::saturating_add)
        };
        let rated = requests
            .clone()
            .filter_map(|record| {
                Some((
                    record.usage.cache_read_input_tokens?,
                    record.context_input_tokens()?,
                ))
            })
            .collect::<Vec<_>>();
        Self {
            input: total(RequestUsageRecord::context_input_tokens),
            output: total(|record| record.usage.output_tokens),
            cache_read: total(|record| record.usage.cache_read_input_tokens),
            cache_write: total(|record| record.usage.cache_write_input_tokens),
            rated_requests: rated.len(),
            rated_read: rated
                .iter()
                .fold(0, |sum, (read, _)| sum.saturating_add(*read)),
            rated_input: rated
                .iter()
                .fold(0, |sum, (_, input)| sum.saturating_add(*input)),
        }
    }

    fn hit_percent(&self) -> Option<u64> {
        (self.rated_input > 0).then(|| floor_percent(self.rated_read, self.rated_input))
    }
}

/// Rounded down, so only a whole is 100%.
fn floor_percent(part: u64, whole: u64) -> u64 {
    (u128::from(part) * 100 / u128::from(whole.max(1))) as u64
}

fn model_name(model: &ModelIdentity) -> String {
    if model.profile.is_empty() || model.profile == model.requested_model {
        model.requested_model.clone()
    } else {
        format!("{}/{}", model.profile, model.requested_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness::protocol::{
        ModelProvider, Protocol, RequestOutcome, RequestOwner, Usage,
    };
    use uuid::Uuid;

    const RUN: u128 = 3;
    const EARLIER_RUN: u128 = 9;

    fn run() -> RunId {
        Uuid::from_u128(RUN).into()
    }

    fn snapshot() -> ChatSnapshot {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(1),
            "folder": "/work/project",
            "messages": [],
            "chat_loop": {"id": Uuid::from_u128(2), "state": "Idle"}
        }))
        .unwrap()
    }

    fn model(protocol: Protocol, profile: &str, name: &str) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: profile.into(),
            protocol,
            requested_model: name.into(),
        }
    }

    fn claude() -> ModelIdentity {
        model(Protocol::Messages, "anthropic", "claude-opus-5-5")
    }

    fn request(
        snapshot: &mut ChatSnapshot,
        run: u128,
        model: ModelIdentity,
        usage: Usage,
        cache_break: Option<CacheBreakNote>,
    ) {
        let ledger = &mut snapshot.request_usage;
        let sequence = ledger
            .start(
                RequestOwner {
                    chat_id: snapshot.id,
                    loop_id: Some(snapshot.chat_loop.id),
                    run_id: Some(Uuid::from_u128(run).into()),
                },
                ModelRequestKind::Conversation,
                model,
            )
            .unwrap();
        ledger.observe(sequence, usage).unwrap();
        if let Some(note) = cache_break {
            ledger.record_cache_break(sequence, note).unwrap();
        }
        ledger.finish(sequence, RequestOutcome::Accepted).unwrap();
    }

    fn usage(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_input_tokens: read,
            cache_write_input_tokens: write,
            reasoning_output_tokens: None,
        }
    }

    fn note(first_changed: Option<&str>, cause: &str) -> CacheBreakNote {
        CacheBreakNote {
            first_changed: first_changed.map(Into::into),
            likely_cause: cause.into(),
            gap_seconds: 12,
        }
    }

    fn summary(cache: &str, is_warning: bool, average: Option<&str>) -> CacheSummary {
        CacheSummary {
            cache: cache.into(),
            is_warning,
            average: average.map(Into::into),
        }
    }

    /// A Chat whose first turn wrote the cache, so later turns read it.
    fn warmed() -> ChatSnapshot {
        let mut snapshot = snapshot();
        request(
            &mut snapshot,
            EARLIER_RUN,
            claude(),
            usage(100, 5, Some(0), Some(1_900)),
            None,
        );
        snapshot
    }

    #[test]
    fn only_one_turns_the_debug_details_on() {
        assert!(enabled_by(Some("1")));
        for value in [
            None,
            Some("0"),
            Some(""),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
            assert!(!enabled_by(value), "{value:?}");
        }
    }

    #[test]
    fn a_warm_turn_shows_its_rate_and_the_chats_rounded_down() {
        let mut snapshot = warmed();
        // Messages input adds its cache counters: 3 + 1990 + 7 = 2000, of which 1990 were read.
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(3, 40, Some(1_990), Some(7)),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache 99%", false, Some("avg 49%"))
        );
    }

    #[test]
    fn only_a_whole_hit_is_a_hundred_percent() {
        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            model(Protocol::Responses, "openai", "gpt-5"),
            usage(2_000, 30, Some(2_000), None),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary().cache,
            "cache 100%"
        );
    }

    #[test]
    fn the_chats_first_cached_request_is_a_cold_start_without_an_average() {
        let mut snapshot = snapshot();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(100, 5, Some(0), Some(1_900)),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache cold", false, None)
        );
        // A later request of the same turn that reads the cache makes the turn's rate count.
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(10, 5, Some(1_990), Some(0)),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache 49%", true, Some("avg 49%"))
        );
    }

    #[test]
    fn the_first_request_after_a_compaction_is_a_cold_start_not_a_break() {
        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(800, 5, Some(0), Some(300)),
            Some(note(Some("system:intro"), COMPACTION_CAUSE)),
        );
        let turn = TurnDebug::of(&snapshot, run());
        assert_eq!(turn.cache_summary(), summary("cache cold", false, None));
        assert!(turn.details().contains("\ncache state: cold\n"));
    }

    #[test]
    fn a_resumed_chat_that_reads_its_cache_at_once_is_not_cold() {
        let mut snapshot = snapshot();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(10, 5, Some(1_900), Some(90)),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache 95%", false, Some("avg 95%"))
        );
    }

    #[test]
    fn a_break_follows_the_rate_and_warns_with_the_shortest_part_name() {
        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(1_900, 5, Some(80), Some(20)),
            Some(note(Some("tool:mcp__beta__fetch"), "prefix_changed")),
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache 4% (break: tools)", true, Some("avg 2%"))
        );

        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(10, 5, Some(1_900), Some(90)),
            Some(note(Some("system:# Environment"), "prefix_changed")),
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary(
                "cache 95% (break: system:# Environment)",
                true,
                Some("avg 47%")
            )
        );

        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(1_900, 5, Some(100), Some(0)),
            Some(note(None, "cache_expired_after_5m")),
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary().cache,
            "cache 5% (break: cache_expired_after_5m)"
        );
    }

    #[test]
    fn a_low_rate_warns() {
        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(1_000, 5, Some(900), Some(100)),
            None,
        );
        assert_eq!(
            TurnDebug::of(&snapshot, run()).cache_summary(),
            summary("cache 45%", true, Some("avg 22%"))
        );
    }

    #[test]
    fn unreported_cache_reads_show_as_not_available_without_an_average() {
        let mut snapshot = snapshot();
        request(
            &mut snapshot,
            RUN,
            model(Protocol::ChatCompletions, "fixture", "fixture"),
            usage(10, 20, None, None),
            None,
        );
        let turn = TurnDebug::of(&snapshot, run());
        assert_eq!(turn.cache_summary(), summary("cache n/a", false, None));
        assert!(!turn.details().contains("cache"), "{}", turn.details());
    }

    #[test]
    fn a_run_without_requests_has_no_debug_details() {
        let snapshot = warmed();
        assert!(TurnDebug::of(&snapshot, run()).has_no_requests());
        assert!(!TurnDebug::of(&snapshot, Uuid::from_u128(EARLIER_RUN).into()).has_no_requests());
    }

    #[test]
    fn the_details_name_everything_known_one_line_each() {
        let mut snapshot = warmed();
        request(
            &mut snapshot,
            RUN,
            claude(),
            usage(10, 40, Some(1_900), Some(90)),
            Some(note(Some("tool:mcp__beta__fetch"), "prefix_changed")),
        );
        snapshot.chat_loop.run_id = Some(run());
        snapshot.chat_loop.runs_started = Some(4);
        snapshot.chat_loop.compactions = Some(2);
        let build = tickloop_code_access::build_identity().unwrap_or(env!("CARGO_PKG_VERSION"));
        assert_eq!(
            TurnDebug::of(&snapshot, run()).details(),
            [
                format!("chat: {}", snapshot.id.public()),
                format!("run: {}", run().public()),
                "turn: 4".into(),
                "model: anthropic/claude-opus-5-5".into(),
                "requests: 1 (chat total 2)".into(),
                "cache this turn: read 1900 / input 2000 (95%), write 90".into(),
                "cache chat: 47% over 2 requests".into(),
                "cache state: break".into(),
                "output: 40".into(),
                "compactions: 2".into(),
                "cache break: tool:mcp__beta__fetch (prefix_changed, gap 12s)".into(),
                format!(
                    "tickloop: {build}, AgentAccess {}.{}",
                    tickloop_code_access::PROTOCOL_MAJOR,
                    tickloop_code_access::PROTOCOL_MINOR
                ),
                "folder: /work/project".into(),
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_turn_number_shows_only_for_the_loops_latest_known_run() {
        let mut snapshot = warmed();
        request(&mut snapshot, RUN, claude(), usage(1, 1, None, None), None);
        snapshot.chat_loop.runs_started = Some(2);
        snapshot.chat_loop.run_id = Some(Uuid::from_u128(EARLIER_RUN).into());
        assert!(!TurnDebug::of(&snapshot, run()).details().contains("turn:"));
        snapshot.chat_loop.run_id = Some(run());
        snapshot.chat_loop.runs_started = None;
        assert!(!TurnDebug::of(&snapshot, run()).details().contains("turn:"));
    }
}
