//! Port of Claude Code's shell path constraints: `checkPathConstraints` and the per-command
//! path extractors (`tools/BashTool/pathValidation.ts`), `validatePath`, `isPathAllowed` and
//! `isDangerousRemovalPath` (`utils/permissions/pathValidation.ts`), and the working-directory
//! and auto-edit safety checks from `utils/permissions/filesystem.ts`. Workspace roots stand in
//! for Claude Code's working directories; permission rules, sandbox allowlists and Windows
//! path forms are outside this module.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;

use super::{security::strip_safe_wrappers, sed::sed_command_is_allowed, syntax::quote_arguments};

const DANGEROUS_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
];
/// Claude Code protects its own `.claude` directory; Tick Loop's counterpart
/// is `.tickloop`, which holds the Agent's settings and permission rules.
const DANGEROUS_DIRECTORIES: &[&str] = &[".git", ".vscode", ".idea", ".tickloop"];
const MAX_SYMLINK_DEPTH: usize = 40;

static SHORT_NAME: LazyLock<Regex> = LazyLock::new(|| pattern(r"~[0-9]"));
static TRAILING_DOTS_OR_SPACES: LazyLock<Regex> = LazyLock::new(|| pattern(r"[.\s]+$"));
static DOS_DEVICE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)\.(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$"));
static THREE_DOT_COMPONENT: LazyLock<Regex> = LazyLock::new(|| pattern(r"(^|/|\\)\.{3,}(/|\\|$)"));
static PATH_TRAVERSAL: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?:^|[\\/])\.\.(?:[\\/]|$)"));
static NEWER_FLAG: LazyLock<Regex> = LazyLock::new(|| pattern(r"^-newer[acmBt][acmtB]$"));

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("static path validation pattern")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileOperation {
    Read,
    Write,
    Create,
}

pub(super) struct PathPolicy<'a> {
    pub(super) cwd: &'a Path,
    pub(super) working_directories: &'a [PathBuf],
    pub(super) readable_directories: &'a [PathBuf],
    pub(super) editable_directory: Option<&'a Path>,
    pub(super) accept_edits: bool,
}

