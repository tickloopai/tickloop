//! Shortens Tick Update data to the bytes a delivery allows it by whole segments, so a delivery
//! never goes out empty because its data is large.
//!
//! Only data over its allowance is trimmed. The largest array at any depth, or string with line
//! breaks, is shortened first, then the next largest; ordinary fields stay. A series keeps its
//! oldest element, its newest elements in one run and older ones at doubling intervals; an array
//! of undeclared order keeps a head and a tail; a text keeps its first line and as many last lines
//! as fit. Each run left out is replaced in place by a marker counting it. Only when the
//! candidates are at their shortest and the data is still over does the largest remaining value
//! give way to a marker with its size.
//!
//! Every size is measured once: a candidate's subtree is disjoint from every other candidate not
//! inside it, so cutting one changes no other's size, and the total follows by arithmetic. The
//! work is linear in the data apart from sorting the candidates.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use tick_protocol::{SeriesEnd, StateSeries};

/// Of the bytes left after a series' oldest element, the share its newest run takes first.
const NEWEST_RUN_SHARE: (usize, usize) = (2, 3);
/// The first interval between sampled older elements of a series; each next one doubles.
const FIRST_SAMPLE_STEP: usize = 2;
const OMITTED_VALUE_PREFIX: &str = "… omitted (";
/// Candidates are looked for this deep into the data; deeper values only count their bytes.
const MAX_CANDIDATE_DEPTH: usize = 32;
/// A value whose JSON Pointer is longer is neither shortened nor omitted on its own.
const MAX_PATH_BYTES: usize = 128;
/// The most candidates shortened before the remaining values give way.
const MAX_SHORTENED: usize = 4;
/// The most values replaced by their size before the whole data is.
const MAX_OMITTED: usize = 4;
/// The most records one trim reports: every shortened candidate and omitted value, and the data
/// itself when it gives way whole.
const MAX_TRIMMED_RECORDS: usize = MAX_SHORTENED + MAX_OMITTED + 1;

/// How a value was shortened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrimRule {
    /// A series whose newest element is its last.
    NewestLast,
    /// A series whose newest element is its first.
    NewestFirst,
    /// An array of undeclared order.
    HeadAndTail,
    /// A text, by lines.
    Lines,
    /// A value replaced by its size.
    Omitted,
}

/// One value a delivery shortened. `total` and `kept` count elements of an array and lines of a
/// text; an omitted value has `total` bytes and keeps none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Trimmed {
    /// JSON Pointer to the value within the delivered `data`.
    pub(crate) path: String,
    pub(crate) total: usize,
    pub(crate) kept: usize,
    pub(crate) rule: TrimRule,
}

impl Trimmed {
    /// The marker that replaces data of `total` encoded bytes whole, and its record.
    pub(crate) fn whole_omission(total: usize) -> (Value, Self) {
        let record = Self {
            path: String::new(),
            total,
            kept: 0,
            rule: TrimRule::Omitted,
        };
        (json!(omitted_value(total)), record)
    }

    /// `data` replaced whole by a marker saying its size.
    fn omit_whole(data: &mut Value) -> Self {
        let (marker, record) = Self::whole_omission(encoded_len(data));
        *data = marker;
        record
    }
}

/// Whether `trimmed` replaced any value by its size instead of shortening it.
pub(crate) fn omits_values(trimmed: &[Trimmed]) -> bool {
    trimmed
        .iter()
        .any(|record| record.rule == TrimRule::Omitted)
}

pub(crate) fn encoded_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |encoded| encoded.len())
}

