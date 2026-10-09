use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::args::ReaderArgs;
use tick_provider::error::{Error, Result};

const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCUMENTS: usize = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CACHED_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PERIOD_INDEX_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct CachedPeriodOriginal {
    document_id: String,
    revision: String,
}

#[derive(Clone)]
pub struct DocumentCache {
    directory: PathBuf,
}

impl DocumentCache {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn configured() -> Option<Self> {
        let directory = std::env::var_os("TICKLOOP_READER_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("TICKLOOP_CONFIG_DIR")
                    .map(|path| PathBuf::from(path).join("research/documents"))
            })
            .or_else(|| {
                std::env::home_dir().map(|home| home.join(".tickloop/research/documents"))
            })?;
        Some(Self::new(directory))
    }

    fn path(&self, document_id: &str, revision: &str) -> Result<PathBuf> {
        let valid_hash =
            |value: &str| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !document_id.strip_prefix("doc-").is_some_and(valid_hash) || !valid_hash(revision) {
            return Err(Error::usage("invalid document revision identity"));
        }
        Ok(self
            .directory
            .join(document_id)
            .join(format!("{revision}.json")))
    }

    pub fn save(&self, document: &Document) -> Result<()> {
        if document.revision != section_revision(&document.sections) {
            return Err(Error::usage("document content does not match its revision"));
        }
        let path = self.path(&document.id, &document.revision)?;
        if path.exists() {
            self.load(&document.id, &document.revision)?;
            return self.remember_period(document);
        }
        let directory = path.parent().expect("cache file has a directory");
        std::fs::create_dir_all(directory)?;
        let bytes =
            serde_json::to_vec(document).map_err(|error| Error::usage(error.to_string()))?;
        if bytes.len() as u64 > MAX_CACHED_DOCUMENT_BYTES {
            return Err(Error::usage("document cache record exceeds 16 MiB"));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(path) {
            Ok(_) => (),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.error.into()),
        }
        self.remember_period(document)
    }

    fn period_path(&self, symbol: &str, period: &str) -> PathBuf {
        let key = format!("{symbol}\0{period}");
        self.directory
            .join("periods")
            .join(format!("{:x}.json", Sha1::digest(key.as_bytes())))
    }

    fn period_originals(&self, symbol: &str, period: &str) -> Result<Vec<CachedPeriodOriginal>> {
        let path = self.period_path(symbol, period);
        if !path.exists() {
            return Ok(Vec::new());
        }
        if std::fs::metadata(&path)?.len() > MAX_PERIOD_INDEX_BYTES {
            return Err(Error::usage("original period index exceeds 64 KiB"));
        }
        serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|error| Error::usage(format!("invalid original period index: {error}")))
    }

    fn remember_period(&self, document: &Document) -> Result<()> {
        let Some((symbol, period)) = document
            .symbol
            .as_deref()
            .zip(document.fiscal_period.as_deref())
        else {
            return Ok(());
        };
        let mut originals = self.period_originals(symbol, period)?;
        originals.retain(|entry| {
            entry.document_id != document.id || entry.revision != document.revision
        });
        originals.push(CachedPeriodOriginal {
            document_id: document.id.clone(),
            revision: document.revision.clone(),
        });
        if originals.len() > MAX_DOCUMENTS {
            originals.remove(0);
        }
        let path = self.period_path(symbol, period);
        let directory = path.parent().expect("period index has a directory");
        std::fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temporary, &originals)
            .map_err(|error| Error::usage(error.to_string()))?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|error| Error::from(error.error))?;
        Ok(())
    }

    pub fn for_period(&self, symbol: &str, period: &str) -> Result<Option<Document>> {
        for original in self.period_originals(symbol, period)?.iter().rev() {
            let Ok(mut document) = self.load(&original.document_id, &original.revision) else {
                continue;
            };
            if document
                .symbol
                .as_deref()
                .is_some_and(|value| value != symbol)
                || document
                    .fiscal_period
                    .as_deref()
                    .is_some_and(|value| value != period)
            {
                continue;
            }
            document.symbol = Some(symbol.into());
            document.fiscal_period = Some(period.into());
            return Ok(Some(document));
        }
        Ok(None)
    }

    pub fn load(&self, document_id: &str, revision: &str) -> Result<Document> {
        let path = self.path(document_id, revision)?;
        if std::fs::metadata(&path)?.len() > MAX_CACHED_DOCUMENT_BYTES {
            return Err(Error::usage("document cache record exceeds 16 MiB"));
        }
        let bytes = std::fs::read(path)?;
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|error| Error::usage(format!("invalid cached original: {error}")))?;
        if document.id != document_id
            || document.revision != revision
            || document.revision != section_revision(&document.sections)
            || document.id != format!("doc-{:x}", Sha1::digest(document.origin.as_bytes()))
            || document.sections.is_empty()
        {
            return Err(Error::usage(
                "cached original does not match the requested revision",
            ));
        }
        Ok(document)
    }
}

