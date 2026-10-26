//! Discovery of the instruction files a conversation loads, following Claude Code's memory files
//! with AGENTS.md first.
//!
//! The user's own instructions are `AGENTS.md` in the configuration root; without that file they
//! are Claude Code's `~/.claude/CLAUDE.md` and `~/.claude/rules`. Then each directory from the
//! filesystem root down to the Folder contributes, so the files nearest the Folder come last and
//! weigh most: its `AGENTS.md`, or when it has none, its `CLAUDE.md`, `.claude/CLAUDE.md` and
//! unconditional `.claude/rules`; then its `CLAUDE.local.md`. Each file is loaded once, `@path`
//! imports follow the file importing them, and a project file imports nothing outside the Folder.

use regex::Regex;
use serde_yaml_ng::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};
use tickloop_harness::policy::instructions::{InstructionFile, InstructionScope};

const AGENTS_FILE_NAME: &str = "AGENTS.md";
const CLAUDE_FILE_NAME: &str = "CLAUDE.md";
const CLAUDE_LOCAL_FILE_NAME: &str = "CLAUDE.local.md";
const CLAUDE_DIRECTORY: &str = ".claude";
const RULES_DIRECTORY: &str = "rules";
const GIT_ENTRY: &str = ".git";
const MARKDOWN_EXTENSION: &str = "md";
const MAX_IMPORT_DEPTH: usize = 5;
/// Claude Code's `TEXT_FILE_EXTENSIONS`: a file with any other extension is never loaded.
const TEXT_FILE_EXTENSIONS: &[&str] = &[
    ".md",
    ".txt",
    ".text",
    ".json",
    ".yaml",
    ".yml",
    ".toml",
    ".xml",
    ".csv",
    ".html",
    ".htm",
    ".css",
    ".scss",
    ".sass",
    ".less",
    ".js",
    ".ts",
    ".tsx",
    ".jsx",
    ".mjs",
    ".cjs",
    ".mts",
    ".cts",
    ".py",
    ".pyi",
    ".pyw",
    ".rb",
    ".erb",
    ".rake",
    ".go",
    ".rs",
    ".java",
    ".kt",
    ".kts",
    ".scala",
    ".c",
    ".cpp",
    ".cc",
    ".cxx",
    ".h",
    ".hpp",
    ".hxx",
    ".cs",
    ".swift",
    ".sh",
    ".bash",
    ".zsh",
    ".fish",
    ".ps1",
    ".bat",
    ".cmd",
    ".env",
    ".ini",
    ".cfg",
    ".conf",
    ".config",
    ".properties",
    ".sql",
    ".graphql",
    ".gql",
    ".proto",
    ".vue",
    ".svelte",
    ".astro",
    ".ejs",
    ".hbs",
    ".pug",
    ".jade",
    ".php",
    ".pl",
    ".pm",
    ".lua",
    ".r",
    ".dart",
    ".ex",
    ".exs",
    ".erl",
    ".hrl",
    ".clj",
    ".cljs",
    ".cljc",
    ".edn",
    ".hs",
    ".lhs",
    ".elm",
    ".ml",
    ".mli",
    ".f",
    ".f90",
    ".f95",
    ".for",
    ".cmake",
    ".make",
    ".makefile",
    ".gradle",
    ".sbt",
    ".rst",
    ".adoc",
    ".asciidoc",
    ".org",
    ".tex",
    ".latex",
    ".lock",
    ".log",
    ".diff",
    ".patch",
];

static FRONTMATTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---\s*\n([\s\S]*?)---\s*\n?").unwrap());
static SIMPLE_FRONTMATTER_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z_-]+):\s+(.+)$").unwrap());
static YAML_SPECIAL_CHARACTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[{}\[\]*&#!|>%@`]|: ").unwrap());
static BRACE_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^{]*)\{([^}]+)\}(.*)$").unwrap());
static IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)@((?:[^\s\\]|\\ )+)").unwrap());
static IMPORT_SYMBOL_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[#%^&*()]+").unwrap());
static IMPORT_PLAIN_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9._-]").unwrap());
static COMMENT_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<!--[\s\S]*?-->").unwrap());
static CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`+[^`]*`+").unwrap());

/// The instruction files for a conversation in `folder`, lowest priority first. `config_root` is
/// the Tickloop configuration root and `home` the user's home directory.
pub fn instruction_files(
    folder: Option<&Path>,
    config_root: Option<&Path>,
    home: Option<&Path>,
) -> Vec<InstructionFile> {
    let folder = folder.map(resolve);
    let home = home.map(resolve);
    let mut discovery = Discovery {
        folder: folder.as_deref(),
        home: home.as_deref(),
        processed: HashSet::new(),
        files: Vec::new(),
    };
    discovery.load_user_files(config_root);
    if let Some(folder) = &folder {
        discovery.load_directory_files(folder);
    }
    discovery.files
}

struct Discovery<'a> {
    folder: Option<&'a Path>,
    home: Option<&'a Path>,
    processed: HashSet<PathBuf>,
    files: Vec<InstructionFile>,
}

struct LoadedFile {
    file: InstructionFile,
    is_conditional: bool,
}

impl Discovery<'_> {
    fn load_user_files(&mut self, config_root: Option<&Path>) {
        let agents = config_root.map(|root| root.join(AGENTS_FILE_NAME));
        if let Some(agents) = agents.filter(|agents| agents.is_file()) {
            self.load(&agents, InstructionScope::User);
            return;
        }
        if let Some(claude) = self.home.map(|home| home.join(CLAUDE_DIRECTORY)) {
            self.load(&claude.join(CLAUDE_FILE_NAME), InstructionScope::User);
            self.load_rules(&claude.join(RULES_DIRECTORY), InstructionScope::User);
        }
    }

    fn load_directory_files(&mut self, folder: &Path) {
        let mut directories = folder
            .ancestors()
            .filter(|directory| directory.parent().is_some())
            .collect::<Vec<_>>();
        directories.reverse();
        let nested_worktree = nested_worktree(folder);
        for directory in directories {
            let skips_checked_in_files = nested_worktree.as_ref().is_some_and(|worktree| {
                directory.starts_with(&worktree.main_root)
                    && !directory.starts_with(&worktree.worktree_root)
            });
            if !skips_checked_in_files {
                self.load_checked_in_files(directory);
            }
            self.load(
                &directory.join(CLAUDE_LOCAL_FILE_NAME),
                InstructionScope::Local,
            );
        }
    }

    fn load_checked_in_files(&mut self, directory: &Path) {
        let agents = directory.join(AGENTS_FILE_NAME);
        if agents.is_file() {
            self.load(&agents, InstructionScope::Project);
            return;
        }
        self.load(&directory.join(CLAUDE_FILE_NAME), InstructionScope::Project);
        // The home directory's `.claude` holds the user's own instructions, not a project's.
        if self.home == Some(directory) {
            return;
        }
        let claude = directory.join(CLAUDE_DIRECTORY);
        self.load(&claude.join(CLAUDE_FILE_NAME), InstructionScope::Project);
        self.load_rules(&claude.join(RULES_DIRECTORY), InstructionScope::Project);
    }

    fn load(&mut self, path: &Path, scope: InstructionScope) {
        let loaded = self.load_with_imports(path, scope, 0);
        self.files
            .extend(loaded.into_iter().map(|loaded| loaded.file));
    }

    /// Rules in `directory` and its subdirectories without `paths` frontmatter; a rule limited to
    /// some paths applies only when a matching file is read.
    fn load_rules(&mut self, directory: &Path, scope: InstructionScope) {
        let mut visited = HashSet::new();
        self.load_rules_in(directory, scope, &mut visited);
    }

    fn load_rules_in(
        &mut self,
        directory: &Path,
        scope: InstructionScope,
        visited: &mut HashSet<PathBuf>,
    ) {
        let resolved = resolve(directory);
        if visited.contains(directory) || visited.contains(&resolved) {
            return;
        }
        visited.insert(directory.to_path_buf());
        visited.insert(resolved.clone());
        let Ok(entries) = fs::read_dir(&resolved) else {
            return;
        };
        let mut names = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            let entry = resolve(&directory.join(&name));
            let Ok(metadata) = fs::metadata(&entry) else {
                continue;
            };
            if metadata.is_dir() {
                self.load_rules_in(&entry, scope, visited);
            } else if metadata.is_file()
                && Path::new(&name)
                    .extension()
                    .is_some_and(|extension| extension == MARKDOWN_EXTENSION)
            {
                let loaded = self.load_with_imports(&entry, scope, 0);
                self.files.extend(
                    loaded
                        .into_iter()
                        .filter(|loaded| !loaded.is_conditional)
                        .map(|loaded| loaded.file),
                );
            }
        }
    }

    /// The file followed by the files it imports, each loaded once.
    fn load_with_imports(
        &mut self,
        path: &Path,
        scope: InstructionScope,
        depth: usize,
    ) -> Vec<LoadedFile> {
        let resolved = resolve(path);
        if depth >= MAX_IMPORT_DEPTH
            || self.processed.contains(path)
            || self.processed.contains(&resolved)
        {
            return Vec::new();
        }
        self.processed.insert(path.to_path_buf());
        self.processed.insert(resolved.clone());
        let Some(parsed) = read_instruction_file(path) else {
            return Vec::new();
        };
        if parsed.content.trim().is_empty() {
            return Vec::new();
        }
        let base_directory = resolved.parent().unwrap_or(Path::new("/"));
        let imports = import_paths(&parsed.content, base_directory, self.home);
        let mut loaded = vec![LoadedFile {
            file: InstructionFile {
                path: path.to_path_buf(),
                scope,
                content: parsed.content,
            },
            is_conditional: parsed.is_conditional,
        }];
        for import in imports {
            if scope != InstructionScope::User && !self.is_inside_folder(&import) {
                continue;
            }
            loaded.extend(self.load_with_imports(&import, scope, depth + 1));
        }
        loaded
    }

    /// Whether `path` is inside the Folder once links are followed, so a link checked into the
    /// project cannot bring in a file from elsewhere.
    fn is_inside_folder(&self, path: &Path) -> bool {
        self.folder
            .is_some_and(|folder| resolve(path).starts_with(folder))
    }
}

struct ParsedFile {
    content: String,
    is_conditional: bool,
}

/// A text file's content without frontmatter or block-level HTML comments, or `None` when it is
/// absent, not a regular file or not a text file.
fn read_instruction_file(path: &Path) -> Option<ParsedFile> {
    if let Some(extension) = path.extension() {
        let extension = format!(".{}", extension.to_string_lossy().to_lowercase());
        if !TEXT_FILE_EXTENSIONS.contains(&extension.as_str()) {
            return None;
        }
    }
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let raw = String::from_utf8_lossy(&fs::read(path).ok()?).into_owned();
    let (body, paths) = split_frontmatter(&raw);
    Some(ParsedFile {
        content: strip_block_comments(body),
        is_conditional: paths.is_some(),
    })
}

/// The content after frontmatter and its `paths` patterns; `None` when the file applies to every
/// path.
fn split_frontmatter(raw: &str) -> (&str, Option<Vec<String>>) {
    let Some(captures) = FRONTMATTER.captures(raw) else {
        return (raw, None);
    };
    let body = &raw[captures.get(0).unwrap().end()..];
    let frontmatter = captures.get(1).map_or("", |text| text.as_str());
    let paths = parse_yaml_mapping(frontmatter)
        .or_else(|| parse_yaml_mapping(&quote_special_values(frontmatter)))
        .and_then(|mapping| mapping.get("paths").cloned())
        .map(|paths| {
            split_path_patterns(&paths)
                .into_iter()
                .map(|pattern| pattern.strip_suffix("/**").unwrap_or(&pattern).to_owned())
                .filter(|pattern| !pattern.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|patterns| !patterns.is_empty() && patterns.iter().any(|pattern| pattern != "**"));
    (body, paths)
}

fn parse_yaml_mapping(text: &str) -> Option<serde_yaml_ng::Mapping> {
    match serde_yaml_ng::from_str::<Value>(text).ok()? {
        Value::Mapping(mapping) => Some(mapping),
        _ => None,
    }
}

/// Quotes unquoted top-level values containing YAML syntax, so `paths: src/**/*.ts` parses.
fn quote_special_values(frontmatter: &str) -> String {
    frontmatter
        .split('\n')
        .map(|line| {
            let Some(captures) = SIMPLE_FRONTMATTER_ENTRY.captures(line) else {
                return line.to_owned();
            };
            let (key, value) = (&captures[1], &captures[2]);
            let is_quoted = (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''));
            if is_quoted || !YAML_SPECIAL_CHARACTERS.is_match(value) {
                return line.to_owned();
            }
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{key}: \"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits comma-separated patterns outside braces and expands brace alternatives.
fn split_path_patterns(value: &Value) -> Vec<String> {
    match value {
        Value::Sequence(items) => items.iter().flat_map(split_path_patterns).collect(),
        Value::String(text) => {
            let mut parts = Vec::new();
            let mut current = String::new();
            let mut brace_depth = 0_i32;
            for character in text.chars() {
                match character {
                    '{' => brace_depth += 1,
                    '}' => brace_depth -= 1,
                    ',' if brace_depth == 0 => {
                        parts.push(std::mem::take(&mut current));
                        continue;
                    }
                    _ => {}
                }
                current.push(character);
            }
            parts.push(current);
            parts
                .iter()
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .flat_map(expand_braces)
                .collect()
        }
        _ => Vec::new(),
    }
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(captures) = BRACE_GROUP.captures(pattern) else {
        return vec![pattern.to_owned()];
    };
    captures[2]
        .split(',')
        .flat_map(|alternative| {
            expand_braces(&format!(
                "{}{}{}",
                &captures[1],
                alternative.trim(),
                &captures[3]
            ))
        })
        .collect()
}

/// Removes HTML comments that form a whole Markdown block, keeping any text after a comment on
/// its closing line; comments inside code and an unclosed comment stay.
fn strip_block_comments(content: &str) -> String {
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    let mut result = String::with_capacity(content.len());
    let mut fence = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(marker) = &fence {
            if closes_fence(line, marker) {
                fence = None;
            }
            result.push_str(line);
            index += 1;
            continue;
        }
        if let Some(marker) = opening_fence(line) {
            fence = Some(marker);
            result.push_str(line);
            index += 1;
            continue;
        }
        let closing = starts_comment_block(line)
            .then(|| (index..lines.len()).find(|&end| lines[end].contains("-->")))
            .flatten();
        let Some(closing) = closing else {
            result.push_str(line);
            index += 1;
            continue;
        };
        let block = lines[index..=closing].concat();
        let residue = COMMENT_SPAN.replace_all(&block, "");
        index = closing + 1;
        if !residue.trim().is_empty() {
            result.push_str(&residue);
            continue;
        }
        while index < lines.len() && lines[index].trim().is_empty() {
            index += 1;
        }
    }
    result
}

fn starts_comment_block(line: &str) -> bool {
    let indentation = line.len() - line.trim_start_matches(' ').len();
    indentation <= 3 && line.trim_start_matches(' ').starts_with("<!--")
}

/// The fence character and length of a line opening a fenced code block.
fn opening_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next().filter(|&c| c == '`' || c == '~')?;
    let length = trimmed.chars().take_while(|&c| c == marker).count();
    (length >= 3).then_some((marker, length))
}

