use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;
use tick_builtin::{DemoApi, DemoOptions};
use tick_market::{
    args::{
        EarningsArgs, EventChartArgs, KlineArgs, NewsArgs, OnceArgs, OptionsArgs, QuoteArgs,
        WatchlistArgs,
    },
    commands,
    config::Settings,
    event_chart::{EventSession, MINIMUM_PANEL_SIZE as EVENT_CHART_MINIMUM},
    metadata::{MinimumPanelSize, TickKind},
};
use tick_provider::api::{CandleAdjust, CandlePeriod};
use tick_sdk::stream::TickStream;

/// The narrowest terminal every Tick is laid out for in full: a laptop at 80 columns.
const LAPTOP: (u16, u16) = (80, 24);
/// The width the Ticks were laid out for before 80 columns became the target.
const WIDE: (u16, u16) = (120, 24);
const ROWS: u16 = 24;
const SCROLL_HINT: &str = "scroll";

struct Scenario {
    kind: TickKind,
    event_chart: bool,
    keys: Vec<KeyCode>,
    shows: &'static [&'static str],
}

fn scenario(kind: TickKind, keys: &[KeyCode], shows: &'static [&'static str]) -> Scenario {
    Scenario {
        kind,
        event_chart: false,
        keys: keys.to_vec(),
        shows,
    }
}

impl Scenario {
    fn minimum(&self) -> MinimumPanelSize {
        if self.event_chart {
            EVENT_CHART_MINIMUM
        } else {
            self.kind.minimum_panel_size()
        }
    }

    fn label(&self, columns: u16, rows: u16) -> String {
        let chart = if self.event_chart { " event chart" } else { "" };
        format!(
            "{}{chart} {:?} at {columns}x{rows}",
            self.kind.name(),
            self.keys
        )
    }
}

/// Every Tick and mode with the fields that must stay whole: the key columns and values, the
/// header lines and the footer keys.
fn every_tick() -> Vec<Scenario> {
    vec![
        scenario(
            TickKind::Quote,
            &[],
            &[
                "Symbol",
                "Name",
                "Change %",
                "700.HK",
                "Demo · 腾讯控股",
                "480.20",
                "-11.80",
                "-2.40%",
                "? Help  q Quit",
            ],
        ),
        scenario(
            TickKind::Quote,
            &[KeyCode::Char('?')],
            &["Keys", "PageUp / PageDown  Page", "g / G  First / last"],
        ),
        scenario(
            TickKind::Watchlist,
            &[],
            &[
                "Watchlist · Demo · US",
                "Name",
                "Change %",
                "TSLA.US",
                "Demo · Tesla",
                "248.50",
                "8.50",
                "+3.54%",
            ],
        ),
        scenario(
            TickKind::Kline,
            &[],
            &[
                "700.HK · day · none",
                "2026-",
                "O 4",
                "H 4",
                "L 4",
                "C 4",
                "V 2",
                "Change -",
                "1–8 Period · m MA off · Live + 30s sync",
                "▲",
            ],
        ),
        Scenario {
            event_chart: true,
            ..scenario(
                TickKind::Kline,
                &[],
                &[
                    "DEMO.US · 2026-08-07",
                    "Release: 2026-08-07T16:05:00-04:00",
                    "Baseline 2026-08-07 regular close",
                    "America/New_York",
                    "v View · r / [ ] Range · e Event · Enter Detail",
                    "Esc Back · ? Help · q Quit",
                ],
            )
        },
        scenario(
            TickKind::Positions,
            &[],
            &[
                "Positions · 模拟盘账户",
                "Qty",
                "P/L %",
                "TSLA.US",
                "120",
                "248.50",
                "2.98万",
                "1920",
                "+6.88%",
                "Total HKD",
            ],
        ),
        scenario(
            TickKind::News,
            &[],
            &[
                "News · Market",
                "09:30",
                "示例资讯",
                "Demo · Market recap",
                "Enter Read  ·  ? Help  ·  q Quit",
            ],
        ),
        scenario(
            TickKind::News,
            &[KeyCode::Enter],
            &["示例资讯正文", "Esc Back  ·  j/k Scroll  ·  q Quit"],
        ),
        scenario(
            TickKind::Earnings,
            &[],
            &[
                "TSLA.US · Earnings · Compare periods",
                "EPS Act",
                "EPS YoY",
                "Rev Act",
                "Revenue YoY",
                "Q2 2026",
                "273.00亿",
                "11.43%",
                "· Enter detail",
            ],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Enter],
            &[
                "TSLA.US · Earnings · FY2026-Q2",
                "Metric",
                "Estimate",
                "27300000000",
                "26400000000",
                "· estimates not archived",
            ],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Char('r')],
            &[
                "Market reactions",
                "EPS Surp",
                "Move",
                "2026-07-21",
                "+0.79%",
                "Counts",
            ],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Char('f')],
            &["Period originals", "Source URL", "body not verified"],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Char('u')],
            &[
                "Upcoming earnings · preparing",
                "Available value",
                "awaiting actuals",
            ],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Char('s')],
            &["Symbol:", "Enter apply"],
        ),
        scenario(
            TickKind::Earnings,
            &[KeyCode::Char('?')],
            &[
                "Enter  Period detail / selected metric original · c",
                "r  Market reactions · f  Period originals · R  Refresh",
            ],
        ),
        scenario(
            TickKind::Options,
            &[],
            &[
                "AAPL.US · Options",
                "Spot 100.00",
                "through",
                "Observed",
                "Call bid/ask",
                "Put bid/ask",
                "C IV%",
                "P IV%",
                "3.00/3.20",
                "[ ] expiry",
                "No last-price substitution",
            ],
        ),
        scenario(
            TickKind::Reader,
            &[],
            &[
                "Reader · Original documents",
                "Quarterly results",
                "Revenue was $12.8",
                "1/4",
            ],
        ),
    ]
}

