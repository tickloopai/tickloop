//! Where a Chat compacts on its own, following Claude Code's `autoCompact.ts`: the context window,
//! optionally capped by the `autoCompactWindow` setting, less the summary's output reservation and
//! a buffer. The runtime compacts beyond this line, `/context` shows the space past it as the
//! Autocompact buffer, and the status line warns before it is reached.

use crate::ModelConfig;

/// Claude Code's `MAX_OUTPUT_TOKENS_FOR_SUMMARY`: the output a compaction summary may need.
const SUMMARY_OUTPUT_RESERVE_TOKENS: usize = 20_000;
/// Claude Code's `AUTOCOMPACT_BUFFER_TOKENS`.
const AUTO_COMPACT_BUFFER_TOKENS: usize = 13_000;
/// Claude Code's `WARNING_THRESHOLD_BUFFER_TOKENS`: how far before the line the context counts as
/// nearly full.
const AUTO_COMPACT_WARNING_BUFFER_TOKENS: u64 = 20_000;
/// A small window keeps at least this share for the conversation, however much the reservations
/// would take.
const MINIMUM_LINE_PERCENT: usize = 50;
/// A small line warns no earlier than this share of it.
const MINIMUM_WARNING_PERCENT: u64 = 80;

/// The context window a Chat's requests fit in: the model card's, or for a model without one the
/// window the user set.
pub fn context_window_tokens(model: &ModelConfig, set_window: Option<usize>) -> Option<usize> {
    model
        .card
        .as_ref()
        .and_then(|card| card.context_window_tokens)
        .or(set_window)
}

/// The context tokens beyond which a conversation request first compacts the Chat, for a known
/// window; `window_cap` is the `autoCompactWindow` setting.
pub fn auto_compact_line(
    window: usize,
    max_output_tokens: u32,
    window_cap: Option<usize>,
) -> usize {
    let window = window_cap.map_or(window, |cap| cap.min(window));
    let reserved = (max_output_tokens as usize).min(SUMMARY_OUTPUT_RESERVE_TOKENS)
        + AUTO_COMPACT_BUFFER_TOKENS;
    window
        .saturating_sub(reserved)
        .max(window.saturating_mul(MINIMUM_LINE_PERCENT) / 100)
}

/// [`auto_compact_line`] for a Chat's model and set window; `None` while the window is unknown,
/// when the Chat never compacts on its own.
pub fn auto_compact_tokens(
    model: &ModelConfig,
    set_window: Option<usize>,
    window_cap: Option<usize>,
) -> Option<usize> {
    context_window_tokens(model, set_window)
        .map(|window| auto_compact_line(window, model.max_output_tokens, window_cap))
}

/// The context tokens from which the context counts as nearly full, before the line at
/// `auto_compact_tokens`.
pub fn auto_compact_warning_tokens(auto_compact_tokens: u64) -> u64 {
    auto_compact_tokens
        .saturating_sub(AUTO_COMPACT_WARNING_BUFFER_TOKENS)
        .max(auto_compact_tokens.saturating_mul(MINIMUM_WARNING_PERCENT) / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_reserves_the_summary_output_and_the_buffer() {
        // A 128k model writing up to 16k reserves all of it; larger outputs reserve 20k.
        assert_eq!(auto_compact_line(128_000, 16_384, None), 98_616);
        assert_eq!(auto_compact_line(200_000, 64_000, None), 167_000);
        assert_eq!(auto_compact_line(1_000_000, 128_000, None), 967_000);
    }

    #[test]
    fn the_cap_lowers_the_window_but_never_raises_it() {
        assert_eq!(
            auto_compact_line(1_000_000, 128_000, Some(400_000)),
            367_000
        );
        assert_eq!(auto_compact_line(200_000, 64_000, Some(400_000)), 167_000);
    }

    #[test]
    fn a_small_window_keeps_half_of_itself_and_never_goes_negative() {
        assert_eq!(auto_compact_line(32_000, 8_192, None), 16_000);
        assert_eq!(auto_compact_line(20_000, 1_024, None), 10_000);
        assert_eq!(auto_compact_line(3_100, 1_024, None), 1_550);
        assert_eq!(auto_compact_line(0, 4_096, None), 0);
        assert_eq!(auto_compact_line(1_000_000, 4_096, Some(1)), 0);
    }

    #[test]
    fn the_warning_comes_twenty_thousand_tokens_early_or_at_four_fifths_of_a_small_line() {
        assert_eq!(auto_compact_warning_tokens(167_000), 147_000);
        assert_eq!(auto_compact_warning_tokens(967_000), 947_000);
        assert_eq!(auto_compact_warning_tokens(16_000), 12_800);
        assert_eq!(auto_compact_warning_tokens(0), 0);
    }

    #[test]
    fn the_window_is_the_cards_or_else_the_one_set() {
        let snapshot: crate::ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::from_u128(0xc1),
            "folder": "/work",
            "messages": [],
            "chat_loop": {"id": uuid::Uuid::from_u128(0x10), "state": "Idle", "run_id": null},
        }))
        .unwrap();
        let mut model = snapshot.model;
        model.card = None;
        assert_eq!(context_window_tokens(&model, None), None);
        assert_eq!(auto_compact_tokens(&model, None, None), None);
        assert_eq!(context_window_tokens(&model, Some(50_000)), Some(50_000));
        model.max_output_tokens = 64_000;
        assert_eq!(
            auto_compact_tokens(&model, Some(200_000), None),
            Some(167_000)
        );
    }
}