impl PathPolicy<'_> {
    /// Port of `validateSinglePathCommand` followed by the rm/rmdir dangerous-path gate;
    /// `true` means Claude Code returns `passthrough`.
    pub(super) fn command_paths_allowed(&self, command: &str, compound_has_cd: bool) -> bool {
        let stripped = strip_safe_wrappers(command);
        let arguments = quote_arguments(&stripped).unwrap_or_default();
        let Some((base_command, arguments)) = arguments.split_first() else {
            return true;
        };
        let Some(default_operation) = operation_type(base_command) else {
            return true;
        };
        let file_flags: &[&str] = match base_command.as_str() {
            "file" => &["-m", "--magic-file"],
            "base64" => &["-i", "--input"],
            "sort" => &["--random-source"],
            "rg" => &["--ignore-file"],
            _ => &[],
        };
        for (index, argument) in arguments.iter().enumerate() {
            let (flag, inline) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(name, value)| {
                    (name, Some(value))
                });
            if file_flags.contains(&flag) {
                let value = inline.or_else(|| arguments.get(index + 1).map(String::as_str));
                if value.is_none_or(|path| !self.validate_path(path, FileOperation::Read)) {
                    return false;
                }
            }
        }
        if base_command == "diff" {
            let operands = extract_paths(base_command, arguments);
            if operands.iter().any(|operand| {
                self.cwd
                    .join(expand_tilde(strip_surrounding_quote(operand)))
                    .is_dir()
            }) {
                return false;
            }
            for flag in arguments
                .iter()
                .filter(|argument| argument.starts_with('-'))
            {
                if let Some((name, path)) = flag.split_once('=')
                    && matches!(name, "--from-file" | "--to-file" | "--exclude-from")
                    && (!self.validate_path(path, FileOperation::Read)
                        || self
                            .cwd
                            .join(expand_tilde(strip_surrounding_quote(path)))
                            .is_dir())
                {
                    return false;
                }
            }
        }
        let operation = if base_command == "sed" && sed_command_is_allowed(&stripped, false) {
            FileOperation::Read
        } else {
            default_operation
        };
        if matches!(base_command.as_str(), "mv" | "cp")
            && arguments.iter().any(|argument| argument.starts_with('-'))
        {
            return false;
        }
        if compound_has_cd && operation != FileOperation::Read {
            return false;
        }
        let paths = extract_paths(base_command, arguments);
        if !paths.iter().all(|path| self.validate_path(path, operation)) {
            return false;
        }
        !(matches!(base_command.as_str(), "rm" | "rmdir")
            && paths
                .iter()
                .any(|path| is_dangerous_removal_path(&self.removal_target(path))))
    }

    /// Port of `validateOutputRedirections`.
    pub(super) fn output_redirections_allowed(
        &self,
        targets: &[&str],
        compound_has_cd: bool,
    ) -> bool {
        if compound_has_cd && !targets.is_empty() {
            return false;
        }
        targets
            .iter()
            .filter(|target| **target != "/dev/null")
            .all(|target| self.validate_path(target, FileOperation::Create))
    }

    fn removal_target(&self, path: &str) -> String {
        let clean = expand_tilde(strip_surrounding_quote(path));
        if clean.starts_with('/') {
            clean
        } else {
            normalize_lexically(&self.cwd.join(clean))
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Port of `validatePath`.
    fn validate_path(&self, path: &str, operation: FileOperation) -> bool {
        let clean = expand_tilde(strip_surrounding_quote(path));
        if clean.starts_with('~') || clean.contains(['$', '%']) || clean.starts_with('=') {
            return false;
        }
        let path_to_resolve = if let Some(glob_index) = clean.find(['*', '?', '[', ']', '{', '}']) {
            if operation != FileOperation::Read {
                return false;
            }
            if PATH_TRAVERSAL.is_match(&clean) {
                clean.clone()
            } else {
                glob_base_directory(&clean[..glob_index])
            }
        } else {
            clean
        };
        // Like `path.resolve`, only relative paths are normalized before symlink resolution.
        let absolute = if path_to_resolve.starts_with('/') {
            PathBuf::from(path_to_resolve)
        } else {
            normalize_lexically(&self.cwd.join(path_to_resolve))
        };
        let resolved = safe_resolve_path(&absolute);
        let resolved = if resolved == absolute {
            normalize_lexically(&resolved)
        } else {
            resolved
        };
        self.is_path_allowed(&resolved, operation)
    }

    /// Port of `isPathAllowed` without permission rules.
    fn is_path_allowed(&self, resolved: &Path, operation: FileOperation) -> bool {
        let paths_to_check = paths_for_permission_check(resolved);
        if self.editable_directory.is_some_and(|directory| {
            paths_to_check.iter().all(|path| {
                resolve_deepest_existing_ancestor(path)
                    .unwrap_or_else(|| path.clone())
                    .starts_with(directory)
            })
        }) {
            return true;
        }
        if operation != FileOperation::Read
            && paths_to_check.iter().any(|path| {
                has_suspicious_path_pattern(path) || is_dangerous_file_to_auto_edit(path)
            })
        {
            return false;
        }
        let working_paths = self
            .working_directories
            .iter()
            .flat_map(|directory| paths_for_permission_check(&normalize_lexically(directory)))
            .collect::<Vec<_>>();
        let is_in_working_directory = paths_to_check.iter().all(|path| {
            working_paths
                .iter()
                .any(|working_path| path_in_working_path(path, working_path))
        });
        if is_in_working_directory {
            return operation == FileOperation::Read || self.accept_edits;
        }
        operation == FileOperation::Read
            && self
                .readable_directories
                .iter()
                .any(|directory| resolved.starts_with(directory))
    }
}

fn operation_type(command: &str) -> Option<FileOperation> {
    Some(match command {
        "mkdir" | "touch" => FileOperation::Create,
        "rm" | "rmdir" | "mv" | "cp" | "sed" => FileOperation::Write,
        "cd" | "ls" | "find" | "cat" | "head" | "tail" | "sort" | "uniq" | "wc" | "cut"
        | "paste" | "column" | "tr" | "file" | "stat" | "diff" | "awk" | "strings" | "hexdump"
        | "od" | "base64" | "nl" | "grep" | "rg" | "git" | "jq" | "sha256sum" | "sha1sum"
        | "md5sum" => FileOperation::Read,
        _ => return None,
    })
}

fn home_directory() -> Option<PathBuf> {
    std::env::home_dir()
}

fn strip_surrounding_quote(path: &str) -> &str {
    let path = path.strip_prefix(['\'', '"']).unwrap_or(path);
    path.strip_suffix(['\'', '"']).unwrap_or(path)
}

fn expand_tilde(path: &str) -> String {
    match home_directory() {
        Some(home) if path == "~" || path.starts_with("~/") => {
            format!("{}{}", home.to_string_lossy(), &path[1..])
        }
        _ => path.to_owned(),
    }
}

/// Port of `getGlobBaseDirectory` given the text before the first glob character.
fn glob_base_directory(before_glob: &str) -> String {
    match before_glob.rfind('/') {
        None => ".".to_owned(),
        Some(0) => "/".to_owned(),
        Some(separator) => before_glob[..separator].to_owned(),
    }
}

/// `path.resolve` style normalization: `.` removed, `..` pops, trailing separators dropped.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

/// Port of `safeResolvePath`.
fn safe_resolve_path(path: &Path) -> PathBuf {
    let is_special = std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        use std::os::unix::fs::FileTypeExt;
        let file_type = metadata.file_type();
        file_type.is_fifo()
            || file_type.is_socket()
            || file_type.is_char_device()
            || file_type.is_block_device()
    });
    if is_special {
        return path.to_path_buf();
    }
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Port of `getPathsForPermissionCheck`: the path, its symlink chain and its resolved form.
fn paths_for_permission_check(path: &Path) -> Vec<PathBuf> {
    let mut paths = BTreeSet::from([path.to_path_buf()]);
    let mut current = path.to_path_buf();
    let mut visited = BTreeSet::new();
    for _ in 0..MAX_SYMLINK_DEPTH {
        if !visited.insert(current.clone()) {
            break;
        }
        if !current.exists() {
            if current == path
                && let Some(resolved) = resolve_deepest_existing_ancestor(path)
            {
                paths.insert(resolved);
            }
            break;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            break;
        };
        if !metadata.file_type().is_symlink() {
            break;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        let parent = current.parent().unwrap_or(Path::new("/"));
        let absolute_target = normalize_lexically(&parent.join(target));
        paths.insert(absolute_target.clone());
        current = absolute_target;
    }
    let resolved = safe_resolve_path(path);
    if resolved != path {
        paths.insert(resolved);
    }
    paths.into_iter().collect()
}

/// Port of `resolveDeepestExistingAncestorSync`.
fn resolve_deepest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut directory = path.to_path_buf();
    let mut missing = Vec::new();
    while let Some(parent) = directory.parent().map(Path::to_path_buf) {
        let Ok(metadata) = std::fs::symlink_metadata(&directory) else {
            missing.push(directory.file_name()?.to_owned());
            directory = parent;
            continue;
        };
        let join_missing = |mut base: PathBuf| {
            for segment in missing.iter().rev() {
                base.push(segment);
            }
            base
        };
        if metadata.file_type().is_symlink() {
            return Some(join_missing(directory.canonicalize().unwrap_or_else(
                |_| {
                    let target = std::fs::read_link(&directory).unwrap_or_default();
                    normalize_lexically(&parent.join(target))
                },
            )));
        }
        return directory
            .canonicalize()
            .ok()
            .filter(|resolved| *resolved != directory)
            .map(join_missing);
    }
    None
}

