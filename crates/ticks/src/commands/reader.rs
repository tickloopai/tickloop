use std::{cell::Cell, io::Write};

use crossterm::event::{Event, KeyCode};
use futures_util::Stream;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    widgets::{Block, Borders, Paragraph},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_ui::{
    format::{width, write_text},
    markdown,
};

use crate::{
    api::{MarketEvent, QuoteServices},
    cli::ReaderArgs,
    config::Settings,
    error::{Error, Result},
    input::{EditOutcome, VimInput},
    metadata::TickKind,
    output::TableData,
    reader::{Document, Library, library_documents},
    research::{
        ResearchContext, ResearchEvidence, ResearchNavigation, ResearchTarget, SetResearchContext,
    },
    runtime::{LiveView, NoMarketApi, ViewState},
    stream::TickStream,
    tools::{ToolCall, Tools},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadDocument {
    pub document_id: String,
    pub revision: Option<String>,
    pub section: Option<String>,
    #[serde(default)]
    pub offset: usize,
    pub max_characters: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    document_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenDocument {
    location: String,
    symbol: Option<String>,
    fiscal_period: Option<String>,
}

pub fn tools() -> Tools {
    tools_for(std::sync::Arc::new(std::sync::RwLock::new(Vec::new())))
}

fn tools_for(library: Library) -> Tools {
    let read_library = library.clone();
    let search_library = library.clone();
    Tools::default()
        .query("documents", "List loaded originals with their immutable revision and section IDs.", move |_, _: Empty| {
            let library = library.clone();
            Box::pin(async move { Ok(json!({"documents": library_documents(&library)?.iter().map(Document::metadata).collect::<Vec<_>>()})) })
        })
        .query("read_document", "Read a bounded slice of an original; supply revision to preserve an older citation. Offsets are Unicode characters.", move |_, args: ReadDocument| {
            let library = read_library.clone();
            Box::pin(async move { read_document(&library, &args) })
        })
        .query("search", "Find passages in loaded originals, retaining their document, revision and section.", move |_, args: Search| {
            let library = search_library.clone();
            Box::pin(async move {
                if args.query.trim().is_empty() { return Err(Error::usage("search query must not be empty")); }
                let documents = library_documents(&library)?;
                let mut matches = Vec::new();
                for document in &documents {
                    if args.document_id.as_ref().is_some_and(|id| id != &document.id) { continue; }
                    for section in &document.sections {
                        for found in search_ranges(&section.body,&args.query) {
                            let offset=section.body[..found.start].chars().count();
                            let text=section.body.chars().skip(offset.saturating_sub(60)).take(240).collect::<String>();
                            matches.push(json!({"document_id":document.id,"revision":document.revision,"section":section.id,"offset":offset,"text":text,"origin":document.origin,"fiscal_period":document.fiscal_period}));
                            if matches.len() >= 100 { return Ok(json!({"matches":matches,"truncated":true})); }
                        }
                    }
                }
                Ok(json!({"matches":matches,"truncated":false}))
            })
        })
        .action::<ReadDocument>("locate", "Return this Reader to an exact document revision and section cited earlier.")
        .action::<OpenDocument>("open_document", "Load an original URL or local file into this Reader; existing revisions remain available.")
        .action::<SetResearchContext>("set_context", "Follow one explicit earnings context; restore cited revisions before fetching originals, retaining earlier evidence.")
        .recover_with("documents")
}

fn normalized_search_text(source: &str) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut offsets = Vec::new();
    let mut previous_space = false;
    for (offset, character) in source.char_indices() {
        if character.is_whitespace() {
            if previous_space {
                continue;
            }
            text.push(' ');
            offsets.push(offset);
            previous_space = true;
        } else {
            previous_space = false;
            for character in character.to_lowercase() {
                text.push(character);
                offsets.extend(std::iter::repeat_n(offset, character.len_utf8()));
            }
        }
    }
    offsets.push(source.len());
    (text, offsets)
}

fn search_ranges(source: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let (query, _) = normalized_search_text(query.trim());
    if query.is_empty() {
        return Vec::new();
    }
    let (normalized, offsets) = normalized_search_text(source);
    normalized
        .match_indices(&query)
        .take(5000)
        .map(|(offset, _)| offsets[offset]..offsets[offset + query.len()])
        .collect()
}

pub fn read_document(library: &Library, args: &ReadDocument) -> Result<Value> {
    crate::reader::restore_revision(library, &args.document_id, args.revision.as_deref())?;
    let documents = library_documents(library)?;
    let document = find_document(&documents, args)?;
    let body = if let Some(section) = &args.section {
        document
            .sections
            .iter()
            .find(|item| &item.id == section)
            .ok_or_else(|| Error::usage("unknown document section"))?
            .body
            .clone()
    } else {
        document
            .sections
            .iter()
            .map(|section| section.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let limit = args.max_characters.unwrap_or(8000);
    if !(1..=32000).contains(&limit) {
        return Err(Error::usage("max_characters must be between 1 and 32000"));
    }
    let total = body.chars().count();
    if args.offset > total {
        return Err(Error::usage("offset is beyond the document"));
    }
    let text: String = body.chars().skip(args.offset).take(limit).collect();
    let next = args.offset + text.chars().count();
    Ok(
        json!({"document":document.metadata(),"section":args.section,"offset":args.offset,"text":text,"next_offset":(next<total).then_some(next),"total_characters":total}),
    )
}

fn find_document<'a>(documents: &'a [Document], args: &ReadDocument) -> Result<&'a Document> {
    documents.iter().rev().find(|document| document.id == args.document_id && args.revision.as_ref().is_none_or(|revision| revision == &document.revision))
        .ok_or_else(|| Error::usage("document revision is not loaded; the Reader cannot substitute a different original"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReaderMode {
    Reading,
    Documents,
    Sections,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorPurpose {
    Search,
    OpenOriginal,
}

pub struct ReaderView {
    library: Library,
    document: usize,
    section: Cell<usize>,
    selection: usize,
    mode: ReaderMode,
    scroll: Cell<usize>,
    page_size: Cell<usize>,
    lines: Cell<usize>,
    section_offsets: std::cell::RefCell<Vec<usize>>,
    pending_offset: Cell<Option<(usize, usize)>>,
    search: String,
    pending_search: Cell<Option<bool>>,
    search_matches: Cell<usize>,
    editor: Option<VimInput>,
    editor_purpose: EditorPurpose,
    notice: Option<String>,
    context: Option<ResearchContext>,
    navigation: Option<ResearchNavigation>,
    visible_offset: Cell<usize>,
    cache: Option<crate::reader::DocumentCache>,
}

impl ReaderView {
    pub fn new(library: Library) -> Self {
        let context = library_documents(&library).ok().and_then(|documents| {
            let document = documents.first()?;
            Some(ResearchContext {
                symbol: document.symbol.clone()?,
                period_id: document.fiscal_period.clone()?,
                ..ResearchContext::default()
            })
        });
        Self {
            library,
            document: 0,
            section: Cell::new(0),
            selection: 0,
            mode: ReaderMode::Reading,
            scroll: Cell::new(0),
            page_size: Cell::new(1),
            lines: Cell::new(0),
            section_offsets: std::cell::RefCell::new(Vec::new()),
            pending_offset: Cell::new(None),
            search: String::new(),
            pending_search: Cell::new(None),
            search_matches: Cell::new(0),
            editor: None,
            editor_purpose: EditorPurpose::Search,
            notice: None,
            context,
            navigation: None,
            visible_offset: Cell::new(0),
            cache: crate::reader::DocumentCache::configured(),
        }
    }

    pub fn with_document_cache(mut self, cache: crate::reader::DocumentCache) -> Self {
        self.cache = Some(cache);
        self
    }

    fn go_to_section(&self, section: usize) {
        self.section.set(section);
        if let Some(offset) = self.section_offsets.borrow().get(section) {
            self.scroll.set(*offset);
        }
    }

    fn search_next(&self, backwards: bool) {
        self.pending_search.set(Some(backwards));
    }

    fn selected_context(&self) -> Option<ResearchContext> {
        let documents = library_documents(&self.library).ok()?;
        let document = documents.get(self.document)?;
        let section = document.sections.get(self.section.get())?;
        let mut context = self.context.clone().or_else(|| {
            Some(ResearchContext {
                symbol: document.symbol.clone()?,
                period_id: document.fiscal_period.clone()?,
                ..ResearchContext::default()
            })
        })?;
        let (offset, text) = paragraph_at(&section.body, self.visible_offset.get())?;
        context.evidence = Some(ResearchEvidence {
            label: format!("{} · {}", document.title, section.title),
            text,
            origin: Some(document.origin.clone()),
            document_id: Some(document.id.clone()),
            revision: Some(document.revision.clone()),
            section: Some(section.id.clone()),
            offset,
            ..ResearchEvidence::default()
        });
        Some(context)
    }

    async fn follow_context(&mut self, context: ResearchContext) -> Result<Value> {
        context.validate()?;
        if let Some(evidence) = &context.evidence
            && let (Some(document_id), Some(revision)) = (&evidence.document_id, &evidence.revision)
        {
            let result = self.act(ToolCall { tool: "locate".into(), args: json!({"document_id":document_id,"revision":revision,"section":evidence.section,"offset":evidence.offset}) }).await?;
            self.context = Some(context);
            return Ok(result);
        }
        let mut failures = Vec::new();
        let mut loaded = None;
        let mut used_cached_original = false;
        for original in &context.documents {
            let document = match crate::reader::load_document(&original.location).await {
                Ok(mut document) => {
                    document.symbol = Some(context.symbol.clone());
                    document.fiscal_period = Some(context.period_id.clone());
                    document.published_at = original.published_at.clone();
                    document.warnings.push(format!(
                        "Period association: {}; {}",
                        original.verification, original.match_basis
                    ));
                    crate::reader::cache_document_in(&mut document, self.cache.as_ref());
                    document
                }
                Err(error) => {
                    failures.push(format!("{}: {}", original.title, error.message));
                    continue;
                }
            };
            let mut documents = self
                .library
                .write()
                .map_err(|_| Error::usage("document library unavailable"))?;
            let index = if let Some(index) = documents
                .iter()
                .position(|item| item.id == document.id && item.revision == document.revision)
            {
                index
            } else {
                if documents.len() >= 32 {
                    return Err(Error::usage("Reader holds at most 32 original revisions"));
                }
                documents.push(document);
                documents.len() - 1
            };
            loaded = Some(index);
            break;
        }
        if loaded.is_none() {
            let documents = library_documents(&self.library)?;
            loaded = documents.iter().rposition(|document| {
                document.symbol.as_deref() == Some(context.symbol.as_str())
                    && document.fiscal_period.as_deref() == Some(context.period_id.as_str())
            });
            if loaded.is_none()
                && let Some(cache) = &self.cache
            {
                match cache.for_period(&context.symbol, &context.period_id) {
                    Ok(Some(document)) => {
                        let mut documents = self
                            .library
                            .write()
                            .map_err(|_| Error::usage("document library unavailable"))?;
                        if documents.len() < 32 {
                            loaded = Some(documents.len());
                            documents.push(document);
                        }
                    }
                    Ok(None) => (),
                    Err(error) => failures.push(format!("Period cache: {}", error.message)),
                }
            }
            used_cached_original = loaded.is_some();
        }
        let index = loaded.ok_or_else(|| Error::network(if failures.is_empty() { "No period-matched original is available; attach a verified issuer document to this context".into() } else { failures.join("; ") }))?;
        self.document = index;
        self.context = Some(context.clone());
        self.section.set(0);
        self.scroll.set(0);
        self.visible_offset.set(0);
        self.section_offsets.borrow_mut().clear();
        self.mode = ReaderMode::Reading;
        let documents = library_documents(&self.library)?;
        let document = &documents[index];
        let found = context.evidence.as_ref().and_then(|evidence| {
            evidence.search_terms.iter().find_map(|term| {
                document
                    .sections
                    .iter()
                    .enumerate()
                    .find_map(|(section, item)| {
                        search_ranges(&item.body, term).first().map(|range| {
                            (
                                section,
                                item.body[..range.start].chars().count(),
                                term.clone(),
                            )
                        })
                    })
            })
        });
        if let Some((section, offset, term)) = found {
            self.section.set(section);
            self.visible_offset.set(offset);
            self.pending_offset.set(Some((section, offset)));
            self.search = term;
            self.notice = Some("Matched original text; verify its period, metric and units before treating it as the reported value".into());
        } else {
            self.search.clear();
            self.notice =
                Some("Period-matched original opened; no verified metric passage was found".into());
        }
        if used_cached_original {
            self.notice = Some(format!(
                "Cached original · retrieved {} · current sources unavailable; verify the original period and units",
                document.retrieved_at
            ));
        }
        Ok(
            json!({"document":document.metadata(),"context":context,"cached_original":used_cached_original,"unavailable_documents":failures}),
        )
    }
}

fn paragraph_at(body: &str, character_offset: usize) -> Option<(usize, String)> {
    let byte = body
        .char_indices()
        .nth(character_offset)
        .map(|(byte, _)| byte)
        .unwrap_or(body.len());
    let mut paragraphs = Vec::new();
    let mut start = None;
    let mut position = 0;
    for line in body.split_inclusive('\n') {
        if line.trim().is_empty() {
            if let Some(start) = start.take() {
                paragraphs.push(start..position);
            }
        } else if start.is_none() {
            start = Some(position);
        }
        position += line.len();
    }
    if let Some(start) = start {
        paragraphs.push(start..body.len());
    }
    let range = paragraphs
        .iter()
        .find(|range| range.end > byte)
        .or_else(|| paragraphs.last())?;
    let paragraph = &body[range.clone()];
    let content_start = range.start + paragraph.len() - paragraph.trim_start().len();
    let text = body[content_start..range.end]
        .trim_end()
        .chars()
        .take(4000)
        .collect();
    Some((body[..content_start].chars().count(), text))
}

#[async_trait::async_trait]
impl LiveView for ReaderView {
    fn tick(&self) -> TickKind {
        TickKind::Reader
    }
    fn tools(&self) -> Tools {
        tools_for(self.library.clone())
    }
    fn research_context(&self) -> Option<ResearchContext> {
        self.context.clone()
    }
    fn take_navigation(&mut self) -> Option<ResearchNavigation> {
        self.navigation.take()
    }
    fn title(&self) -> String {
        "Reader · Original documents".into()
    }
    fn priority(&self) -> &[usize] {
        &[0, 1, 2]
    }
    fn on_market(&mut self, _: &MarketEvent) {}
    fn captures_text(&self) -> bool {
        self.editor.is_some()
    }
    fn table(&self) -> TableData {
        TableData {
            headers: vec!["Title", "Source", "Fiscal period"],
            rows: library_documents(&self.library)
                .unwrap_or_default()
                .iter()
                .map(|document| {
                    vec![
                        json!(document.title),
                        json!(document.origin),
                        json!(document.fiscal_period),
                    ]
                })
                .collect(),
        }
    }
    fn content(&self) -> Value {
        json!({"documents":library_documents(&self.library).unwrap_or_default().iter().map(Document::metadata).collect::<Vec<_>>()})
    }
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        match call.tool.as_str() {
            "set_context" => {
                self.follow_context(call.arguments::<SetResearchContext>()?.context)
                    .await
            }
            "locate" => {
                let args: ReadDocument = call.arguments()?;
                crate::reader::restore_revision_from(
                    &self.library,
                    &args.document_id,
                    args.revision.as_deref(),
                    self.cache.as_ref(),
                )?;
                let result = read_document(&self.library, &args)?;
                let documents = library_documents(&self.library)?;
                let document = find_document(&documents, &args)?;
                let mut offset = args.offset;
                let section = if let Some(section) = &args.section {
                    document
                        .sections
                        .iter()
                        .position(|item| &item.id == section)
                        .ok_or_else(|| Error::usage("unknown document section"))?
                } else {
                    let mut target = 0;
                    for (index, section) in document.sections.iter().enumerate() {
                        target = index;
                        let length = section.body.chars().count();
                        if offset <= length {
                            break;
                        }
                        offset = offset.saturating_sub(length + 2);
                    }
                    target
                };
                self.document = documents
                    .iter()
                    .position(|item| std::ptr::eq(item, document))
                    .unwrap_or(0);
                if let Some((symbol, period)) = document
                    .symbol
                    .as_ref()
                    .zip(document.fiscal_period.as_ref())
                {
                    if self.context.as_ref().is_none_or(|context| {
                        &context.symbol != symbol || &context.period_id != period
                    }) {
                        self.context = Some(ResearchContext {
                            symbol: symbol.clone(),
                            period_id: period.clone(),
                            ..ResearchContext::default()
                        });
                    }
                } else {
                    self.context = None;
                }
                self.mode = ReaderMode::Reading;
                self.section_offsets.borrow_mut().clear();
                self.section.set(section);
                self.scroll.set(0);
                self.pending_offset.set(Some((section, offset)));
                self.visible_offset.set(offset);
                self.notice = Some("Returned to the cited original revision".into());
                Ok(result)
            }
            "open_document" => {
                let args: OpenDocument = call.arguments()?;
                let mut document = crate::reader::load_document(&args.location).await?;
                document.symbol = args.symbol;
                document.fiscal_period = args.fiscal_period;
                crate::reader::cache_document_in(&mut document, self.cache.as_ref());
                let mut documents = self
                    .library
                    .write()
                    .map_err(|_| Error::usage("document library unavailable"))?;
                if let Some(index) = documents
                    .iter()
                    .position(|item| item.id == document.id && item.revision == document.revision)
                {
                    self.document = index;
                    if documents[index].symbol.is_none() {
                        documents[index].symbol = document.symbol;
                    }
                    if documents[index].fiscal_period.is_none() {
                        documents[index].fiscal_period = document.fiscal_period;
                    }
                } else {
                    if documents.len() >= 32 {
                        return Err(Error::usage(
                            "Reader holds at most 32 original revisions; open another Reader",
                        ));
                    }
                    self.document = documents.len();
                    documents.push(document);
                }
                let metadata = documents[self.document].metadata();
                let selected = documents[self.document].clone();
                drop(documents);
                if let Some((symbol, period)) = selected
                    .symbol
                    .as_ref()
                    .zip(selected.fiscal_period.as_ref())
                {
                    if self.context.as_ref().is_none_or(|context| {
                        &context.symbol != symbol || &context.period_id != period
                    }) {
                        self.context = Some(ResearchContext {
                            symbol: symbol.clone(),
                            period_id: period.clone(),
                            ..ResearchContext::default()
                        });
                    }
                    if let Some(context) = &mut self.context
                        && !context
                            .documents
                            .iter()
                            .any(|original| original.location == selected.origin)
                    {
                        context.documents.insert(
                            0,
                            crate::research::ResearchDocument {
                                title: selected.title.clone(),
                                location: selected.origin.clone(),
                                kind: selected.format.clone(),
                                published_at: selected.published_at.clone(),
                                verification: "user_supplied_period_not_verified".into(),
                                match_basis: "explicit_original_selection".into(),
                            },
                        );
                    }
                }
                self.section.set(0);
                self.scroll.set(0);
                self.section_offsets.borrow_mut().clear();
                self.mode = ReaderMode::Reading;
                Ok(metadata)
            }
            _ => Err(Error::usage("unknown Reader action")),
        }
    }
    async fn on_key(&mut self, key: KeyCode, _: &QuoteServices, _: &mut ViewState) -> Result<bool> {
        if let Some(editor) = &mut self.editor {
            match editor.handle(key) {
                EditOutcome::Submit => {
                    let text = editor.text().trim().to_owned();
                    self.editor = None;
                    if self.editor_purpose == EditorPurpose::OpenOriginal {
                        let context = self.context.clone();
                        match self.act(ToolCall { tool: "open_document".into(), args: json!({"location":text,"symbol":context.as_ref().map(|context| &context.symbol),"fiscal_period":context.as_ref().map(|context| &context.period_id)}) }).await {
                            Ok(_) => self.notice = Some("Original supplied by user; verify the fiscal period and metric in its text".into()),
                            Err(error) => { self.notice = Some(error.message); self.editor = Some(VimInput::new(text)); }
                        }
                    } else {
                        self.search = text;
                        if !self.search.is_empty() {
                            self.search_next(false);
                        }
                    }
                }
                EditOutcome::Cancel => self.editor = None,
                _ => (),
            }
            return Ok(true);
        }
        if key == KeyCode::Char('o') {
            self.editor_purpose = EditorPurpose::OpenOriginal;
            self.editor = Some(VimInput::new(String::new()));
            return Ok(true);
        }
        if key == KeyCode::Char('R') {
            if let Some(context) = self.context.clone()
                && let Err(error) = self.follow_context(context).await
            {
                self.notice = Some(error.message);
            }
            return Ok(true);
        }
        if key == KeyCode::Char('a') {
            let has_document = library_documents(&self.library)?
                .get(self.document)
                .is_some();
            if let Some(context) = self
                .selected_context()
                .or_else(|| (!has_document).then(|| self.context.clone()).flatten())
            {
                if context
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.text.trim().is_empty())
                {
                    self.notice =
                        Some("No nonempty original paragraph is available to send to Chat".into());
                    return Ok(true);
                }
                self.navigation = Some(ResearchNavigation {
                    target: ResearchTarget::Chat,
                    context,
                });
            } else {
                self.notice = Some(
                    if has_document {
                        "No nonempty original paragraph is available to send to Chat"
                    } else {
                        "Choose a company and fiscal period before sending this paragraph to Chat"
                    }
                    .into(),
                );
            }
            return Ok(true);
        }
        let documents = library_documents(&self.library)?;
        let Some(document) = documents.get(self.document) else {
            return Ok(false);
        };
        if self.mode != ReaderMode::Reading {
            let count = if self.mode == ReaderMode::Documents {
                documents.len()
            } else {
                document.sections.len()
            };
            match key {
                KeyCode::Esc => self.mode = ReaderMode::Reading,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selection = (self.selection + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selection = self.selection.saturating_sub(1)
                }
                KeyCode::Home | KeyCode::Char('g') => self.selection = 0,
                KeyCode::End | KeyCode::Char('G') => self.selection = count.saturating_sub(1),
                KeyCode::Enter => {
                    if self.mode == ReaderMode::Documents {
                        self.document = self.selection;
                        if let Some(document) = documents.get(self.document) {
                            self.context = document
                                .symbol
                                .as_ref()
                                .zip(document.fiscal_period.as_ref())
                                .map(|(symbol, period)| ResearchContext {
                                    symbol: symbol.clone(),
                                    period_id: period.clone(),
                                    ..ResearchContext::default()
                                });
                        }
                        self.section.set(0);
                        self.scroll.set(0);
                        self.visible_offset.set(0);
                        self.section_offsets.borrow_mut().clear();
                    } else {
                        self.go_to_section(self.selection);
                    }
                    self.mode = ReaderMode::Reading;
                }
                _ => (),
            }
            return Ok(true);
        }
        match key {
            KeyCode::Esc => {
                self.notice = None;
                self.search.clear();
            }
            KeyCode::Char('d') => {
                self.mode = ReaderMode::Documents;
                self.selection = self.document;
            }
            KeyCode::Char('t') => {
                self.mode = ReaderMode::Sections;
                self.selection = self.section.get();
            }
            KeyCode::Char('/') => {
                self.editor_purpose = EditorPurpose::Search;
                self.editor = Some(VimInput::new(self.search.clone()));
            }
            KeyCode::Char('n') if !self.search.is_empty() => self.search_next(false),
            KeyCode::Char('N') if !self.search.is_empty() => self.search_next(true),
            KeyCode::Char('[') => self.go_to_section(self.section.get().saturating_sub(1)),
            KeyCode::Char(']') => self.go_to_section(
                (self.section.get() + 1).min(document.sections.len().saturating_sub(1)),
            ),
            KeyCode::Down | KeyCode::Char('j') => self
                .scroll
                .set((self.scroll.get() + 1).min(self.lines.get().saturating_sub(1))),
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll.set(self.scroll.get().saturating_sub(1))
            }
            KeyCode::PageDown => self.scroll.set(
                self.scroll
                    .get()
                    .saturating_add(self.page_size.get())
                    .min(self.lines.get().saturating_sub(1)),
            ),
            KeyCode::PageUp => self
                .scroll
                .set(self.scroll.get().saturating_sub(self.page_size.get())),
            KeyCode::Home | KeyCode::Char('g') => self.scroll.set(0),
            KeyCode::End | KeyCode::Char('G') => self
                .scroll
                .set(self.lines.get().saturating_sub(self.page_size.get())),
            KeyCode::Char('c') => {
                self.notice = Some(format!(
                    "{} @ {} · {}",
                    document.id,
                    &document.revision[..12],
                    document.sections[self.section.get()].id
                ))
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn help(&self) -> &'static str {
        "j/k / Ctrl+N/P / arrows  Navigate / scroll\nEnter  Open selected document / section\nd  Documents    t  Table of contents\n[ / ]  Previous / next section\n/  Search    n / N  Next / previous visible match\nc  Show stable citation    a  Ask Chat about visible paragraph\no  Open original URL/path    R  Retry context originals\nPageUp/PageDown  Page    g/G  Start/end\nEsc  Leave Insert, then cancel / return\nq / Ctrl+C  Quit    ?  Help\nSearch editor: i/a Insert, h/l move, x delete, u undo"
    }
    fn draw(&self, frame: &mut Frame<'_>, _: &mut ViewState, settings: &Settings) {
        let styles = settings.styles();
        let area = frame.area();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Reader · 财报原文 ")
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(1),
        ));
        frame.render_widget(
            block,
            Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)),
        );
        if inner.width == 0 || inner.height < 4 {
            return;
        }
        let documents = library_documents(&self.library).unwrap_or_default();
        let Some(document) = documents.get(self.document) else {
            let context = self
                .context
                .as_ref()
                .map(ResearchContext::title)
                .unwrap_or_else(|| "Reader".into());
            let state = self
                .notice
                .as_deref()
                .unwrap_or("No readable original is currently available for this fiscal period.");
            let text = format!(
                "{context}\n\n{state}\n\nR retry matched materials · o open another original\na ask Chat about this event"
            );
            frame.render_widget(
                Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
                inner,
            );
            if let Some(editor) = &self.editor {
                let line = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
                frame.render_widget(ratatui::widgets::Clear, line);
                let label = format!("[{}] Original: ", editor.mode_label());
                let prefix = width(&label, settings.ambiguous_width);
                let (text, cursor) = editor.viewport(
                    (inner.width as usize).saturating_sub(prefix),
                    settings.ambiguous_width,
                );
                write_text(
                    frame.buffer_mut(),
                    line.x,
                    line.y,
                    &format!("{label}{text}"),
                    line.width as usize,
                    settings.ambiguous_width,
                    styles.text,
                );
                if prefix < inner.width as usize {
                    frame.set_cursor_position((line.x + (prefix + cursor) as u16, line.y));
                }
            }
            return;
        };
        let header = format!(
            "{} · {}\n{}\nPublished: {} · {}",
            document.title,
            document
                .fiscal_period
                .as_deref()
                .unwrap_or("period unspecified"),
            document.origin,
            document.published_at.as_deref().unwrap_or("unknown"),
            &document.revision[..12]
        );
        frame.render_widget(
            Paragraph::new(header).style(styles.text),
            Rect::new(inner.x, inner.y, inner.width, 3),
        );
        let body = Rect::new(
            inner.x,
            inner.y + 3,
            inner.width,
            inner.height.saturating_sub(4),
        );
        self.page_size.set(body.height.max(1) as usize);
        if self.mode != ReaderMode::Reading {
            let entries: Vec<String> = if self.mode == ReaderMode::Documents {
                documents
                    .iter()
                    .map(|document| {
                        format!(
                            "{} · {}",
                            document.title,
                            document.fiscal_period.as_deref().unwrap_or("—")
                        )
                    })
                    .collect()
            } else {
                document
                    .sections
                    .iter()
                    .map(|section| format!("{} · {}", section.id, section.title))
                    .collect()
            };
            let offset = self
                .selection
                .saturating_sub(body.height.saturating_sub(1) as usize);
            for (row, (index, text)) in entries
                .iter()
                .enumerate()
                .skip(offset)
                .take(body.height as usize)
                .enumerate()
            {
                write_text(
                    frame.buffer_mut(),
                    body.x,
                    body.y + row as u16,
                    text,
                    body.width as usize,
                    settings.ambiguous_width,
                    if index == self.selection {
                        styles.text.patch(styles.selection)
                    } else {
                        styles.text
                    },
                );
            }
        } else {
            let mut lines = Vec::new();
            let mut offsets = Vec::new();
            let mut match_rows = Vec::new();
            for section in &document.sections {
                offsets.push(lines.len());
                if !self.search.is_empty() {
                    let layout = markdown::layout(
                        &section.body,
                        body.width as usize,
                        settings.ambiguous_width,
                    );
                    for range in search_ranges(&section.body, &self.search) {
                        let first = layout
                            .iter()
                            .rposition(|line| line.source_offset <= range.start)
                            .unwrap_or(0);
                        let last = layout
                            .iter()
                            .rposition(|line| line.source_offset < range.end)
                            .unwrap_or(first);
                        match_rows.push((lines.len() + first)..=(lines.len() + last));
                    }
                }
                lines.extend(markdown::render(
                    &section.body,
                    body.width as usize,
                    settings.ambiguous_width,
                    &styles,
                ));
                lines.extend(markdown::render(
                    "\n",
                    body.width as usize,
                    settings.ambiguous_width,
                    &styles,
                ));
            }
            if self.section_offsets.borrow().is_empty() {
                self.scroll
                    .set(*offsets.get(self.section.get()).unwrap_or(&0));
            }
            *self.section_offsets.borrow_mut() = offsets.clone();
            self.lines.set(lines.len());
            if let Some((section, character_offset)) = self.pending_offset.take()
                && let Some(section_body) = document.sections.get(section)
            {
                let byte_offset = section_body
                    .body
                    .char_indices()
                    .nth(character_offset)
                    .map(|(offset, _)| offset)
                    .unwrap_or(section_body.body.len());
                let section_lines = markdown::layout(
                    &section_body.body,
                    body.width as usize,
                    settings.ambiguous_width,
                );
                let row = section_lines
                    .iter()
                    .rposition(|line| line.source_offset <= byte_offset)
                    .unwrap_or(0);
                self.scroll.set(offsets[section] + row);
            }
            let matches: Vec<usize> = match_rows.iter().map(|range| *range.start()).collect();
            self.search_matches.set(matches.len());
            if let Some(backwards) = self.pending_search.take() {
                let target = if backwards {
                    matches
                        .iter()
                        .rev()
                        .find(|index| **index < self.scroll.get())
                        .or(matches.last())
                } else {
                    matches
                        .iter()
                        .find(|index| **index > self.scroll.get())
                        .or(matches.first())
                };
                if let Some(target) = target {
                    self.scroll.set(*target);
                }
            }
            let offset = self.scroll.get().min(lines.len().saturating_sub(1));
            self.scroll.set(offset);
            self.section.set(
                offsets
                    .iter()
                    .rposition(|start| *start <= offset)
                    .unwrap_or(0),
            );
            if let Some(section) = document.sections.get(self.section.get()) {
                let row = offset.saturating_sub(offsets[self.section.get()]);
                let layout =
                    markdown::layout(&section.body, body.width as usize, settings.ambiguous_width);
                if let Some(line) = layout.get(row) {
                    self.visible_offset
                        .set(section.body[..line.source_offset].chars().count());
                }
            }
            for (row, line) in lines
                .iter()
                .skip(offset)
                .take(body.height as usize)
                .enumerate()
            {
                let mut x = body.x;
                let line_text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let matching = match_rows
                    .iter()
                    .any(|range| range.contains(&(offset + row)))
                    || document
                        .sections
                        .get(self.section.get())
                        .is_some_and(|section| line_text.trim() == section.title);
                for span in &line.spans {
                    write_text(
                        frame.buffer_mut(),
                        x,
                        body.y + row as u16,
                        &span.content,
                        body.right().saturating_sub(x) as usize,
                        settings.ambiguous_width,
                        if matching {
                            span.style.patch(styles.selection)
                        } else {
                            span.style
                        },
                    );
                    x = x.saturating_add(width(&span.content, settings.ambiguous_width) as u16);
                }
            }
        }
        let footer = if let Some(editor) = &self.editor {
            let prefix = format!(
                "[{}] {}",
                editor.mode_label(),
                if self.editor_purpose == EditorPurpose::Search {
                    "/"
                } else {
                    "Original: "
                }
            );
            let prefix_width = width(&prefix, settings.ambiguous_width);
            let (text, cursor) = editor.viewport(
                (inner.width as usize).saturating_sub(prefix_width),
                settings.ambiguous_width,
            );
            frame.set_cursor_position((
                inner
                    .x
                    .saturating_add((prefix_width + cursor) as u16)
                    .min(inner.right() - 1),
                inner.bottom() - 1,
            ));
            format!("{prefix}{text}")
        } else if let Some(notice) = &self.notice {
            notice.clone()
        } else if !self.search.is_empty() {
            format!(
                "/{} · {} matches · n/N next/previous",
                self.search,
                self.search_matches.get()
            )
        } else {
            format!(
                "d Docs  t Contents  / Search  [ ] Section  {}/{}",
                self.section.get() + 1,
                document.sections.len()
            )
        };
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.bottom() - 1,
            &footer,
            inner.width as usize,
            settings.ambiguous_width,
            styles.muted_text,
        );
    }
}