fn keys(codes: Vec<KeyCode>) -> impl futures_util::Stream<Item = std::io::Result<Event>> + Unpin {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for code in codes.into_iter().chain([KeyCode::Char('q')]) {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            sender
                .send(Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))))
                .unwrap();
        }
    });
    tokio_stream::wrappers::UnboundedReceiverStream::new(receiver)
}

/// Each cell's symbol, row by row; the cell after a wide glyph holds a blank.
fn cells(terminal: &Terminal<TestBackend>) -> Vec<Vec<String>> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol().to_owned())
                .collect()
        })
        .collect()
}

/// The screen as text, each wide glyph once.
fn text(cells: &[Vec<String>]) -> String {
    cells
        .iter()
        .map(|row| {
            let mut line = String::new();
            let mut covered = 0;
            for symbol in row {
                if covered > 0 {
                    covered -= 1;
                    continue;
                }
                covered = tickloop_ui::format::width(symbol, Default::default()).saturating_sub(1);
                line.push_str(symbol);
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The screen with every digit masked: the demo moves prices at random, while layout,
/// selection and mode stay put.
fn layout_of(cells: &[Vec<String>]) -> String {
    text(cells)
        .chars()
        .map(|character| {
            if character.is_ascii_digit() {
                '#'
            } else {
                character
            }
        })
        .collect()
}

async fn render(scenario: &Scenario, columns: u16, rows: u16) -> Vec<Vec<String>> {
    render_keys(scenario, columns, rows, scenario.keys.clone()).await
}

async fn render_keys(
    scenario: &Scenario,
    columns: u16,
    rows: u16,
    codes: Vec<KeyCode>,
) -> Vec<Vec<String>> {
    let api = DemoApi::new().unwrap();
    let settings = Settings {
        is_demo: true,
        ..Settings::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
    let mut updates = TickStream::in_memory(scenario.kind, "demo");
    let input = keys(codes);
    match scenario.kind {
        TickKind::Quote => commands::quote::run_live(
            &QuoteArgs {
                symbols: ["700.HK", "AAPL.US", "TSLA.US", "NVDA.US", "9988.HK"]
                    .map(String::from)
                    .to_vec(),
                output: OnceArgs::default(),
            },
            &api,
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::Watchlist => commands::watchlist::run_live(
            &WatchlistArgs::default(),
            &api,
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::Kline => {
            let (symbol, event) = if scenario.event_chart {
                (
                    "DEMO.US",
                    EventChartArgs {
                        event_date: Some("2026-08-07".into()),
                        event_session: EventSession::AfterHours,
                        published_at: Some("2026-08-07T16:05:00-04:00".into()),
                        ..EventChartArgs::default()
                    },
                )
            } else {
                ("700.HK", EventChartArgs::default())
            };
            commands::kline::run_live(
                &KlineArgs {
                    symbol: symbol.into(),
                    period: CandlePeriod::Day,
                    count: None,
                    adjust: CandleAdjust::None,
                    event,
                    output: OnceArgs::default(),
                },
                &api,
                &settings,
                &mut terminal,
                input,
                &mut updates,
            )
            .await
            .unwrap()
        }
        TickKind::Positions => commands::positions::run_live(
            &OnceArgs::default(),
            &api,
            &api,
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::News => commands::news::run_live(
            &NewsArgs {
                symbol: None,
                limit: 20,
                output: OnceArgs::default(),
            },
            &api,
            &api,
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::Earnings => commands::earnings::run_live(
            &EarningsArgs {
                symbol: "TSLA.US".into(),
                count: None,
                output: OnceArgs::default(),
            },
            &api,
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::Options => commands::options::run_live(
            &OptionsArgs {
                check: false,
                symbol: "AAPL.US".into(),
                expiry: None,
                snapshot_dir: None,
                strikes: 21,
                output: OnceArgs::default(),
            },
            Arc::new(DemoOptions),
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
        TickKind::Reader => commands::reader::run_live(
            tick_market::reader::demo_library(),
            &settings,
            &mut terminal,
            input,
            &mut updates,
        )
        .await
        .unwrap(),
    }
    cells(&terminal)
}

async fn assert_whole(scenarios: Vec<Scenario>, size: impl Fn(&Scenario) -> (u16, u16)) {
    for scenario in scenarios {
        let (columns, rows) = size(&scenario);
        let frame = text(&render(&scenario, columns, rows).await);
        let label = scenario.label(columns, rows);
        assert!(
            !frame.contains(SCROLL_HINT),
            "{label} scrolls although the Panel holds its layout:\n{frame}"
        );
        for field in scenario.shows {
            assert!(frame.contains(field), "{label} hides {field:?}:\n{frame}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn every_tick_shows_its_key_fields_whole_at_its_own_minimum_width() {
    assert_whole(every_tick(), |scenario| (scenario.minimum().columns, ROWS)).await;
}

#[tokio::test(start_paused = true)]
async fn every_tick_shows_its_key_fields_whole_on_an_80_column_laptop() {
    assert_whole(every_tick(), |_| LAPTOP).await;
}

#[test]
fn every_minimum_width_fits_the_80_column_laptop() {
    for kind in [
        TickKind::Quote,
        TickKind::Watchlist,
        TickKind::Kline,
        TickKind::Positions,
        TickKind::News,
        TickKind::Earnings,
        TickKind::Options,
        TickKind::Reader,
    ] {
        assert!(kind.minimum_panel_size().columns <= LAPTOP.0, "{kind:?}");
    }
    assert!(EVENT_CHART_MINIMUM.columns <= LAPTOP.0);
}

#[tokio::test(start_paused = true)]
async fn the_laptop_width_keeps_the_options_open_interest_columns() {
    assert_whole(
        vec![scenario(TickKind::Options, &[], &["C OI", "P OI"])],
        |_| LAPTOP,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn wider_panels_keep_the_columns_their_layouts_had() {
    assert_whole(
        vec![
            scenario(TickKind::Quote, &[], &["Open", "High", "Low", "Turnover"]),
            scenario(TickKind::Positions, &[], &["Available"]),
            scenario(TickKind::Earnings, &[], &["Date", "· [ ] periods"]),
            scenario(TickKind::Earnings, &[KeyCode::Enter], &["QoQ", "Unit"]),
            scenario(
                TickKind::Earnings,
                &[KeyCode::Char('r')],
                &["EPS Est", "EPS Act", "Rev Surp", "+20d"],
            ),
            scenario(TickKind::Options, &[], &["C volume", "P volume"]),
        ],
        |_| WIDE,
    )
    .await;
}

fn row(cells: &[Vec<String>], row: usize) -> String {
    cells[row].concat()
}

/// Asserts the visible rows of `narrow` are the columns of `canvas` starting at `offset`.
fn assert_slice(narrow: &[Vec<String>], canvas: &[Vec<String>], offset: usize, label: &str) {
    let columns = narrow[0].len();
    let visible = &narrow[..narrow.len() - 1];
    let slice: Vec<Vec<String>> = canvas
        .iter()
        .map(|row| row[offset..offset + columns].to_vec())
        .collect();
    assert_eq!(
        layout_of(visible),
        layout_of(&slice),
        "{label}: the Panel is not the canvas from column {offset}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_panel_narrower_than_the_minimum_scrolls_over_the_minimum_layout() {
    let watchlist = scenario(TickKind::Watchlist, &[], &[]);
    let minimum = watchlist.minimum().columns;
    let canvas = render(&watchlist, minimum, ROWS - 1).await;
    for columns in [40, 30] {
        let largest = usize::from(minimum - columns);
        let start = render(&watchlist, columns, ROWS).await;
        assert_slice(&start, &canvas, 0, "before scrolling");
        let indicator = row(&start, usize::from(ROWS) - 1);
        assert!(indicator.starts_with('━'), "{indicator}");
        assert!(
            indicator.trim_end().ends_with("─ < > scroll"),
            "{indicator}"
        );

        let right = vec![KeyCode::Char('>'); 2];
        let scrolled = render_keys(&watchlist, columns, ROWS, right).await;
        assert_slice(&scrolled, &canvas, largest, "after two >");
        let indicator = row(&scrolled, usize::from(ROWS) - 1);
        assert!(indicator.starts_with('─'), "{indicator}");
        assert!(indicator.contains("━ < > scroll"), "{indicator}");

        let there_and_back = ['>', '>', '<', '<'].map(KeyCode::Char).to_vec();
        let back = render_keys(&watchlist, columns, ROWS, there_and_back).await;
        assert_slice(&back, &canvas, 0, "after scrolling back");
    }
}

#[tokio::test(start_paused = true)]
async fn the_viewport_reveals_the_kline_cursor_and_keeps_a_manual_scroll() {
    let kline = scenario(TickKind::Kline, &[], &[]);
    let columns = 30;
    let start = text(&render(&kline, columns, ROWS).await);
    assert!(start.contains('▲'), "{start}");
    let indicator = start.lines().last().unwrap();
    assert!(indicator.starts_with('─'), "{indicator}");

    let scrolled = text(&render_keys(&kline, columns, ROWS, vec![KeyCode::Char('<'); 2]).await);
    assert!(!scrolled.contains('▲'), "{scrolled}");
    assert!(scrolled.contains("700.HK · day"), "{scrolled}");
    assert!(
        scrolled.lines().last().unwrap().starts_with('━'),
        "{scrolled}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_panel_shorter_than_the_minimum_scrolls_vertically_and_follows_the_selection() {
    let positions = scenario(TickKind::Positions, &[], &[]);
    let minimum = positions.minimum();
    let rows = minimum.rows - 2;
    let start = text(&render(&positions, minimum.columns, rows).await);
    assert!(start.contains("↕ 1–5/8 { } scroll"), "{start}");
    assert!(start.contains(">TSLA.US"), "{start}");

    let down = vec![KeyCode::Char('j'); 4];
    let followed = text(&render_keys(&positions, minimum.columns, rows, down.clone()).await);
    assert!(followed.contains(">Total USD"), "{followed}");
    assert!(followed.contains("↕ 3–7/8"), "{followed}");

    let manual = [down, vec![KeyCode::Char('{')]].concat();
    let scrolled = text(&render_keys(&positions, minimum.columns, rows, manual).await);
    assert!(scrolled.contains("↕ 1–5/8"), "{scrolled}");
    assert!(!scrolled.contains("Total USD"), "{scrolled}");
}

#[tokio::test(start_paused = true)]
async fn scroll_keys_reach_the_tick_when_the_panel_fits_or_an_editor_owns_the_keys() {
    for kind in [
        TickKind::Quote,
        TickKind::Watchlist,
        TickKind::Kline,
        TickKind::Positions,
        TickKind::News,
        TickKind::Reader,
    ] {
        let fitting = scenario(kind, &[], &[]);
        let (columns, rows) = LAPTOP;
        let untouched = render_keys(&fitting, columns, rows, vec![KeyCode::F(12); 4]).await;
        let pressed = render_keys(
            &fitting,
            columns,
            rows,
            ['<', '>', '{', '}'].map(KeyCode::Char).to_vec(),
        )
        .await;
        // Demo prices move at random and resize their columns, so each row's leading words
        // (selection marker, symbol, heading) and the Kline cursor line show whether a key
        // changed the view.
        let settled = |cells: &[Vec<String>]| -> Vec<String> {
            layout_of(cells)
                .lines()
                .map(|line| {
                    if kind == TickKind::Kline {
                        if line.contains('▲') {
                            line.to_owned()
                        } else {
                            String::new()
                        }
                    } else {
                        line.split_whitespace()
                            .take(2)
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                })
                .collect()
        };
        assert_eq!(
            settled(&untouched),
            settled(&pressed),
            "{kind:?} binds a scroll key"
        );
    }

    let prompt = scenario(
        TickKind::Earnings,
        &[KeyCode::Char('s'), KeyCode::Char('>')],
        &[],
    );
    let frame = text(&render(&prompt, 40, ROWS).await);
    assert!(frame.contains("Symbol: >"), "{frame}");
}

#[tokio::test(start_paused = true)]
async fn the_help_overlay_lists_the_scroll_keys_while_the_panel_scrolls() {
    let help = scenario(TickKind::Quote, &[KeyCode::Char('?')], &[]);
    let scrolling = text(&render(&help, 40, ROWS).await);
    assert!(scrolling.contains("< / >  Scroll"), "{scrolling}");
    let fitting = text(&render(&help, LAPTOP.0, LAPTOP.1).await);
    assert!(!fitting.contains("Scroll"), "{fitting}");
}
