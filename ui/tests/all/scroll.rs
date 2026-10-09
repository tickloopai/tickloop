use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect, Size},
    style::Style,
};
use tickloop_ui::{
    format::{Hint, fit_hints, pack_fields, write_text},
    palette::standalone_styles,
    scroll::{ScrollViewport, ViewportLayout},
    settings::{AmbiguousWidth, ColorMode},
};

const NARROW: AmbiguousWidth = AmbiguousWidth::Narrow;

fn row(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn viewport(area: Rect, minimum: Size) -> ScrollViewport {
    let mut viewport = ScrollViewport::default();
    viewport.set_layout(ViewportLayout::of(area, minimum));
    viewport
}

#[test]
fn a_panel_that_holds_the_minimum_layout_never_scrolls() {
    assert_eq!(
        ViewportLayout::of(Rect::new(0, 0, 80, 24), Size::new(80, 14)),
        None
    );
    let layout = ViewportLayout::of(Rect::new(0, 0, 40, 10), Size::new(60, 14)).unwrap();
    assert_eq!(layout.canvas, Size::new(60, 14));
    assert_eq!(layout.visible, Rect::new(0, 0, 40, 9));
    assert_eq!(layout.indicator, Rect::new(0, 9, 40, 1));
    assert!(layout.scrolls_horizontally() && layout.scrolls_vertically());

    let tall_enough = ViewportLayout::of(Rect::new(0, 0, 40, 30), Size::new(60, 14)).unwrap();
    assert_eq!(tall_enough.canvas, Size::new(60, 29));
    assert!(!tall_enough.scrolls_vertically());
}

#[test]
fn the_slice_blanks_wide_glyphs_cut_by_either_edge() {
    let mut canvas = Buffer::empty(Rect::new(0, 0, 10, 2));
    write_text(&mut canvas, 0, 0, "ab腾讯cd", 10, NARROW, Style::default());
    let mut viewport = viewport(Rect::new(0, 0, 4, 2), Size::new(10, 1));
    let styles = standalone_styles(ColorMode::RedUp);

    viewport.handle_key('>');
    assert_eq!(viewport.offset(), Position::new(2, 0));
    let mut target = Buffer::empty(Rect::new(0, 0, 4, 2));
    viewport.render(&canvas, &mut target, styles, NARROW);
    assert_eq!(row(&target, 0), "腾 讯 ");

    viewport.handle_key('>');
    assert_eq!(viewport.offset(), Position::new(4, 0));
    let mut target = Buffer::empty(Rect::new(0, 0, 4, 2));
    viewport.render(&canvas, &mut target, styles, NARROW);
    assert_eq!(row(&target, 0), "讯 cd");

    let mut cut = self::viewport(Rect::new(0, 0, 3, 2), Size::new(10, 1));
    let mut slices = Vec::new();
    for _ in 0..3 {
        cut.handle_key('>');
        let mut target = Buffer::empty(Rect::new(0, 0, 3, 2));
        cut.render(&canvas, &mut target, styles, NARROW);
        slices.push(row(&target, 0));
    }
    assert_eq!(cut.offset(), Position::new(3, 0));
    assert_eq!(slices, ["b腾 ", "腾  ", " 讯 "]);
}

#[test]
fn the_indicator_thumb_spans_the_visible_share_of_the_canvas() {
    let canvas = Buffer::empty(Rect::new(0, 0, 60, 5));
    let styles = standalone_styles(ColorMode::RedUp);
    let mut viewport = viewport(Rect::new(0, 0, 30, 6), Size::new(60, 5));
    let mut target = Buffer::empty(Rect::new(0, 0, 30, 6));
    viewport.render(&canvas, &mut target, styles, NARROW);
    assert_eq!(
        row(&target, 5),
        format!("{}{} < > scroll", "━".repeat(10), "─".repeat(9))
    );

    viewport.handle_key('>');
    viewport.handle_key('>');
    assert_eq!(viewport.offset(), Position::new(30, 0));
    let mut target = Buffer::empty(Rect::new(0, 0, 30, 6));
    viewport.render(&canvas, &mut target, styles, NARROW);
    assert_eq!(
        row(&target, 5),
        format!("{}{} < > scroll", "─".repeat(9), "━".repeat(10))
    );

    let mut vertical = self::viewport(Rect::new(0, 0, 60, 4), Size::new(60, 8));
    vertical.handle_key('}');
    let mut target = Buffer::empty(Rect::new(0, 0, 60, 4));
    vertical.render(
        &Buffer::empty(Rect::new(0, 0, 60, 8)),
        &mut target,
        styles,
        NARROW,
    );
    assert_eq!(row(&target, 3).trim_end(), " ↕ 2–4/8 { } scroll");
}

#[test]
fn keys_for_an_axis_that_does_not_scroll_stay_with_the_tick() {
    let mut fitting = viewport(Rect::new(0, 0, 80, 24), Size::new(60, 14));
    assert!(!fitting.handle_key('>'));
    let mut sideways = viewport(Rect::new(0, 0, 40, 24), Size::new(60, 14));
    assert!(!sideways.handle_key('}'));
    assert!(sideways.handle_key('>'));
    assert!(!sideways.handle_key('x'));
    assert!(sideways.help().is_some());
    assert!(fitting.help().is_none());
}

#[test]
fn the_viewport_follows_the_focus_only_when_it_moves() {
    let mut viewport = viewport(Rect::new(0, 0, 20, 6), Size::new(50, 20));
    viewport.follow(Some(30), Some(12));
    assert_eq!(viewport.offset(), Position::new(11, 8));
    viewport.handle_key('<');
    viewport.follow(Some(30), Some(12));
    assert_eq!(
        viewport.offset(),
        Position::new(1, 8),
        "manual scroll stays"
    );
    viewport.follow(Some(0), Some(12));
    assert_eq!(viewport.offset(), Position::new(0, 8));
}

#[test]
fn hints_shorten_from_the_end_then_drop_whole() {
    let hints = [
        Hint::new("1–8 Period", "1–8"),
        Hint::new("m MA off", "m MA"),
        Hint::new("Live + 30s sync", "Live"),
    ];
    let fit = |columns| fit_hints(&hints, " · ", columns, NARROW);
    assert_eq!(fit(80), "1–8 Period · m MA off · Live + 30s sync");
    assert_eq!(fit(30), "1–8 Period · m MA off · Live");
    assert_eq!(fit(20), "1–8 · m MA · Live");
    assert_eq!(fit(10), "1–8 · m MA");
    assert_eq!(fit(2), "");
}

#[test]
fn fields_pack_into_lines_without_splitting_one() {
    let fields = ["2026-09-10 09:30", "O 382.35", "H 382.35", "L 380.10"];
    assert_eq!(pack_fields(&fields, 1, 43, NARROW), vec![0..4]);
    assert_eq!(pack_fields(&fields, 1, 36, NARROW), vec![0..3, 3..4]);
    assert_eq!(
        pack_fields(&fields, 1, 10, NARROW),
        vec![0..1, 1..2, 2..3, 3..4]
    );
}

#[test]
fn only_canvas_cells_in_the_visible_slice_reach_the_panel() {
    let mut viewport = viewport(Rect::new(0, 0, 20, 6), Size::new(50, 20));
    viewport.follow(Some(30), Some(12));
    assert_eq!(
        viewport.on_panel(Position::new(30, 12)),
        Some(Position::new(19, 4))
    );
    assert_eq!(viewport.on_panel(Position::new(10, 12)), None);
    assert_eq!(viewport.on_panel(Position::new(30, 13)), None);
}