pub async fn run_once(
    args: &ReaderArgs,
    library: Library,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let view = ReaderView::new(library);
    updates.publish(view.content())?;
    match args.output.format {
        crate::cli::OutputFormat::Json => {
            serde_json::to_writer_pretty(
                &mut *output,
                &json!({"documents":library_documents(&view.library)?}),
            )
            .map_err(|error| Error::usage(error.to_string()))?;
            writeln!(output)?;
            Ok(())
        }
        crate::cli::OutputFormat::Table => {
            view.table()
                .write(args.output.format, settings, color, output)?;
            for document in library_documents(&view.library)? {
                writeln!(
                    output,
                    "\n{} · {}\nSource: {}\nRevision: {}\n",
                    document.title,
                    document
                        .fiscal_period
                        .as_deref()
                        .unwrap_or("period unspecified"),
                    document.origin,
                    document.revision
                )?;
                for section in document.sections {
                    writeln!(output, "{}\n{}\n", section.id, section.body)?;
                }
            }
            Ok(())
        }
    }
}

pub async fn run_live<B: Backend, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    library: Library,
    settings: &Settings,
    terminal: &mut Terminal<B>,
    input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let mut view = ReaderView::new(library);
    if let Some(context) = ResearchContext::from_environment()? {
        view.context = Some(context.clone());
        if let Err(error) = view.follow_context(context).await {
            view.notice = Some(error.message);
        }
    }
    let (_sender, events) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::run(
        &mut view,
        &NoMarketApi,
        settings,
        terminal,
        input,
        events,
        updates,
    )
    .await
}

