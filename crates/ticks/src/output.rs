use crate::{
    cli::OutputFormat,
    config::Settings,
    error::{Error, Result},
};
use ratatui::backend::IntoCrossterm;
use serde_json::{Map, Value};
use std::io::Write;
use tickloop_ui::{
    format::{NumberLanguage, abbreviate, clean, pad, width},
    styles::PriceDirection,
};

/// How one column's value is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellFormat {
    Plain,
    Price,
    Percent,
    Abbreviated,
    MagnitudeBar,
}

/// One column's presentation: how its value is written, whether it carries an up or down
/// direction, and the header a narrow Panel falls back to. Every Tick's columns are named here,
/// so formatting and header compaction can never disagree about a column.
struct Column {
    header: &'static str,
    short: Option<&'static str>,
    format: CellFormat,
    signed: bool,
}

const COLUMNS: &[Column] = &[
    column("Latest Price", Some("Price"), CellFormat::Price, false),
    column("Change", None, CellFormat::Price, true),
    column(
        "Change Percent",
        Some("Change %"),
        CellFormat::Percent,
        true,
    ),
    column("Open", None, CellFormat::Price, false),
    column("High", None, CellFormat::Price, false),
    column("Low", None, CellFormat::Price, false),
    column("Previous Close", Some("Prev"), CellFormat::Price, false),
    column("Volume", None, CellFormat::Abbreviated, false),
    column("Turnover", None, CellFormat::Abbreviated, false),
    column(
        "Market Value",
        Some("Value"),
        CellFormat::Abbreviated,
        false,
    ),
    column("Profit Loss", Some("P/L"), CellFormat::Abbreviated, true),
    column(
        "Profit Loss Percent",
        Some("P/L %"),
        CellFormat::Percent,
        true,
    ),
    column("Cost Price", Some("Cost"), CellFormat::Price, false),
    column("Quantity", Some("Qty"), CellFormat::Plain, false),
    column(
        "Available Quantity",
        Some("Available"),
        CellFormat::Plain,
        false,
    ),
    column("Published At", Some("Time"), CellFormat::Plain, false),
    column("Updated At", Some("Updated"), CellFormat::Plain, false),
    column("Report Date", Some("Date"), CellFormat::Plain, false),
    column("EPS Estimate", Some("EPS Est"), CellFormat::Plain, false),
    column("EPS Actual", Some("EPS Act"), CellFormat::Plain, false),
    column("EPS Surprise", Some("EPS Surp"), CellFormat::Plain, true),
    column(
        "Revenue Estimate",
        Some("Rev Est"),
        CellFormat::Abbreviated,
        false,
    ),
    column(
        "Revenue Actual",
        Some("Rev Act"),
        CellFormat::Abbreviated,
        false,
    ),
    column(
        "Revenue Surprise",
        Some("Rev Surp"),
        CellFormat::Abbreviated,
        true,
    ),
    column("Before 20d", Some("-20d"), CellFormat::Percent, true),
    column("Before 5d", Some("-5d"), CellFormat::Percent, true),
    column("Day", None, CellFormat::Percent, true),
    column("After 5d", Some("+5d"), CellFormat::Percent, true),
    column("After 20d", Some("+20d"), CellFormat::Percent, true),
    column("Day Bar", Some("Move"), CellFormat::MagnitudeBar, true),
];

const fn column(
    header: &'static str,
    short: Option<&'static str>,
    format: CellFormat,
    signed: bool,
) -> Column {
    Column {
        header,
        short,
        format,
        signed,
    }
}

fn column_rules(header: &str) -> Option<&'static Column> {
    COLUMNS.iter().find(|column| column.header == header)
}

/// The header a Panel too narrow for the full name shows instead.
pub fn short_header(header: &str) -> &str {
    column_rules(header)
        .and_then(|column| column.short)
        .unwrap_or(header)
}

pub struct TableData {
    pub headers: Vec<&'static str>,
    pub rows: Vec<Vec<Value>>,
}

// Adapted header-to-JSON-key convention; see THIRD_PARTY_NOTICES.md.
pub fn header_key(header: &str) -> String {
    header.to_lowercase().replace(' ', "_")
}