fn closes_fence(line: &str, &(marker, length): &(char, usize)) -> bool {
    let trimmed = line.trim();
    trimmed.chars().take_while(|&c| c == marker).count() >= length
        && trimmed.chars().all(|c| c == marker)
}

/// The paths imported with `@path` outside code and HTML comments, resolved against the
/// directory of the importing file.
fn import_paths(content: &str, base_directory: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut imports = Vec::new();
    let mut fence = None;
    for line in content.lines() {
        if let Some(marker) = &fence {
            if closes_fence(line, marker) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = opening_fence(line) {
            fence = Some(marker);
            continue;
        }
        let without_comments = COMMENT_SPAN.replace_all(line, "");
        let text = CODE_SPAN.replace_all(&without_comments, "");
        for captures in IMPORT.captures_iter(&text) {
            let reference = &captures[1];
            let reference = reference
                .split_once('#')
                .map_or(reference, |(path, _)| path)
                .replace("\\ ", " ");
            if let Some(path) = resolve_import(&reference, base_directory, home)
                && !imports.contains(&path)
            {
                imports.push(path);
            }
        }
    }
    imports
}

fn resolve_import(reference: &str, base_directory: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let is_valid = reference.starts_with("./")
        || reference.starts_with("~/")
        || (reference.starts_with('/') && reference != "/")
        || (!reference.starts_with('@')
            && !IMPORT_SYMBOL_PREFIX.is_match(reference)
            && IMPORT_PLAIN_START.is_match(reference));
    if !is_valid {
        return None;
    }
    let path = match reference.strip_prefix("~/") {
        Some(relative) => home?.join(relative),
        None => base_directory.join(reference),
    };
    Some(normalize(&path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component),
        }
    }
    normalized
}

