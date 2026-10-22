//! Code colours in the classes of Claude Code's cli-highlight default theme. With the
//! `code-highlight` feature the grammar comes from syntect's bundled Sublime syntaxes and each
//! token is classified by its innermost scope that a rule names; hosts map the classes onto the
//! terminal's own palette colours. Without the feature code keeps the text colour.

use std::sync::Arc;

/// The cli-highlight classes Claude Code colours. Everything else keeps the text colour.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CodeToken {
    Keyword,
    BuiltIn,
    Type,
    Number,
    String,
    Function,
    Comment,
    Meta,
}

/// One source line as runs of text and the class each belongs to.
pub type CodeLine = Vec<(String, Option<CodeToken>)>;

/// `code` of a fenced block whose info string is `info`, or `None` when its language is unknown.
pub fn highlight_fenced(info: &str, code: &str) -> Option<Arc<Vec<CodeLine>>> {
    #[cfg(feature = "code-highlight")]
    return engine::highlight_fenced(info, code);
    #[cfg(not(feature = "code-highlight"))]
    {
        let _ = (info, code);
        None
    }
}

/// `code` of the file at `path`, chosen by its extension, or `None` when it is unknown.
pub fn highlight_path(path: &str, code: &str) -> Option<Arc<Vec<CodeLine>>> {
    #[cfg(feature = "code-highlight")]
    return engine::highlight_path(path, code);
    #[cfg(not(feature = "code-highlight"))]
    {
        let _ = (path, code);
        None
    }
}

#[cfg(feature = "code-highlight")]
mod engine {
    use std::sync::{Arc, LazyLock, Mutex, PoisonError};

    use lru::LruCache;

    use syntect::{
        easy::ScopeRangeIterator,
        parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet},
        util::LinesWithEndings,
    };

    use super::{CodeLine, CodeToken};

    /// Source bytes of the highlighted blocks kept for reflows, which lay the whole conversation
    /// out again for each width: enough for a long conversation, so a resize never highlights again.
    const CACHED_SOURCE_BYTES: usize = 16 * 1024 * 1024;

    static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

    /// Scope prefixes and the class they give, tried innermost scope first. A rule without a class
    /// keeps the text colour even inside a coloured scope, as operators do.
    static RULES: LazyLock<Vec<(Scope, Option<CodeToken>)>> = LazyLock::new(|| {
        [
            ("comment", Some(CodeToken::Comment)),
            ("string", Some(CodeToken::String)),
            ("constant.character.escape", Some(CodeToken::String)),
            ("constant.numeric", Some(CodeToken::Number)),
            ("constant.language", Some(CodeToken::Keyword)),
            ("keyword.operator", None),
            ("keyword", Some(CodeToken::Keyword)),
            ("storage.type", Some(CodeToken::Keyword)),
            ("storage.modifier", Some(CodeToken::Keyword)),
            ("entity.name.function", Some(CodeToken::Function)),
            ("entity.name.tag", Some(CodeToken::Keyword)),
            ("entity.other.attribute-name", Some(CodeToken::BuiltIn)),
            ("entity.name.type", Some(CodeToken::Type)),
            ("entity.name.class", Some(CodeToken::Type)),
            ("support.function", Some(CodeToken::BuiltIn)),
            ("support.type", Some(CodeToken::BuiltIn)),
            ("support.class", Some(CodeToken::BuiltIn)),
            ("variable.language", Some(CodeToken::BuiltIn)),
            ("meta.annotation", Some(CodeToken::Meta)),
            ("meta.attribute", Some(CodeToken::Meta)),
            ("meta.preprocessor", Some(CodeToken::Meta)),
            ("markup.inserted", Some(CodeToken::Number)),
            ("markup.deleted", Some(CodeToken::String)),
        ]
        .into_iter()
        .map(|(prefix, token)| {
            (
                Scope::new(prefix).expect("rule scopes are valid scope names"),
                token,
            )
        })
        .collect()
    });

    /// Highlighted blocks by syntax name and source, least recently used first out.
    struct Cache {
        blocks: LruCache<(String, String), Arc<Vec<CodeLine>>>,
        source_bytes: usize,
    }

    impl Cache {
        fn insert(&mut self, key: (String, String), lines: Arc<Vec<CodeLine>>) {
            self.source_bytes += key.1.len();
            if let Some(((_, replaced), _)) = self.blocks.push(key, lines) {
                self.source_bytes -= replaced.len();
            }
            while self.source_bytes > CACHED_SOURCE_BYTES
                && let Some(((_, source), _)) = self.blocks.pop_lru()
            {
                self.source_bytes -= source.len();
            }
        }
    }

    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
        Mutex::new(Cache {
            blocks: LruCache::unbounded(),
            source_bytes: 0,
        })
    });

    pub(super) fn highlight_fenced(info: &str, code: &str) -> Option<Arc<Vec<CodeLine>>> {
        let language = info.split_whitespace().next()?;
        let syntax = SYNTAXES
            .find_syntax_by_token(language)
            .or_else(|| SYNTAXES.find_syntax_by_extension(language))?;
        highlight(syntax, code)
    }

    pub(super) fn highlight_path(path: &str, code: &str) -> Option<Arc<Vec<CodeLine>>> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = name
            .rsplit_once('.')
            .map_or(name, |(_, extension)| extension);
        let syntax = SYNTAXES.find_syntax_by_extension(extension)?;
        highlight(syntax, code)
    }

    fn highlight(syntax: &SyntaxReference, code: &str) -> Option<Arc<Vec<CodeLine>>> {
        let key = (syntax.name.clone(), code.to_owned());
        if let Some(lines) = CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .blocks
            .get(&key)
        {
            return Some(lines.clone());
        }
        let mut parser = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut lines = Vec::new();
        for source in LinesWithEndings::from(code) {
            let operations = parser.parse_line(source, &SYNTAXES).ok()?;
            let mut line = CodeLine::new();
            for (range, operation) in ScopeRangeIterator::new(&operations, source) {
                stack.apply(operation).ok()?;
                let text = source[range].trim_end_matches(['\n', '\r']);
                if text.is_empty() {
                    continue;
                }
                let token = classify(&stack);
                match line.last_mut() {
                    Some((last, last_token)) if *last_token == token => last.push_str(text),
                    _ => line.push((text.to_owned(), token)),
                }
            }
            lines.push(line);
        }
        let lines = Arc::new(lines);
        CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, lines.clone());
        Some(lines)
    }

    fn classify(stack: &ScopeStack) -> Option<CodeToken> {
        stack.as_slice().iter().rev().find_map(|scope| {
            RULES
                .iter()
                .find(|(prefix, _)| prefix.is_prefix_of(*scope))
                .map(|(_, token)| *token)
        })?
    }
}