/// Port of `pathInWorkingPath`: case-insensitive, with macOS `/private` aliases folded.
fn path_in_working_path(path: &Path, working_path: &Path) -> bool {
    let fold = |path: &Path| {
        let text = path.to_string_lossy();
        let text = if let Some(rest) = text.strip_prefix("/private/var/") {
            format!("/var/{rest}")
        } else if text == "/private/tmp" || text.starts_with("/private/tmp/") {
            text["/private".len()..].to_owned()
        } else {
            text.into_owned()
        };
        PathBuf::from(text.to_lowercase())
    };
    fold(path).starts_with(fold(working_path))
}

/// `hasSuspiciousWindowsPathPattern` checks that apply on every platform.
fn has_suspicious_path_pattern(path: &Path) -> bool {
    let text = path.to_string_lossy();
    SHORT_NAME.is_match(&text)
        || ["\\\\?\\", "\\\\.\\", "//?/", "//./"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
        || TRAILING_DOTS_OR_SPACES.is_match(&text)
        || DOS_DEVICE_SUFFIX.is_match(&text)
        || THREE_DOT_COMPONENT.is_match(&text)
}

/// Port of `checkPathSafetyForAutoEdit` for a file write: `None` when the
/// path and its symlink chain are safe to edit without asking.
pub(in crate::tools) fn auto_edit_safety_check(path: &Path) -> Option<crate::tools::SafetyCheck> {
    let paths = paths_for_permission_check(path);
    if paths.iter().any(|path| has_suspicious_path_pattern(path)) {
        return Some(crate::tools::SafetyCheck {
            reason: format!(
                "Tick Loop requested permissions to write to {}, which contains a suspicious path pattern that requires manual approval.",
                path.display()
            ),
            classifier_approvable: false,
        });
    }
    paths
        .iter()
        .any(|path| is_dangerous_file_to_auto_edit(path))
        .then(|| crate::tools::SafetyCheck {
            reason: format!(
                "Tick Loop requested permissions to edit {} which is a sensitive file.",
                path.display()
            ),
            classifier_approvable: true,
        })
}

/// Port of `isDangerousFilePathToAutoEdit`.
fn is_dangerous_file_to_auto_edit(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.starts_with("\\\\") || text.starts_with("//") {
        return true;
    }
    let segments = text.split('/').collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| DANGEROUS_DIRECTORIES.contains(&segment.to_lowercase().as_str()))
    {
        return true;
    }
    segments
        .last()
        .is_some_and(|name| DANGEROUS_FILES.contains(&name.to_lowercase().as_str()))
}