impl TableData {
    pub fn json(&self) -> Value {
        Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    Value::Object(
                        self.headers
                            .iter()
                            .zip(row)
                            .map(|(header, value)| (header_key(header), value.clone()))
                            .collect::<Map<_, _>>(),
                    )
                })
                .collect(),
        )
    }

    pub fn display_rows(&self, settings: &Settings) -> Vec<Vec<DisplayCell>> {
        let language = match settings.language {
            crate::config::Language::ZhCn => NumberLanguage::Chinese,
            crate::config::Language::En => NumberLanguage::English,
        };
        self.rows
            .iter()
            .map(|row| {
                self.headers
                    .iter()
                    .zip(row)
                    .map(|(header, value)| {
                        let raw = display_value(value);
                        let number = raw.parse::<f64>().ok();
                        let rules = column_rules(header);
                        let text = match (rules.map(|column| column.format), number) {
                            (Some(CellFormat::Abbreviated), Some(number)) => {
                                abbreviate(number, language)
                            }
                            (Some(CellFormat::Percent), Some(number)) => format!("{number:+.2}%"),
                            (Some(CellFormat::MagnitudeBar), Some(number)) => {
                                tickloop_ui::charts::magnitude_bar(number)
                            }
                            (Some(CellFormat::Price), Some(_)) => raw
                                .parse::<rust_decimal::Decimal>()
                                .map(|number| {
                                    let number = number.normalize();
                                    if number.scale() <= 2 {
                                        format!("{number:.2}")
                                    } else {
                                        number.to_string()
                                    }
                                })
                                .unwrap_or(raw),
                            _ => raw,
                        };
                        let direction = if rules.is_some_and(|column| column.signed) {
                            PriceDirection::from_change(number.unwrap_or(0.0))
                        } else {
                            PriceDirection::Flat
                        };
                        DisplayCell {
                            text,
                            direction,
                            is_error: self
                                .headers
                                .iter()
                                .position(|header| *header == "Error")
                                .is_some_and(|index| {
                                    row.get(index).is_some_and(|value| !value.is_null())
                                }),
                        }
                    })
                    .collect()
            })
            .collect()
    }

    pub fn write(
        &self,
        format: OutputFormat,
        settings: &Settings,
        color: bool,
        output: &mut impl Write,
    ) -> Result<()> {
        if format == OutputFormat::Json {
            serde_json::to_writer(&mut *output, &self.json())
                .map_err(|error| Error::usage(error.to_string()))?;
            writeln!(output)?;
            return Ok(());
        }
        let cells = self.display_rows(settings);
        let rows: Vec<Vec<String>> = cells
            .iter()
            .map(|row| row.iter().map(|cell| cell.text.clone()).collect())
            .collect();
        let widths: Vec<usize> = self
            .headers
            .iter()
            .enumerate()
            .map(|(column, header)| {
                rows.iter()
                    .map(|row| width(&row[column], settings.ambiguous_width))
                    .max()
                    .unwrap_or(0)
                    .max(width(header, settings.ambiguous_width))
            })
            .collect();
        for (row_index, row) in std::iter::once(
            self.headers
                .iter()
                .map(|header| header.to_string())
                .collect::<Vec<_>>(),
        )
        .chain(rows)
        .enumerate()
        {
            let line: Vec<String> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(column, (cell, columns))| {
                    let padded = pad(cell, *columns, settings.ambiguous_width, column >= 2);
                    if color && row_index > 0 {
                        let styles = settings.styles();
                        let style: crossterm::style::ContentStyle = styles
                            .text
                            .patch(styles.price(cells[row_index - 1][column].direction))
                            .patch(if cells[row_index - 1][column].is_error {
                                styles.error
                            } else {
                                ratatui::style::Style::new()
                            })
                            .into_crossterm();
                        return style.apply(padded).to_string();
                    }
                    padded
                })
                .collect();
            writeln!(output, "{}", line.join("  ").trim_end())?;
        }
        Ok(())
    }
}

pub fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "--".into(),
        Value::String(text) => clean(text),
        value => value.to_string(),
    }
}

#[derive(Clone)]
pub struct DisplayCell {
    pub text: String,
    pub direction: PriceDirection,
    pub is_error: bool,
}