#[cfg(test)]
mod paragraph_tests {
    use super::paragraph_at;

    #[test]
    fn apple_original_whitespace_selects_the_next_real_paragraph_at_its_exact_offset() {
        let body = "# \n                        \n    \n        Apple reports third quarter results\n    \n\n                    \n\n\n                \n            \n\n\n        \n\n\n\n        \n\n\n            \n\n\n                \n                \n                    June quarter records for total company revenue and EPS\n \niPhone, Mac and Services revenue set new June quarter records\n                \n            \n\n";
        let (offset, text) = paragraph_at(body, 161).unwrap();
        assert_eq!(
            text,
            "June quarter records for total company revenue and EPS"
        );
        assert_eq!(
            offset,
            body[..body.find("June quarter records").unwrap()]
                .chars()
                .count()
        );
        assert_eq!(
            body.chars()
                .skip(offset)
                .take(text.chars().count())
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn trailing_whitespace_uses_the_previous_paragraph_and_counts_unicode_characters() {
        let body = "财报\n\n   \n\n  Revenue $109.4 billion.\n\n     \n";
        let (offset, text) = paragraph_at(body, body.chars().count()).unwrap();
        assert_eq!(text, "Revenue $109.4 billion.");
        assert_eq!(
            offset,
            body[..body.find("Revenue").unwrap()].chars().count()
        );
        assert_eq!(
            body.chars()
                .skip(offset)
                .take(text.chars().count())
                .collect::<String>(),
            text
        );
        assert!(paragraph_at(" \n\n  \t\n", 2).is_none());
    }
}