/// Port of `isDangerousRemovalPath`.
fn is_dangerous_removal_path(path: &str) -> bool {
    let mut forward_slashed = String::new();
    for character in path.chars() {
        let is_separator = matches!(character, '/' | '\\');
        if !(is_separator && forward_slashed.ends_with('/')) {
            forward_slashed.push(if is_separator { '/' } else { character });
        }
    }
    if forward_slashed == "*" || forward_slashed.ends_with("/*") {
        return true;
    }
    let normalized = if forward_slashed == "/" {
        forward_slashed.as_str()
    } else {
        forward_slashed
            .strip_suffix('/')
            .unwrap_or(&forward_slashed)
    };
    if normalized == "/" {
        return true;
    }
    if home_directory().is_some_and(|home| home.to_string_lossy().replace('\\', "/") == normalized)
    {
        return true;
    }
    match normalized.rfind('/') {
        Some(0) => normalized.len() > 1,
        _ => false,
    }
}

fn filter_out_flags(arguments: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut after_double_dash = false;
    for argument in arguments {
        if after_double_dash {
            paths.push(argument.clone());
        } else if argument == "--" {
            after_double_dash = true;
        } else if !argument.starts_with('-') {
            paths.push(argument.clone());
        }
    }
    paths
}

fn pattern_command_paths(arguments: &[String], flags_with_arguments: &[&str]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut pattern_found = false;
    let mut after_double_dash = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if !after_double_dash && argument == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && argument.starts_with('-') {
            if let Some(value) = argument
                .strip_prefix("-f")
                .filter(|value| !value.is_empty())
            {
                pattern_found = true;
                paths.push(value.to_owned());
                continue;
            }
            if argument
                .strip_prefix("-e")
                .is_some_and(|value| !value.is_empty())
            {
                pattern_found = true;
                continue;
            }
            let flag = argument.split('=').next().unwrap_or_default();
            if matches!(flag, "-e" | "--regexp" | "-f" | "--file") {
                pattern_found = true;
            }
            if matches!(flag, "-f" | "--file" | "--exclude-from") {
                if let Some((_, value)) = argument.split_once('=') {
                    paths.push(value.to_owned());
                } else if let Some(value) = arguments.get(index) {
                    paths.push(value.clone());
                }
            }
            if flags_with_arguments.contains(&flag) && !argument.contains('=') {
                index += 1;
            }
            continue;
        }
        if !pattern_found {
            pattern_found = true;
            continue;
        }
        paths.push(argument.clone());
    }
    paths
}