fn resolve(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct NestedWorktree {
    worktree_root: PathBuf,
    main_root: PathBuf,
}

/// A git worktree checked out inside its main repository, whose checked-in files the walk up
/// would otherwise load twice.
fn nested_worktree(folder: &Path) -> Option<NestedWorktree> {
    let worktree_root = folder
        .ancestors()
        .find(|directory| directory.join(GIT_ENTRY).exists())?
        .to_path_buf();
    let main_root = main_repository_root(&worktree_root)?;
    (main_root != worktree_root && worktree_root.starts_with(&main_root)).then_some(
        NestedWorktree {
            worktree_root,
            main_root,
        },
    )
}

/// The main working directory of the worktree checked out at `worktree_root`, verified the way
/// `git worktree add` links them so a cloned repository cannot point elsewhere.
fn main_repository_root(worktree_root: &Path) -> Option<PathBuf> {
    let pointer = fs::read_to_string(worktree_root.join(GIT_ENTRY)).ok()?;
    let worktree_git_directory =
        normalize(&worktree_root.join(pointer.trim().strip_prefix("gitdir:")?.trim()));
    let common_directory = normalize(
        &worktree_git_directory.join(
            fs::read_to_string(worktree_git_directory.join("commondir"))
                .ok()?
                .trim(),
        ),
    );
    if worktree_git_directory.parent()? != common_directory.join("worktrees") {
        return None;
    }
    let backlink = fs::canonicalize(
        fs::read_to_string(worktree_git_directory.join("gitdir"))
            .ok()?
            .trim(),
    )
    .ok()?;
    if backlink != fs::canonicalize(worktree_root).ok()?.join(GIT_ENTRY) {
        return None;
    }
    if common_directory.file_name()? != GIT_ENTRY {
        return Some(common_directory);
    }
    common_directory.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_comments_are_removed_with_the_blank_lines_after_them() {
        assert_eq!(
            strip_block_comments("A\n\n<!-- note\nmore -->\n\nB\n"),
            "A\n\nB\n"
        );
        assert_eq!(
            strip_block_comments("<!-- note --> Use bun\nB"),
            " Use bun\nB"
        );
        assert_eq!(
            strip_block_comments("```\n<!-- kept -->\n```\n"),
            "```\n<!-- kept -->\n```\n"
        );
        assert_eq!(strip_block_comments("<!-- unclosed\nB"), "<!-- unclosed\nB");
        assert_eq!(
            strip_block_comments("Text <!-- inline --> stays"),
            "Text <!-- inline --> stays"
        );
    }

    #[test]
    fn imports_skip_code_comments_and_non_paths() {
        let base = Path::new("/repo/docs");
        let home = Some(Path::new("/home/u"));
        let content = "See @guide.md#setup and @./a.md, @~/notes.md, @/abs/b.md.\n\
             Mail me@example.com, @@x, @#tag, @/ and `@code.md`.\n\
             <!-- @hidden.md --> @after.md\n\
             ```\n@fenced.md\n```\n\
             @my\\ file.md @../up.md";
        assert_eq!(
            import_paths(content, base, home),
            [
                "/repo/docs/guide.md",
                "/repo/docs/a.md,",
                "/home/u/notes.md,",
                "/abs/b.md.",
                "/repo/docs/after.md",
                "/repo/docs/my file.md",
                "/repo/up.md",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn paths_frontmatter_makes_a_rule_conditional() {
        let (body, paths) = split_frontmatter("---\npaths: src/**/*.{ts,tsx}, lib/**\n---\nBody");
        assert_eq!(body, "Body");
        assert_eq!(
            paths.unwrap(),
            ["src/**/*.ts", "src/**/*.tsx", "lib"].map(String::from)
        );
        for unconditional in [
            "---\npaths: \"**\"\n---\nBody",
            "---\ndescription: x\n---\nBody",
            "Body",
        ] {
            assert_eq!(split_frontmatter(unconditional), ("Body", None));
        }
        let (_, listed) = split_frontmatter("---\npaths:\n  - \"a/*.md\"\n  - b\n---\n");
        assert_eq!(listed.unwrap(), ["a/*.md", "b"].map(String::from));
    }
}
