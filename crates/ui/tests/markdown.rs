use ratatui::style::Modifier;
use tickloop_ui::{
    format::width,
    markdown::render,
    palette::standalone_styles,
    settings::{AmbiguousWidth, ColorMode},
};

#[test]
fn markdown_renders_headings_lists_and_visible_links_without_splitting_graphemes() {
    let text = "# 中文标题\n\n- 第一条中文新闻 e\u{301}\n- [官方链接](https://example.com)";
    for ambiguous in [AmbiguousWidth::Narrow, AmbiguousWidth::Wide] {
        let lines = render(text, 12, ambiguous, &standalone_styles(ColorMode::RedUp));
        let plain: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(plain.iter().all(|line| width(line, ambiguous) <= 12));
        assert!(plain.iter().any(|line| line.contains("中文标题")));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("中文标题")
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        let all: String = plain.iter().map(|line| line.trim_start()).collect();
        assert!(all.contains("官方链接"));
        assert!(all.contains("https://example.com"));
        assert!(all.contains("e\u{301}"));
        assert!(!all.contains("[官方链接]"));
        assert!(!plain.iter().any(|line| line.starts_with('\u{301}')));
    }
}

#[test]
fn nested_markdown_roles_patch_inherited_modifiers_and_restore_surrounding_text() {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
        widgets::{Paragraph, Widget},
    };

    let mut styles = standalone_styles(ColorMode::RedUp);
    styles.canvas = Style::reset().bg(Color::Indexed(100));
    styles.text = Style::new()
        .fg(Color::Indexed(101))
        .add_modifier(Modifier::BOLD | Modifier::ITALIC);
    styles.panel = Style::new().bg(Color::Indexed(102));
    styles.strong = Style::new()
        .fg(Color::Indexed(103))
        .remove_modifier(Modifier::ITALIC)
        .add_modifier(Modifier::DIM);
    styles.emphasis = Style::new()
        .fg(Color::Indexed(104))
        .remove_modifier(Modifier::BOLD | Modifier::DIM)
        .add_modifier(Modifier::UNDERLINED);
    let lines = render(
        "**Strong *Nested* StrongAgain** Plain",
        80,
        AmbiguousWidth::Narrow,
        &styles,
    );
    let area = Rect::new(0, 0, 80, 1);
    let mut buffer = Buffer::empty(area);
    buffer.set_style(area, styles.panel_base());
    Paragraph::new(lines).render(area, &mut buffer);
    for (column, foreground, modifiers) in [
        (0, 103, Modifier::BOLD | Modifier::DIM),
        (7, 104, Modifier::UNDERLINED),
        (14, 103, Modifier::BOLD | Modifier::DIM),
        (26, 101, Modifier::BOLD | Modifier::ITALIC),
    ] {
        let cell = &buffer[(column, 0)];
        assert_eq!(cell.fg, Color::Indexed(foreground));
        assert_eq!(cell.bg, Color::Indexed(102));
        assert_eq!(cell.modifier, modifiers);
    }
    assert_eq!(buffer[(79, 0)].bg, Color::Indexed(102));
}

mod layout {
    use tickloop_ui::{
        format::width,
        markdown::{MarkdownLine, StreamingLayout, layout, plain_text},
        settings::AmbiguousWidth,
    };

    fn texts(lines: &[MarkdownLine]) -> Vec<String> {
        lines.iter().map(MarkdownLine::text).collect()
    }

    fn render(source: &str, columns: usize) -> Vec<String> {
        texts(&layout(source, columns, AmbiguousWidth::Narrow))
    }

    #[test]
    fn markers_are_replaced_by_semantic_marks() {
        let lines = layout(
            "## 标题\n\n**粗** *斜* `code` ***both***",
            80,
            AmbiguousWidth::Narrow,
        );
        assert_eq!(texts(&lines), ["标题", "", "粗 斜 code both"]);
        assert_eq!(lines[0].spans[0].marks.heading, Some(2));
        let body = &lines[2].spans;
        let marks_of = |text: &str| {
            body.iter()
                .find(|span| span.text == text)
                .unwrap_or_else(|| panic!("missing span {text}"))
                .marks
        };
        assert!(marks_of("粗").strong);
        assert!(marks_of("斜").emphasis);
        assert!(marks_of("code").inline_code);
        assert!(marks_of("both").strong && marks_of("both").emphasis);
    }

    #[test]
    fn blank_lines_follow_the_source_and_always_follow_headings() {
        assert_eq!(
            render("# A\nfirst\nsecond\n\nthird\n- item\n\n\n\nlast\n\n", 80),
            [
                "A", "", "first", "second", "", "third", "- item", "", "last"
            ]
        );
    }

    #[test]
    fn lists_indent_by_level_and_hang_continuation_rows_under_the_text() {
        assert_eq!(
            render(
                "1. one\n   1. nested\n      1. deep\n2. two words wrap here\n\n- a\n  - b",
                16
            ),
            [
                "1. one",
                "   a. nested",
                "      i. deep",
                "2. two words",
                "   wrap here",
                "",
                "- a",
                "  - b",
            ]
        );
    }

    #[test]
    fn quotes_prefix_only_non_empty_lines_with_a_dim_bar() {
        let lines = layout("> said\n>\n> again", 40, AmbiguousWidth::Narrow);
        assert_eq!(texts(&lines), ["▎ said", "", "▎ again"]);
        assert!(lines[0].spans[0].marks.quote_bar);
        assert!(lines[0].spans[1].marks.quote);
    }