/// Port of `PATH_EXTRACTORS`.
fn extract_paths(command: &str, arguments: &[String]) -> Vec<String> {
    match command {
        "cd" => {
            if arguments.is_empty() {
                home_directory()
                    .map(|home| vec![home.to_string_lossy().into_owned()])
                    .unwrap_or_default()
            } else {
                vec![arguments.join(" ")]
            }
        }
        "ls" => {
            let paths = filter_out_flags(arguments);
            if paths.is_empty() {
                vec![".".to_owned()]
            } else {
                paths
            }
        }
        "find" => find_paths(arguments),
        "tr" => {
            let has_delete = arguments.iter().any(|argument| {
                argument == "-d"
                    || argument == "--delete"
                    || (argument.starts_with('-') && argument.contains('d'))
            });
            filter_out_flags(arguments)
                .into_iter()
                .skip(if has_delete { 1 } else { 2 })
                .collect()
        }
        "grep" => {
            let paths = pattern_command_paths(
                arguments,
                &[
                    "-e",
                    "--regexp",
                    "-f",
                    "--file",
                    "--exclude-from",
                    "--exclude",
                    "--include",
                    "--exclude-dir",
                    "--include-dir",
                    "-m",
                    "--max-count",
                    "-A",
                    "--after-context",
                    "-B",
                    "--before-context",
                    "-C",
                    "--context",
                ],
            );
            if paths.is_empty()
                && arguments
                    .iter()
                    .any(|argument| matches!(argument.as_str(), "-r" | "-R" | "--recursive"))
            {
                vec![".".to_owned()]
            } else {
                paths
            }
        }
        "rg" => {
            let paths = pattern_command_paths(
                arguments,
                &[
                    "-e",
                    "--regexp",
                    "-f",
                    "--file",
                    "-t",
                    "--type",
                    "-T",
                    "--type-not",
                    "-g",
                    "--glob",
                    "-m",
                    "--max-count",
                    "--max-depth",
                    "-r",
                    "--replace",
                    "-A",
                    "--after-context",
                    "-B",
                    "--before-context",
                    "-C",
                    "--context",
                ],
            );
            if paths.is_empty() {
                vec![".".to_owned()]
            } else {
                paths
            }
        }
        "sed" => sed_paths(arguments),
        "jq" => jq_paths(arguments),
        "git" => {
            if arguments.first().is_some_and(|argument| argument == "diff")
                && arguments.iter().any(|argument| argument == "--no-index")
            {
                filter_out_flags(&arguments[1..])
                    .into_iter()
                    .take(2)
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => filter_out_flags(arguments),
    }
}

fn find_paths(arguments: &[String]) -> Vec<String> {
    const PATH_FLAGS: &[&str] = &[
        "-newer",
        "-anewer",
        "-cnewer",
        "-mnewer",
        "-samefile",
        "-path",
        "-wholename",
        "-ilname",
        "-lname",
        "-ipath",
        "-iwholename",
    ];
    let mut paths = Vec::new();
    let mut found_non_global_flag = false;
    let mut after_double_dash = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if argument.is_empty() {
            continue;
        }
        if after_double_dash {
            paths.push(argument.clone());
            continue;
        }
        if argument == "--" {
            after_double_dash = true;
            continue;
        }
        if argument.starts_with('-') {
            if matches!(argument.as_str(), "-H" | "-L" | "-P") {
                continue;
            }
            found_non_global_flag = true;
            if (PATH_FLAGS.contains(&argument.as_str()) || NEWER_FLAG.is_match(argument))
                && let Some(next) = arguments.get(index).filter(|next| !next.is_empty())
            {
                paths.push(next.clone());
                index += 1;
            }
            continue;
        }
        if !found_non_global_flag {
            paths.push(argument.clone());
        }
    }
    if paths.is_empty() {
        vec![".".to_owned()]
    } else {
        paths
    }
}

fn sed_paths(arguments: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut script_found = false;
    let mut after_double_dash = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if argument.is_empty() {
            continue;
        }
        if !after_double_dash && argument == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && argument.starts_with('-') {
            if matches!(argument.as_str(), "-f" | "--file") {
                if let Some(script) = arguments.get(index).filter(|script| !script.is_empty()) {
                    paths.push(script.clone());
                    index += 1;
                }
                script_found = true;
            } else if matches!(argument.as_str(), "-e" | "--expression") {
                index += 1;
                script_found = true;
            } else if argument.contains(['e', 'f']) {
                script_found = true;
            }
            continue;
        }
        if !script_found {
            script_found = true;
            continue;
        }
        paths.push(argument.clone());
    }
    paths
}