pub fn cache_document(document: &mut Document) {
    cache_document_in(document, DocumentCache::configured().as_ref());
}

pub fn cache_document_in(document: &mut Document, cache: Option<&DocumentCache>) {
    if let Some(cache) = cache
        && let Err(error) = cache.save(document)
    {
        document.warnings.push(format!(
            "Original revision could not be preserved: {}",
            error.message
        ));
    }
}

pub fn restore_revision(
    library: &Library,
    document_id: &str,
    revision: Option<&str>,
) -> Result<()> {
    restore_revision_from(
        library,
        document_id,
        revision,
        DocumentCache::configured().as_ref(),
    )
}

pub fn restore_revision_from(
    library: &Library,
    document_id: &str,
    revision: Option<&str>,
    cache: Option<&DocumentCache>,
) -> Result<()> {
    if library_documents(library)?.iter().any(|document| {
        document.id == document_id && revision.is_none_or(|revision| revision == document.revision)
    }) {
        return Ok(());
    }
    let revision = revision
        .ok_or_else(|| Error::usage("missing original needs an exact revision to restore"))?;
    let document = cache
        .ok_or_else(|| Error::usage("original revision cache is unavailable"))?
        .load(document_id, revision)?;
    let mut documents = library
        .write()
        .map_err(|_| Error::usage("document library unavailable"))?;
    if documents.len() >= MAX_DOCUMENTS {
        return Err(Error::usage("Reader holds at most 32 original revisions"));
    }
    if !documents
        .iter()
        .any(|item| item.id == document.id && item.revision == document.revision)
    {
        documents.push(document);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocumentSection {
    pub id: String,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Document {
    pub id: String,
    pub revision: String,
    pub title: String,
    pub origin: String,
    pub retrieved_at: String,
    pub published_at: Option<String>,
    pub symbol: Option<String>,
    pub fiscal_period: Option<String>,
    pub format: String,
    pub sections: Vec<DocumentSection>,
    pub warnings: Vec<String>,
}

impl Document {
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "id":self.id,"revision":self.revision,"title":self.title,"origin":self.origin,
            "retrieved_at":self.retrieved_at,"published_at":self.published_at,
            "symbol":self.symbol,"fiscal_period":self.fiscal_period,"format":self.format,
            "sections":self.sections.iter().map(|section| serde_json::json!({"id":section.id,"title":section.title,"characters":section.body.chars().count()})).collect::<Vec<_>>(),
            "warnings":self.warnings,
        })
    }

    pub fn from_text(origin: &str, title: &str, body: &str) -> Self {
        let sections = split_sections(body);
        Self {
            id: format!("doc-{:x}", Sha1::digest(origin.as_bytes())),
            revision: section_revision(&sections),
            title: title.into(),
            origin: origin.into(),
            retrieved_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
            published_at: None,
            symbol: None,
            fiscal_period: None,
            format: "markdown".into(),
            sections,
            warnings: Vec::new(),
        }
    }
}