#[cfg(all(test, feature = "code-highlight"))]
mod tests {
    use super::*;

    #[test]
    fn a_long_conversation_stays_highlighted_across_reflows() {
        let sources = (0..500)
            .map(|index| format!("fn cached_{index}() {{}}\n"))
            .collect::<Vec<_>>();
        let first = sources
            .iter()
            .map(|source| highlight_fenced("rust", source).unwrap())
            .collect::<Vec<_>>();
        for (source, highlighted) in sources.iter().zip(&first) {
            assert!(Arc::ptr_eq(
                &highlight_fenced("rust", source).unwrap(),
                highlighted
            ));
        }
    }

    fn tokens(lines: &[CodeLine]) -> Vec<(String, Option<CodeToken>)> {
        lines
            .iter()
            .flatten()
            .filter(|(text, _)| !text.trim().is_empty())
            .map(|(text, token)| (text.trim().to_owned(), *token))
            .collect()
    }

    #[test]
    fn rust_keywords_strings_numbers_and_comments_get_their_classes() {
        let lines = highlight_fenced(
            "rust",
            "// note\nfn main() {\n    let count = 42;\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        assert_eq!(lines.len(), 5);
        let found = tokens(&lines);
        for expected in [
            ("// note".to_owned(), Some(CodeToken::Comment)),
            ("fn".to_owned(), Some(CodeToken::Keyword)),
            ("main".to_owned(), Some(CodeToken::Function)),
            ("let".to_owned(), Some(CodeToken::Keyword)),
            ("42".to_owned(), Some(CodeToken::Number)),
        ] {
            assert!(found.contains(&expected), "{expected:?} in {found:?}");
        }
        assert!(
            found
                .iter()
                .any(|(text, token)| text.contains("hi") && *token == Some(CodeToken::String)),
            "{found:?}"
        );
    }

    #[test]
    fn python_definitions_and_unknown_languages() {
        let lines = highlight_fenced("python", "def area(r):\n    return 3.14 * r\n").unwrap();
        let found = tokens(&lines);
        assert!(
            found.contains(&("def".into(), Some(CodeToken::Keyword))),
            "{found:?}"
        );
        assert!(
            found.contains(&("area".into(), Some(CodeToken::Function))),
            "{found:?}"
        );
        assert!(
            found.contains(&("3.14".into(), Some(CodeToken::Number))),
            "{found:?}"
        );
        assert!(highlight_fenced("no-such-language", "x").is_none());
        assert!(highlight_fenced("", "x").is_none());
    }

    #[test]
    fn a_path_picks_the_language_by_extension_and_lines_keep_their_text() {
        let code = "x = 1\n\ny = 'two'";
        let lines = highlight_path("/tmp/example.py", code).unwrap();
        let text = lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(text, ["x = 1", "", "y = 'two'"]);
        assert!(highlight_path("/tmp/README", code).is_none());
    }
}