fn jq_paths(arguments: &[String]) -> Vec<String> {
    const FLAGS_WITH_ARGUMENTS: &[&str] = &[
        "-e",
        "--expression",
        "-f",
        "--from-file",
        "--arg",
        "--argjson",
        "--slurpfile",
        "--rawfile",
        "--args",
        "--jsonargs",
        "-L",
        "--library-path",
        "--indent",
        "--tab",
    ];
    let mut paths = Vec::new();
    let mut filter_found = false;
    let mut after_double_dash = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if !after_double_dash && argument == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && argument.starts_with('-') {
            let flag = argument.split('=').next().unwrap_or_default();
            if matches!(flag, "-e" | "--expression") {
                filter_found = true;
            }
            if FLAGS_WITH_ARGUMENTS.contains(&flag) && !argument.contains('=') {
                index += 1;
            }
            continue;
        }
        if !filter_found {
            filter_found = true;
            continue;
        }
        paths.push(argument.clone());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        roots: Vec<PathBuf>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("src")).unwrap();
            std::fs::write(root.join("src/main.rs"), "").unwrap();
            Self {
                roots: vec![root.clone()],
                root,
                _directory: directory,
            }
        }

        fn policy(&self, accept_edits: bool) -> PathPolicy<'_> {
            PathPolicy {
                cwd: &self.root,
                working_directories: &self.roots,
                readable_directories: &[],
                editable_directory: None,
                accept_edits,
            }
        }
    }

    #[test]
    fn read_paths_must_stay_inside_workspace_roots() {
        let fixture = Fixture::new();
        let policy = fixture.policy(false);
        for command in [
            "cat src/main.rs",
            "ls",
            "ls -la src",
            "grep -rn foo",
            "grep -n foo src/main.rs",
            "find . -name '*.rs'",
            "git diff --no-index src/main.rs src/other.rs",
            "head -30",
            "ls 'src/*.rs'",
            "cd src",
            "sed -n '1p' src/main.rs",
        ] {
            assert!(policy.command_paths_allowed(command, false), "{command}");
        }
        for command in [
            "cat /etc/passwd",
            "cat ../outside.txt",
            "ls ~",
            "cd",
            "cd ..",
            "grep -rn foo /etc",
            "find / -name x",
            "rg foo ~/secrets",
            "cat ~root/.ssh/id_rsa",
            "cat $HOME/x",
            "cat 100%",
            "ls =rg",
            "git diff --no-index /etc/passwd src/main.rs",
        ] {
            assert!(!policy.command_paths_allowed(command, false), "{command}");
        }
    }

    #[test]
    fn write_paths_require_accept_edits_and_safe_targets() {
        let fixture = Fixture::new();
        assert!(
            !fixture
                .policy(false)
                .command_paths_allowed("mkdir build", false)
        );
        let policy = fixture.policy(true);
        for command in [
            "mkdir -p build/out",
            "touch notes.txt",
            "rm -rf build",
            "rmdir build",
            "mv a.txt b.txt",
            "cp a.txt b.txt",
            "sed -i 's/a/b/' src/main.rs",
        ] {
            assert!(policy.command_paths_allowed(command, false), "{command}");
        }
        for command in [
            "touch ../outside.txt",
            "mkdir /tmp/x",
            "mv -f a.txt b.txt",
            "cp -r src dst",
            "cp --target-directory=/tmp a.txt",
            "touch .git/hooks/pre-commit",
            "touch .bashrc",
            "mkdir .vscode",
            "touch 'notes*.txt'",
            "rm -rf *",
            "rm -rf build/*",
            "rm -rf /",
            "rm -rf ~",
            "rm -rf /tmp",
            "touch notes.txt.",
            "touch ~/x",
        ] {
            assert!(!policy.command_paths_allowed(command, false), "{command}");
        }
        assert!(!policy.command_paths_allowed("touch notes.txt", true));
        assert!(policy.command_paths_allowed("cat src/main.rs", true));
    }

    #[test]
    fn output_redirections_are_create_operations() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .policy(false)
                .output_redirections_allowed(&["/dev/null"], false)
        );
        assert!(
            !fixture
                .policy(false)
                .output_redirections_allowed(&["out.txt"], false)
        );
        assert!(
            fixture
                .policy(true)
                .output_redirections_allowed(&["out.txt"], false)
        );
        assert!(
            !fixture
                .policy(true)
                .output_redirections_allowed(&["out.txt"], true)
        );
        assert!(
            !fixture
                .policy(true)
                .output_redirections_allowed(&["/tmp/out"], false)
        );
    }

    #[test]
    fn symlinks_leaving_the_workspace_are_outside() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.root.join("link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("missing"),
            fixture.root.join("dangling"),
        )
        .unwrap();
        let policy = fixture.policy(true);
        assert!(!policy.command_paths_allowed("cat link/file", false));
        assert!(!policy.command_paths_allowed("touch link/new.txt", false));
        assert!(!policy.command_paths_allowed("touch dangling", false));
    }

    #[test]
    fn dangerous_removal_paths_match_claude_code() {
        let home = home_directory().unwrap();
        for path in ["/", "*", "/work/*", "/usr", "/tmp/", home.to_str().unwrap()] {
            assert!(is_dangerous_removal_path(path), "{path}");
        }
        for path in ["/usr/local", "/work/build"] {
            assert!(!is_dangerous_removal_path(path), "{path}");
        }
    }
}