fn section_revision(sections: &[DocumentSection]) -> String {
    format!(
        "{:x}",
        Sha1::digest(serde_json::to_vec(sections).expect("text sections serialize"))
    )
}

pub type Library = Arc<RwLock<Vec<Document>>>;

pub fn library_documents(library: &Library) -> Result<Vec<Document>> {
    library
        .read()
        .map(|documents| documents.clone())
        .map_err(|_| Error::usage("document library is unavailable"))
}

pub fn validate_location(location: &str, working_directory: &Path) -> Result<()> {
    if location.starts_with("http://") || location.starts_with("https://") {
        let url =
            reqwest::Url::parse(location).map_err(|_| Error::usage("invalid document URL"))?;
        if url.host_str().is_none() {
            return Err(Error::usage("document URL needs a host"));
        }
        return Ok(());
    }
    if location.contains("://") {
        return Err(Error::usage(
            "Reader supports HTTP(S) URLs or local file paths",
        ));
    }
    let path = working_directory.join(location);
    let metadata = std::fs::metadata(path)
        .map_err(|error| Error::usage(format!("cannot open document: {error}")))?;
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(Error::usage("document must be a file smaller than 8 MiB"));
    }
    Ok(())
}

pub async fn load_for_context(
    args: &ReaderArgs,
    context: Option<&crate::research::ResearchContext>,
) -> Result<Library> {
    if let Some(context) = context {
        context.validate()?;
        Ok(Arc::new(RwLock::new(Vec::new())))
    } else {
        load(args).await
    }
}

pub async fn load(args: &ReaderArgs) -> Result<Library> {
    if let Some(published_at) = &args.published_at {
        OffsetDateTime::parse(published_at, &Rfc3339).map_err(|_| {
            Error::usage("published_at must be RFC3339 with an explicit UTC offset")
        })?;
    }
    if args.documents.is_empty() || args.documents.len() > MAX_DOCUMENTS {
        return Err(Error::usage(
            "Reader needs between 1 and 32 document locations",
        ));
    }
    let mut documents = Vec::new();
    for origin in &args.documents {
        let mut document = load_document(origin).await?;
        document.symbol = args.symbol.clone();
        document.fiscal_period = args.fiscal_period.clone();
        document.published_at = args.published_at.clone();
        if document.published_at.is_some() {
            document
                .warnings
                .push("Publication time supplied by caller; not independently verified".into());
        }
        cache_document(&mut document);
        if !documents
            .iter()
            .any(|existing: &Document| existing.id == document.id)
        {
            documents.push(document);
        }
    }
    Ok(Arc::new(RwLock::new(documents)))
}