/// Shortens `data` to at most `allowance` encoded bytes, keeping the newest end of each array
/// `series` declares, and tells what it shortened in at most [`MAX_TRIMMED_RECORDS`] records;
/// data already within it stays as it is.
pub(crate) fn trim(data: &mut Value, allowance: usize, series: &[StateSeries]) -> Vec<Trimmed> {
    let mut candidates = Vec::new();
    let mut size = measure(data, &mut String::new(), 0, &mut candidates);
    let mut trimmed = Vec::new();
    if size <= allowance {
        return trimmed;
    }
    // A candidate is larger than any candidate inside it, so it comes first.
    candidates.sort_unstable_by_key(|(_, size)| std::cmp::Reverse(*size));
    let mut shortened: Vec<String> = Vec::new();
    for (path, candidate_size) in candidates {
        if size <= allowance || shortened.len() == MAX_SHORTENED {
            break;
        }
        if shortened.iter().any(|done| is_within(&path, done)) {
            continue;
        }
        let target = candidate_size.saturating_sub(size - allowance);
        let newest = series
            .iter()
            .find(|declared| declared.path == path)
            .map(|declared| declared.newest);
        let Some(value) = data.pointer_mut(&path) else {
            continue;
        };
        if let Some((record, shortened_size)) = shorten(value, candidate_size, target, newest) {
            size = size - candidate_size + shortened_size;
            trimmed.push(Trimmed {
                path: path.clone(),
                ..record
            });
        }
        shortened.push(path);
    }
    if size > allowance {
        omit(data, size, allowance, &mut trimmed);
    }
    debug_assert!(trimmed.len() <= MAX_TRIMMED_RECORDS);
    trimmed
}

/// The encoded size of `value` at `path`, collecting each array and multi-line string within
/// [`MAX_CANDIDATE_DEPTH`] and [`MAX_PATH_BYTES`] with its own size.
fn measure(
    value: &Value,
    path: &mut String,
    depth: usize,
    candidates: &mut Vec<(String, usize)>,
) -> usize {
    if depth > MAX_CANDIDATE_DEPTH || path.len() > MAX_PATH_BYTES {
        return encoded_len(value);
    }
    let size = match value {
        Value::Array(elements) => {
            let mut size = 2 + elements.len().saturating_sub(1);
            for (index, element) in elements.iter().enumerate() {
                let length = path.len();
                path.push_str(&format!("/{index}"));
                size += measure(element, path, depth + 1, candidates);
                path.truncate(length);
            }
            size
        }
        Value::Object(fields) => {
            let mut size = 2 + fields.len().saturating_sub(1);
            for (key, field) in fields {
                let length = path.len();
                path.push('/');
                path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                // The key, its colon and its value.
                size += encoded_len(&json!(key)) + 1 + measure(field, path, depth + 1, candidates);
                path.truncate(length);
            }
            size
        }
        scalar => encoded_len(scalar),
    };
    let is_candidate = match value {
        Value::Array(elements) => elements.len() > 1,
        Value::String(text) => text.contains('\n'),
        _ => false,
    };
    if is_candidate {
        candidates.push((path.clone(), size));
    }
    size
}