    #[test]
    fn wrapping_breaks_words_hard_splits_long_words_and_keeps_code_indentation() {
        assert_eq!(render("alpha beta gamma", 11), ["alpha beta", "gamma"]);
        assert_eq!(render("abcdefghijkl xy", 5), ["abcde", "fghij", "kl xy"]);
        assert_eq!(render("中文句子没有空格", 6), ["中文句", "子没有", "空格"]);
        assert_eq!(
            render("入口：`/mode` 命令（`terminal.rs`）、补全，很长的说明", 18),
            ["入口：/mode 命令", "（terminal.rs）、", "补全，很长的说明"]
        );
        assert_eq!(
            render("```\n    indented code\n```", 40),
            ["    indented code"]
        );
        for columns in [1, 2, 3, 7, 20] {
            for line in render("中文 **mixed** words and `code` 与表格", columns) {
                assert!(width(&line, AmbiguousWidth::Narrow) <= columns, "{line:?}");
            }
        }
    }

    #[test]
    fn tables_use_box_borders_when_they_fit_and_key_values_when_they_do_not() {
        let source = "| 名字 | 说明 |\n|---|---:|\n| a | 很长的说明 |\n| bb | x |";
        assert_eq!(
            render(source, 40),
            [
                "┌──────┬────────────┐",
                "│ 名字 │    说明    │",
                "├──────┼────────────┤",
                "│ a    │ 很长的说明 │",
                "├──────┼────────────┤",
                "│ bb   │          x │",
                "└──────┴────────────┘",
            ]
        );
        let narrow = layout(source, 16, AmbiguousWidth::Narrow);
        assert_eq!(
            texts(&narrow),
            [
                "名字: a",
                "说明: 很长的说明",
                "───────────────",
                "名字: bb",
                "说明: x"
            ]
        );
        assert!(narrow[0].spans[0].marks.strong);
    }

    #[test]
    fn links_images_html_and_tildes_render_as_readable_text() {
        assert_eq!(
            render(
                "[文档](https://a.io) <https://b.io> [mail](mailto:x@y.z) ![图](https://c.io/i.png) <b>bold</b> ~100",
                200
            ),
            ["文档 (https://a.io) https://b.io x@y.z https://c.io/i.png bold ~100"]
        );
        assert_eq!(render("---", 20), ["---"]);
    }

    #[test]
    fn streaming_reuses_completed_blocks_and_matches_a_full_layout() {
        let sources = [
            "# Title\n\nFirst paragraph that wraps.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\n- one\n- two\n\nTail **bold** end",
            "> quote line\ncontinued lazily\n\n- item one\n- item two\n  continued\n\nafter list\n| h | i |\n|---|---|\n| 1 | 2 |\ntext after table\n\n1. a\n\n   para in item\n2. b\n\n中文段落，**加粗**与`代码`。",
        ];
        for source in sources {
            let mut streaming = StreamingLayout::default();
            let mut previous: Vec<String> = Vec::new();
            let mut reused_any = false;
            for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                let prefix = &source[..end];
                let unchanged = streaming.update(prefix, 14, AmbiguousWidth::Narrow);
                let current = texts(streaming.lines());
                assert_eq!(current[..unchanged], previous[..unchanged]);
                reused_any |= unchanged > 0;
                assert_eq!(
                    current,
                    texts(&layout(prefix, 14, AmbiguousWidth::Narrow)),
                    "prefix {prefix:?}"
                );
                previous = current;
            }
            assert!(reused_any);
        }
        let source = sources[0];
        let mut streaming = StreamingLayout::default();
        streaming.update(source, 14, AmbiguousWidth::Narrow);
        assert_eq!(streaming.update(source, 30, AmbiguousWidth::Narrow), 0);
        assert_eq!(
            texts(streaming.lines()),
            texts(&layout(source, 30, AmbiguousWidth::Narrow))
        );
    }

    #[test]
    fn source_offsets_increase_through_the_document() {
        let source = "para one\n\n- item\n\n> quote\n\n| h |\n|---|\n| v |";
        let lines = layout(source, 40, AmbiguousWidth::Narrow);
        assert!(
            lines
                .windows(2)
                .all(|pair| pair[0].source_offset <= pair[1].source_offset)
        );
        assert!(lines.last().unwrap().source_offset <= source.len());
    }

    #[test]
    fn control_sequences_never_reach_the_laid_out_text() {
        let lines = render(
            "a\u{1b}[31mred\u{7f} `co\u{1b}de`\n\n```\nx\u{1b}[0m\n```",
            40,
        );
        assert_eq!(lines, ["a[31mred code", "", "x[0m"]);
    }

    #[test]
    fn unfinished_inline_markup_stays_literal_while_streaming() {
        let mut streaming = StreamingLayout::default();
        streaming.update("**still typing", 40, AmbiguousWidth::Narrow);
        assert_eq!(texts(streaming.lines()), ["**still typing"]);
        streaming.update("**still typing**", 40, AmbiguousWidth::Narrow);
        let lines = streaming.lines();
        assert_eq!(texts(lines), ["still typing"]);
        assert!(lines[0].spans[0].marks.strong);
    }

    #[test]
    fn plain_text_is_unbounded_by_width() {
        assert_eq!(
            plain_text("**a** long line that never wraps\n\n- item"),
            "a long line that never wraps\n\n- item"
        );
    }
}