pub async fn load_document(origin: &str) -> Result<Document> {
    validate_location(origin, &std::env::current_dir()?)?;
    let (bytes, content_type, resolved_origin) =
        if origin.starts_with("https://") || origin.starts_with("http://") {
            let client = reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(5))
                .user_agent("TickLoop/0.1 document reader")
                .build()
                .map_err(|error| Error::network(error.to_string()))?;
            let mut response = client
                .get(origin)
                .send()
                .await
                .map_err(|error| Error::network(format!("document request failed: {error}")))?
                .error_for_status()
                .map_err(|error| Error::network(format!("document unavailable: {error}")))?;
            if response
                .content_length()
                .is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64)
            {
                return Err(Error::usage("document exceeds the 8 MiB limit"));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let resolved = response.url().to_string();
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| Error::network(error.to_string()))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_DOCUMENT_BYTES {
                    return Err(Error::usage("document exceeds the 8 MiB limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            (bytes, content_type, resolved)
        } else {
            let path = Path::new(origin)
                .canonicalize()
                .map_err(|error| Error::usage(format!("cannot open document: {error}")))?;
            let metadata = std::fs::metadata(&path)?;
            if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES as u64 {
                return Err(Error::usage("document must be a file smaller than 8 MiB"));
            }
            (
                std::fs::read(&path)?,
                String::new(),
                path.to_string_lossy().into_owned(),
            )
        };
    let name = resolved_origin
        .split('/')
        .next_back()
        .unwrap_or("Document")
        .split('?')
        .next()
        .unwrap_or("Document");
    let mut warnings = Vec::new();
    let (title, body, format) =
        if bytes.starts_with(b"%PDF-") || content_type.contains("application/pdf") {
            let extracted = tokio::task::spawn_blocking(move || extract_pdf(&bytes))
                .await
                .map_err(|error| Error::usage(error.to_string()))??;
            let body = extracted
                .split('\u{c}')
                .filter(|page| !page.trim().is_empty())
                .enumerate()
                .map(|(index, page)| {
                    format!(
                        "# Page {}\n\n```text\n{}\n```\n",
                        index + 1,
                        page.trim_end()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            warnings.push(
                "PDF text extraction preserves text, not the page layout; scanned pages need OCR"
                    .into(),
            );
            (name.to_owned(), body, "pdf")
        } else {
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::usage("document is not UTF-8 text, HTML or a supported PDF"))?;
            if content_type.contains("text/html")
                || text
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("<!doctype html")
                || text.to_ascii_lowercase().contains("<html")
            {
                let (title, body) = html_to_markdown(&text, &resolved_origin);
                (title.unwrap_or_else(|| name.to_owned()), body, "html")
            } else {
                (name.to_owned(), text, "markdown")
            }
        };
    let body = sanitize_text(&body);
    if body.trim().is_empty() {
        return Err(Error::usage(
            "document contains no extractable text; scanned PDFs require OCR",
        ));
    }
    let mut document = Document::from_text(&resolved_origin, &title, &body);
    document.format = format.into();
    document.warnings = warnings;
    Ok(document)
}

fn extract_pdf(bytes: &[u8]) -> Result<String> {
    let mut pdf = tempfile::Builder::new()
        .prefix("tickloop-document-")
        .suffix(".pdf")
        .tempfile()?;
    pdf.write_all(bytes)?;
    let mut child = std::process::Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(pdf.path())
        .arg("-")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            Error::usage(format!("PDF reading requires pdftotext (Poppler): {error}"))
        })?;
    let started = std::time::Instant::now();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output = std::thread::spawn(move || {
        use std::io::Read;
        let mut text = Vec::new();
        stdout
            .take((MAX_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut text)
            .map(|_| text)
    });
    let errors = std::thread::spawn(move || {
        use std::io::Read;
        let mut text = String::new();
        let _ = stderr.take(4096).read_to_string(&mut text);
        text
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= FETCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::usage("PDF extraction timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = output
        .join()
        .map_err(|_| Error::usage("PDF extraction failed"))??;
    let error = errors.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::usage(format!(
            "PDF extraction failed: {}",
            sanitize_text(&error)
        )));
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::usage("extracted PDF text exceeds 8 MiB"));
    }
    String::from_utf8(bytes).map_err(|_| Error::usage("PDF extractor returned invalid UTF-8"))
}

pub fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

fn split_sections(body: &str) -> Vec<DocumentSection> {
    let mut sections = Vec::new();
    let mut title = "Document".to_owned();
    let mut text = String::new();
    let mut fence: Option<&str> = None;
    for line in body.lines() {
        let marker = if line.trim_start().starts_with("```") {
            Some("```")
        } else if line.trim_start().starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(marker) = marker {
            fence = if fence == Some(marker) {
                None
            } else if fence.is_none() {
                Some(marker)
            } else {
                fence
            };
        }
        if fence.is_none() && line.starts_with('#') && line.trim_start_matches('#').starts_with(' ')
        {
            if !text.trim().is_empty() {
                sections.push(DocumentSection {
                    id: format!("s{:03}", sections.len() + 1),
                    title,
                    body: text.trim().into(),
                });
            }
            title = line.trim_start_matches('#').trim().into();
            text = String::new();
        }
        text.push_str(line);
        text.push('\n');
    }
    if !text.trim().is_empty() {
        sections.push(DocumentSection {
            id: format!("s{:03}", sections.len() + 1),
            title,
            body: text.trim().into(),
        });
    }
    sections
}

pub fn html_to_markdown(html: &str, origin: &str) -> (Option<String>, String) {
    let document = Html::parse_document(html);
    let title_selector = Selector::parse("title").expect("valid selector");
    let title = document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_owned());
    let root = ["main", "article", "body"]
        .iter()
        .find_map(|selector| {
            document
                .select(&Selector::parse(selector).expect("valid selector"))
                .next()
        })
        .unwrap_or_else(|| document.root_element());
    let mut text = String::new();
    append_element(root, origin, &mut text);
    (title, text)
}

fn append_element(element: ElementRef<'_>, origin: &str, output: &mut String) {
    let name = element.value().name();
    if matches!(name, "script" | "style" | "noscript" | "svg" | "template")
        || element.value().attr("hidden").is_some()
    {
        return;
    }
    if name == "table" {
        let rows = Selector::parse("tr").expect("valid selector");
        let cells = Selector::parse("th,td").expect("valid selector");
        output.push_str("\n\n");
        for (index, row) in element.select(&rows).enumerate() {
            let values: Vec<_> = row
                .select(&cells)
                .map(|cell| {
                    cell.text()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace('|', "\\|")
                })
                .collect();
            if values.is_empty() {
                continue;
            }
            output.push_str(&format!("| {} |\n", values.join(" | ")));
            if index == 0 {
                output.push_str(&format!("| {} |\n", vec!["---"; values.len()].join(" | ")));
            }
        }
        output.push('\n');
        return;
    }
    let block = matches!(
        name,
        "p" | "div" | "section" | "article" | "main" | "ul" | "ol" | "blockquote" | "pre"
    );
    if block {
        output.push_str("\n\n");
    }
    if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        output.push_str(&format!(
            "\n\n{} ",
            "#".repeat(name.as_bytes()[1] as usize - b'0' as usize)
        ));
    }
    if name == "li" {
        output.push_str("\n- ");
    }
    if name == "br" {
        output.push('\n');
    }
    for child in element.children() {
        match child.value() {
            Node::Text(text) => output.push_str(text),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    append_element(child, origin, output);
                }
            }
            _ => (),
        }
    }
    if name == "a"
        && let Some(href) = element.value().attr("href")
    {
        let url = reqwest::Url::parse(origin)
            .ok()
            .and_then(|base| base.join(href).ok())
            .map(|url| url.to_string())
            .unwrap_or_else(|| href.to_owned());
        if url.starts_with("http://") || url.starts_with("https://") {
            output.push_str(&format!(" ({url})"));
        }
    }
    if block || matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        output.push_str("\n\n");
    }
}

pub fn demo_library() -> Library {
    let mut document = Document::from_text(
        "demo://earnings/FY2026Q2",
        "DEMO · FY2026 Q2 Earnings Release",
        "# Quarterly results\n\nDEMO: fictional company and financial figures.\n\n## Revenue\n\nRevenue was $12.8 billion. Growth came from enterprise services.\n\n| Segment | Revenue |\n| --- | --- |\n| Enterprise | 8.4 billion |\n| Consumer | 4.4 billion |\n\n## Cash flow\n\nOperating cash flow was $3.4 billion; capital expenditures were $1.1 billion.\n\n## Outlook\n\nManagement expects investment spending to remain elevated. These examples are not live disclosures.",
    );
    document.symbol = Some("DEMO.US".into());
    document.fiscal_period = Some("FY2026Q2".into());
    document
        .warnings
        .push("Simulated document; no company disclosure was fetched".into());
    Arc::new(RwLock::new(vec![document]))
}