/// Whether `path` is `ancestor` or a value inside it.
fn is_within(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Brings `data` of `size` encoded bytes within `allowance` by replacing values with markers
/// saying their size: the smallest value whose replacement alone is enough, or else the largest
/// top-level values while at most [`MAX_OMITTED`] of them are enough, or else the data itself.
fn omit(data: &mut Value, mut size: usize, allowance: usize, trimmed: &mut Vec<Trimmed>) {
    let excess = size - allowance;
    let saving = |bytes: usize| bytes.saturating_sub(encoded_len(&json!(omitted_value(bytes))));
    let mut path = String::new();
    while let Some((child, bytes)) = data.pointer(&path).and_then(|node| {
        children(node, &path)
            .into_iter()
            .max_by_key(|(_, bytes)| *bytes)
    }) {
        if saving(bytes) < excess || child.len() > MAX_PATH_BYTES {
            break;
        }
        path = child;
    }
    if !path.is_empty() {
        trimmed.push(omit_at(data, &path));
        return;
    }
    let mut largest = children(data, "");
    largest.sort_unstable_by_key(|(_, size)| std::cmp::Reverse(*size));
    let mut needed = 0;
    let mut saved = 0;
    for (child, bytes) in largest.iter().take(MAX_OMITTED) {
        if saved >= excess || saving(*bytes) == 0 || child.len() > MAX_PATH_BYTES {
            break;
        }
        saved += saving(*bytes);
        needed += 1;
    }
    if saved < excess {
        trimmed.push(Trimmed::omit_whole(data));
        return;
    }
    for (child, bytes) in largest.into_iter().take(needed) {
        trimmed.push(omit_at(data, &child));
        size -= saving(bytes);
    }
    debug_assert!(size <= allowance);
}

fn omit_at(data: &mut Value, path: &str) -> Trimmed {
    let value = data
        .pointer_mut(path)
        .expect("an omitted value was found at its path");
    Trimmed {
        path: path.to_owned(),
        ..Trimmed::omit_whole(value)
    }
}

/// Each field or element of `value` with its path and encoded size.
fn children(value: &Value, path: &str) -> Vec<(String, usize)> {
    match value {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, field)| {
                let key = key.replace('~', "~0").replace('/', "~1");
                (format!("{path}/{key}"), encoded_len(field))
            })
            .collect(),
        Value::Array(elements) => elements
            .iter()
            .enumerate()
            .map(|(index, element)| (format!("{path}/{index}"), encoded_len(element)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Shortens one candidate of `size` encoded bytes toward `target`, with its new size; `None`
/// when no shorter form exists, and then it stays as it is.
fn shorten(
    value: &mut Value,
    size: usize,
    target: usize,
    newest: Option<SeriesEnd>,
) -> Option<(Trimmed, usize)> {
    match value {
        Value::Array(elements) => {
            let rule = match newest {
                Some(SeriesEnd::Last) => TrimRule::NewestLast,
                Some(SeriesEnd::First) => TrimRule::NewestFirst,
                None => TrimRule::HeadAndTail,
            };
            let selection = select_elements(elements, target, rule);
            let (kept, shortened_size) = (selection.kept.len(), selection.bytes());
            if shortened_size >= size {
                return None;
            }
            let total = elements.len();
            *elements = selection.into_values(std::mem::take(elements));
            Some((
                Trimmed {
                    path: String::new(),
                    total,
                    kept,
                    rule,
                },
                shortened_size,
            ))
        }
        Value::String(text) => {
            let (shortened, total, kept) = shorten_lines(text, target)?;
            let shortened_size = encoded_len(&json!(shortened));
            if shortened_size >= size {
                return None;
            }
            *text = shortened;
            Some((
                Trimmed {
                    path: String::new(),
                    total,
                    kept,
                    rule: TrimRule::Lines,
                },
                shortened_size,
            ))
        }
        _ => None,
    }
}

fn omitted_elements(count: usize) -> String {
    format!("… {count} omitted …")
}

fn omitted_lines(count: usize) -> String {
    format!("… {count} lines omitted …")
}

/// The marker for a value of `bytes` left out.
pub(crate) fn omitted_value(bytes: usize) -> String {
    format!("{OMITTED_VALUE_PREFIX}{bytes} bytes) …")
}

fn marker_len(count: usize) -> usize {
    encoded_len(&json!(omitted_elements(count)))
}

/// The elements of an array a shortened copy keeps, and that copy's encoded size: each run of
/// elements it leaves out becomes one marker.
struct Selection {
    sizes: Vec<usize>,
    kept: BTreeSet<usize>,
    /// Encoded bytes of every element and marker in the copy.
    content: usize,
    /// Elements and markers in the copy.
    items: usize,
}

impl Selection {
    fn new(elements: &[Value]) -> Self {
        let count = elements.len();
        Self {
            sizes: elements.iter().map(encoded_len).collect(),
            kept: BTreeSet::new(),
            content: marker_len(count),
            items: 1,
        }
    }

    fn bytes(&self) -> usize {
        2 + self.content + self.items.saturating_sub(1)
    }

    /// The copy's content and item count once it keeps `index` too.
    fn keeping(&self, index: usize) -> (usize, usize) {
        let first = self
            .kept
            .range(..index)
            .next_back()
            .map_or(0, |kept| kept + 1);
        let last = self
            .kept
            .range(index + 1..)
            .next()
            .map_or(self.sizes.len() - 1, |kept| kept - 1);
        let mut content = self.content - marker_len(last - first + 1) + self.sizes[index];
        let mut items = self.items;
        for run in [index - first, last - index] {
            if run > 0 {
                content += marker_len(run);
                items += 1;
            }
        }
        (content, items)
    }

    fn bytes_keeping(&self, index: usize) -> usize {
        let (content, items) = self.keeping(index);
        2 + content + items.saturating_sub(1)
    }

    fn keep(&mut self, index: usize) {
        if self.kept.contains(&index) {
            return;
        }
        (self.content, self.items) = self.keeping(index);
        self.kept.insert(index);
    }

    fn into_values(self, elements: Vec<Value>) -> Vec<Value> {
        let mut values = Vec::with_capacity(self.items);
        let mut left_out = 0;
        for (index, element) in elements.into_iter().enumerate() {
            if self.kept.contains(&index) {
                if left_out > 0 {
                    values.push(json!(omitted_elements(left_out)));
                    left_out = 0;
                }
                values.push(element);
            } else {
                left_out += 1;
            }
        }
        if left_out > 0 {
            values.push(json!(omitted_elements(left_out)));
        }
        values
    }
}

fn select_elements(elements: &[Value], target: usize, rule: TrimRule) -> Selection {
    let count = elements.len();
    let mut selection = Selection::new(elements);
    match rule {
        TrimRule::NewestLast => select_series(&mut selection, target, |position| position),
        TrimRule::NewestFirst => {
            select_series(&mut selection, target, |position| count - 1 - position)
        }
        _ => select_head_and_tail(&mut selection, target),
    }
    selection
}

/// Keeps a series' oldest element, then its newest ones in one run until they take their share
/// of the bytes left, then older ones at doubling intervals before that run while they fit, and
/// finally lengthens the newest run with what is left. `at` maps a position counted from the
/// oldest end to the element's index.
fn select_series(selection: &mut Selection, target: usize, at: impl Fn(usize) -> usize) {
    let count = selection.sizes.len();
    selection.keep(at(0));
    if count == 1 {
        return;
    }
    let base = selection.bytes();
    let (numerator, denominator) = NEWEST_RUN_SHARE;
    let newest_share = target.saturating_sub(base) * numerator / denominator;
    selection.keep(at(count - 1));
    let mut newest_run_start = count - 1;
    while newest_run_start > 1
        && selection
            .bytes_keeping(at(newest_run_start - 1))
            .saturating_sub(base)
            <= newest_share
    {
        newest_run_start -= 1;
        selection.keep(at(newest_run_start));
    }
    let mut position = newest_run_start;
    let mut step = FIRST_SAMPLE_STEP;
    while position > step {
        position -= step;
        if selection.bytes_keeping(at(position)) > target {
            break;
        }
        selection.keep(at(position));
        step *= 2;
    }
    for position in (1..newest_run_start).rev() {
        if selection.kept.contains(&at(position)) {
            continue;
        }
        if selection.bytes_keeping(at(position)) > target {
            break;
        }
        selection.keep(at(position));
    }
}

/// Keeps elements from both ends in turn while they fit, so head and tail take about half each.
fn select_head_and_tail(selection: &mut Selection, target: usize) {
    let (mut head, mut tail) = (0, selection.sizes.len());
    let (mut head_fits, mut tail_fits) = (true, true);
    while head < tail && (head_fits || tail_fits) {
        if head_fits {
            head_fits = selection.bytes_keeping(head) <= target;
            if head_fits {
                selection.keep(head);
                head += 1;
            }
        }
        if tail_fits && head < tail {
            tail_fits = selection.bytes_keeping(tail - 1) <= target;
            if tail_fits {
                tail -= 1;
                selection.keep(tail);
            }
        }
    }
}

/// A text without its trailing blank lines, keeping its first line and as many of its last lines
/// as fit `target` encoded bytes with a marker for those between; with the count of its lines
/// and of those kept. `None` when that changes nothing.
fn shorten_lines(text: &str, target: usize) -> Option<(String, usize, usize)> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while lines.len() > 1 && lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let total = lines.len();
    let escaped: Vec<usize> = lines
        .iter()
        .map(|line| encoded_len(&json!(line)) - 2)
        .collect();
    // Lines joined by breaks encode as a string whose every break takes two bytes.
    let joined = |parts: usize, escaped_bytes: usize| 2 + escaped_bytes + 2 * (parts - 1);
    // The encoded text keeping the first line and those from `tail_start` on.
    let encoded = |tail_start: usize, tail_bytes: usize| match tail_start {
        1 => joined(total, escaped[0] + tail_bytes),
        _ => {
            let marker = encoded_len(&json!(omitted_lines(tail_start - 1))) - 2;
            joined(2 + total - tail_start, escaped[0] + marker + tail_bytes)
        }
    };
    let mut tail_start = total.saturating_sub(1).max(1);
    let mut tail_bytes: usize = escaped[tail_start..].iter().sum();
    while tail_start > 1 && encoded(tail_start - 1, tail_bytes + escaped[tail_start - 1]) <= target
    {
        tail_start -= 1;
        tail_bytes += escaped[tail_start];
    }
    let shortened = match tail_start {
        1 => lines.join("\n"),
        _ => format!(
            "{}\n{}\n{}",
            lines[0],
            omitted_lines(tail_start - 1),
            lines[tail_start..].join("\n")
        ),
    };
    (shortened != text).then(|| (shortened, total, 1 + total - tail_start))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tick_protocol::SeriesEnd;

    use super::*;

    /// The serialized size of every element in the series examples. At 154 bytes the 4 KiB
    /// example keeps exactly the elements the delivery rule lists.
    const ELEMENT_BYTES: usize = 154;

    fn element(index: usize) -> Value {
        let bare = json!({"i": index, "pad": ""});
        let pad = ELEMENT_BYTES - encoded_len(&bare);
        let element = json!({"i": index, "pad": "x".repeat(pad)});
        assert_eq!(encoded_len(&element), ELEMENT_BYTES);
        element
    }

    fn series(path: &str, newest: SeriesEnd) -> Vec<StateSeries> {
        vec![StateSeries {
            path: path.into(),
            newest,
        }]
    }

    /// The 1-based positions of the elements a trimmed array kept, and the counts its markers
    /// name, in order.
    fn kept_and_markers(trimmed: &Value) -> (Vec<usize>, Vec<usize>) {
        let mut kept = Vec::new();
        let mut markers = Vec::new();
        for value in trimmed.as_array().unwrap() {
            match value {
                Value::String(marker) => markers.push(
                    marker
                        .trim_start_matches("… ")
                        .trim_end_matches(" omitted …")
                        .parse()
                        .unwrap(),
                ),
                element => kept.push(element["i"].as_u64().unwrap() as usize + 1),
            }
        }
        (kept, markers)
    }

    #[test]
    fn a_series_newest_last_keeps_its_first_element_newest_run_and_doubling_samples() {
        let mut data = Value::Array((0..1000).map(element).collect());

        let trimmed = trim(&mut data, 4096, &series("", SeriesEnd::Last));

        assert!(encoded_len(&data) <= 4096, "{}", encoded_len(&data));
        let (kept, markers) = kept_and_markers(&data);
        let mut expected = vec![1, 475, 731, 859, 923, 955, 971, 979, 983];
        expected.extend(985..=1000);
        assert_eq!(kept, expected);
        assert_eq!(markers, [473, 255, 127, 63, 31, 15, 7, 3, 1]);
        assert_eq!(
            trimmed,
            [Trimmed {
                path: String::new(),
                total: 1000,
                kept: 25,
                rule: TrimRule::NewestLast,
            }]
        );
    }

    #[test]
    fn a_series_newest_first_is_trimmed_as_the_mirror_of_newest_last() {
        let elements: Vec<_> = (0..50).map(element).collect();
        let mut newest_first = json!({"history": elements});
        let mut newest_last =
            json!({"history": elements.iter().rev().cloned().collect::<Vec<_>>()});

        let first = trim(
            &mut newest_first,
            4096,
            &series("/history", SeriesEnd::First),
        );
        let last = trim(&mut newest_last, 4096, &series("/history", SeriesEnd::Last));

        let mirrored: Vec<_> = newest_last["history"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .cloned()
            .collect();
        assert_eq!(newest_first["history"].as_array().unwrap(), &mirrored);
        let history = newest_first["history"].as_array().unwrap();
        assert_eq!(history[0]["i"], 0, "the newest element leads");
        assert_eq!(
            history.last().unwrap()["i"],
            49,
            "the oldest element closes"
        );
        assert_eq!(first[0].rule, TrimRule::NewestFirst);
        assert_eq!(first[0].path, "/history");
        assert_eq!(
            (first[0].total, first[0].kept),
            (last[0].total, last[0].kept)
        );
        assert!(encoded_len(&newest_first) <= 4096);
    }

    #[test]
    fn an_array_of_undeclared_order_keeps_a_head_and_a_tail_around_one_marker() {
        let mut data =
            json!({"rows": (0..100).map(element).collect::<Vec<_>>(), "symbol": "700.HK"});

        let trimmed = trim(&mut data, 4096, &[]);

        assert!(encoded_len(&data) <= 4096);
        assert_eq!(data["symbol"], "700.HK", "ordinary fields stay");
        let (kept, markers) = kept_and_markers(&data["rows"]);
        assert_eq!(markers.len(), 1, "{markers:?}");
        let head = kept.iter().take_while(|position| **position <= 50).count();
        let tail = kept.len() - head;
        assert!(head.abs_diff(tail) <= 1, "head {head}, tail {tail}");
        assert_eq!(kept[..head], (1..=head).collect::<Vec<_>>());
        assert_eq!(kept[head..], (101 - tail..=100).collect::<Vec<_>>());
        assert_eq!(markers[0], 100 - kept.len());
        assert_eq!(
            trimmed,
            [Trimmed {
                path: "/rows".into(),
                total: 100,
                kept: kept.len(),
                rule: TrimRule::HeadAndTail,
            }]
        );
    }

    #[test]
    fn a_long_text_keeps_its_first_line_and_as_many_last_lines_as_fit() {
        let lines: Vec<String> = (1..=200)
            .map(|line| format!("line {line:03} {}", "-".repeat(60)))
            .collect();
        let text = format!("{}\n\n\n", lines.join("\n"));
        let mut data = json!({"text": text, "exit": null});

        let trimmed = trim(&mut data, 4096, &[]);

        assert!(encoded_len(&data) <= 4096);
        assert_eq!(data["exit"], Value::Null);
        let shortened: Vec<&str> = data["text"].as_str().unwrap().split('\n').collect();
        assert_eq!(shortened[0], lines[0]);
        let kept_tail = shortened.len() - 2;
        assert_eq!(
            shortened[1],
            format!("… {} lines omitted …", 200 - 1 - kept_tail)
        );
        assert_eq!(shortened[2..], lines[200 - kept_tail..]);
        assert!(kept_tail > 40, "{kept_tail}");
        assert_eq!(
            trimmed,
            [Trimmed {
                path: "/text".into(),
                total: 200,
                kept: 1 + kept_tail,
                rule: TrimRule::Lines,
            }]
        );
    }

    #[test]
    fn a_small_state_is_left_as_it_is() {
        let original = json!({"symbol": "700.HK", "rows": [1, 2, 3], "text": "a\nb\n\n"});
        let mut data = original.clone();

        assert!(trim(&mut data, 4096, &[]).is_empty());
        assert_eq!(data, original);
    }

    #[test]
    fn the_largest_candidate_is_trimmed_first_and_the_next_only_when_still_over() {
        let mut data = json!({
            "big": (0..200).map(element).collect::<Vec<_>>(),
            "small": (0..10).map(element).collect::<Vec<_>>(),
        });

        let trimmed = trim(&mut data, 4096, &[]);

        assert_eq!(trimmed.len(), 1, "{trimmed:?}");
        assert_eq!(trimmed[0].path, "/big");
        assert_eq!(data["small"].as_array().unwrap().len(), 10);
        assert!(encoded_len(&data) <= 4096);
    }

    #[test]
    fn a_value_no_segment_can_shorten_enough_is_replaced_by_its_size() {
        let mut data = json!({"symbol": "700.HK", "note": "n".repeat(10_000)});

        let trimmed = trim(&mut data, 4096, &[]);

        assert_eq!(data["symbol"], "700.HK");
        assert_eq!(data["note"], "… omitted (10002 bytes) …");
        assert_eq!(
            trimmed,
            [Trimmed {
                path: "/note".into(),
                total: 10_002,
                kept: 0,
                rule: TrimRule::Omitted,
            }]
        );
    }

    /// Far above what a linear trim of these inputs takes even in a debug build on a loaded
    /// machine, far below the minutes measuring every candidate again after each cut took.
    const LINEAR_TRIM_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

    /// Trimming stays linear in its input, near the 512 KiB a Tick Update may take.
    #[test]
    fn trimming_the_largest_inputs_stays_within_a_time_bound() {
        let many_small_arrays = Value::Object(
            (0..10_000)
                .map(|index| {
                    (
                        format!("k{index:05}"),
                        json!(["x".repeat(18), "y".repeat(18)]),
                    )
                })
                .collect(),
        );
        let many_small_texts = Value::Object(
            (0..20_000)
                .map(|index| (format!("t{index:05}"), json!(format!("a{index}\nb\nc"))))
                .collect(),
        );
        let long_text = json!({"text": (0..50_000).map(|line| format!("line {line}")).collect::<Vec<_>>().join("\n")});
        let many_numbers = json!({"values": (0..100_000).collect::<Vec<_>>()});
        let mut deep = json!(vec!["leaf\nleaf"; 4]);
        for _ in 0..200 {
            deep = json!([deep, "x".repeat(2_000)]);
        }
        for (name, mut data) in [
            ("many small arrays", many_small_arrays),
            ("many small texts", many_small_texts),
            ("long text", long_text),
            ("many numbers", many_numbers),
            ("deep nesting", deep),
        ] {
            let started = std::time::Instant::now();
            let trimmed = trim(&mut data, 4096, &series("/values", SeriesEnd::Last));
            let elapsed = started.elapsed();
            assert!(elapsed < LINEAR_TRIM_BOUND, "{name} took {elapsed:?}");
            assert!(encoded_len(&data) <= 4096, "{name}: {}", encoded_len(&data));
            assert!(trimmed.len() <= MAX_TRIMMED_RECORDS, "{name}: {trimmed:?}");
        }
    }

    #[test]
    fn measured_sizes_match_the_encoding() {
        let data = json!({
            "quote\"d/key~": ["é\n\t\u{1}", 1.5e300, -0.0, 12345678901234567890u64, null, true],
            "nested": {"a": [[], {}, [1, [2, [3]]]], "b": "plain"},
        });
        let mut candidates = Vec::new();
        assert_eq!(
            measure(&data, &mut String::new(), 0, &mut candidates),
            encoded_len(&data)
        );
        for (path, size) in candidates {
            assert_eq!(size, encoded_len(data.pointer(&path).unwrap()), "{path}");
        }
    }

    #[test]
    fn a_trimmed_path_escapes_its_keys_as_a_json_pointer() {
        let mut data = json!({"a/b": {"c~d": (0..100).map(element).collect::<Vec<_>>()}});

        let trimmed = trim(&mut data, 4096, &[]);

        assert_eq!(trimmed[0].path, "/a~1b/c~0d");
    }
}
