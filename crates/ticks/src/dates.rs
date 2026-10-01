//! ISO calendar dates on the wire: every Tick that talks dates to Longbridge or to the Agent
//! reads and writes them here, so one spelling is understood everywhere.

pub fn iso_date(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

/// `YYYY-MM-DD`, also accepting the `.` and `/` separators Longbridge responses use.
pub fn parse_iso_date(text: &str) -> Option<time::Date> {
    let mut parts = text.split(['-', '.', '/']);
    let year: i32 = parts.next()?.trim().parse().ok()?;
    let month: u8 = parts.next()?.trim().parse().ok()?;
    let day: u8 = parts.next()?.trim().parse().ok()?;
    time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}
