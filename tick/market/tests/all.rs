//! Every market integration test, compiled as one binary.

#[path = "all/configuration.rs"]
mod configuration;
#[path = "all/demo.rs"]
mod demo;
#[path = "all/earnings.rs"]
mod earnings;
#[path = "all/event_chart.rs"]
mod event_chart;
#[path = "all/kline.rs"]
mod kline;
#[path = "all/kline_tools.rs"]
mod kline_tools;
#[path = "all/live_quote.rs"]
mod live_quote;
#[path = "all/narrow_errors.rs"]
mod narrow_errors;
#[path = "all/news.rs"]
mod news;
#[path = "all/options_research.rs"]
mod options_research;
#[path = "all/positions.rs"]
mod positions;
#[path = "all/quote.rs"]
mod quote;
#[path = "all/reader.rs"]
mod reader;
#[path = "all/recovery.rs"]
mod recovery;
#[path = "all/themed_runtime.rs"]
mod themed_runtime;
#[path = "all/tick_stream.rs"]
mod tick_stream;
#[path = "all/watchlist.rs"]
mod watchlist;
