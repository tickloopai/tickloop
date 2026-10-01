//! Read-only command allowlists from Claude Code's `tools/BashTool/readOnlyValidation.ts`
//! (`COMMAND_ALLOWLIST`, `READONLY_COMMANDS`) and `utils/shell/readOnlyCommandValidation.ts`
//! (git, ripgrep, pyright and docker maps). Ant-only `gh` and `aki` entries are omitted.

use std::sync::LazyLock;

use regex::Regex;

use super::sed::sed_command_is_allowed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FlagArgument {
    NoValue,
    Number,
    Text,
    Char,
    Braces,
    Eof,
}

use FlagArgument::{Braces, Char, Eof, NoValue as N, Number as NUM, Text as S};

type FlagTable = &'static [(&'static str, FlagArgument)];
type DangerCheck = fn(&str, &[String]) -> bool;

pub(super) struct CommandConfig {
    pub(super) command: &'static str,
    flag_tables: &'static [FlagTable],
    pub(super) respects_double_dash: bool,
    pub(super) regex: Option<&'static LazyLock<Regex>>,
    pub(super) is_dangerous: Option<DangerCheck>,
}

impl CommandConfig {
    const fn new(command: &'static str, flag_tables: &'static [FlagTable]) -> Self {
        Self {
            command,
            flag_tables,
            respects_double_dash: true,
            regex: None,
            is_dangerous: None,
        }
    }

    const fn dangerous_when(mut self, check: DangerCheck) -> Self {
        self.is_dangerous = Some(check);
        self
    }

    const fn ignoring_double_dash(mut self) -> Self {
        self.respects_double_dash = false;
        self
    }

    /// Later tables and later entries win, like JavaScript object spreads.
    pub(super) fn flag(&self, name: &str) -> Option<FlagArgument> {
        self.flag_tables.iter().rev().find_map(|table| {
            table
                .iter()
                .rev()
                .find(|(flag, _)| *flag == name)
                .map(|(_, argument)| *argument)
        })
    }
}

const GIT_REF_SELECTION: FlagTable = &[
    ("--all", N),
    ("--branches", N),
    ("--tags", N),
    ("--remotes", N),
];
const GIT_DATE_FILTER: FlagTable = &[
    ("--since", S),
    ("--after", S),
    ("--until", S),
    ("--before", S),
];
const GIT_LOG_DISPLAY: FlagTable = &[
    ("--oneline", N),
    ("--graph", N),
    ("--decorate", N),
    ("--no-decorate", N),
    ("--date", S),
    ("--relative-date", N),
];
const GIT_COUNT: FlagTable = &[("--max-count", NUM), ("-n", NUM)];
const GIT_STAT: FlagTable = &[
    ("--stat", N),
    ("--numstat", N),
    ("--shortstat", N),
    ("--name-only", N),
    ("--name-status", N),
];
const GIT_COLOR: FlagTable = &[("--color", N), ("--no-color", N)];
const GIT_PATCH: FlagTable = &[
    ("--patch", N),
    ("-p", N),
    ("--no-patch", N),
    ("--no-ext-diff", N),
    ("-s", N),
];
const GIT_AUTHOR_FILTER: FlagTable = &[("--author", S), ("--committer", S), ("--grep", S)];

const GIT_DIFF: FlagTable = &[
    ("--dirstat", N),
    ("--summary", N),
    ("--patch-with-stat", N),
    ("--word-diff", N),
    ("--word-diff-regex", S),
    ("--color-words", N),
    ("--no-renames", N),
    ("--no-ext-diff", N),
    ("--check", N),
    ("--ws-error-highlight", S),
    ("--full-index", N),
    ("--binary", N),
    ("--abbrev", NUM),
    ("--break-rewrites", N),
    ("--find-renames", N),
    ("--find-copies", N),
    ("--find-copies-harder", N),
    ("--irreversible-delete", N),
    ("--diff-algorithm", S),
    ("--histogram", N),
    ("--patience", N),
    ("--minimal", N),
    ("--ignore-space-at-eol", N),
    ("--ignore-space-change", N),
    ("--ignore-all-space", N),
    ("--ignore-blank-lines", N),
    ("--inter-hunk-context", NUM),
    ("--function-context", N),
    ("--exit-code", N),
    ("--quiet", N),
    ("--cached", N),
    ("--staged", N),
    ("--pickaxe-regex", N),
    ("--pickaxe-all", N),
    ("--no-index", N),
    ("--relative", S),
    ("--diff-filter", S),
    ("-p", N),
    ("-u", N),
    ("-s", N),
    ("-M", N),
    ("-C", N),
    ("-B", N),
    ("-D", N),
    ("-l", N),
    ("-S", S),
    ("-G", S),
    ("-O", S),
    ("-R", N),
];
const GIT_LOG: FlagTable = &[
    ("--abbrev-commit", N),
    ("--full-history", N),
    ("--dense", N),
    ("--sparse", N),
    ("--simplify-merges", N),
    ("--ancestry-path", N),
    ("--source", N),
    ("--first-parent", N),
    ("--merges", N),
    ("--no-merges", N),
    ("--reverse", N),
    ("--walk-reflogs", N),
    ("--skip", NUM),
    ("--max-age", NUM),
    ("--min-age", NUM),
    ("--no-min-parents", N),
    ("--no-max-parents", N),
    ("--follow", N),
    ("--no-walk", N),
    ("--left-right", N),
    ("--cherry-mark", N),
    ("--cherry-pick", N),
    ("--boundary", N),
    ("--topo-order", N),
    ("--date-order", N),
    ("--author-date-order", N),
    ("--pretty", S),
    ("--format", S),
    ("--diff-filter", S),
    ("-S", S),
    ("-G", S),
    ("--pickaxe-regex", N),
    ("--pickaxe-all", N),
];
const GIT_SHOW: FlagTable = &[
    ("--abbrev-commit", N),
    ("--word-diff", N),
    ("--word-diff-regex", S),
    ("--color-words", N),
    ("--pretty", S),
    ("--format", S),
    ("--first-parent", N),
    ("--raw", N),
    ("--diff-filter", S),
    ("-m", N),
    ("--quiet", N),
];
const GIT_SHORTLOG: FlagTable = &[
    ("-s", N),
    ("--summary", N),
    ("-n", N),
    ("--numbered", N),
    ("-e", N),
    ("--email", N),
    ("-c", N),
    ("--committer", N),
    ("--group", S),
    ("--format", S),
    ("--no-merges", N),
    ("--author", S),
];
const GIT_LS_REMOTE: FlagTable = &[
    ("--branches", N),
    ("-b", N),
    ("--tags", N),
    ("-t", N),
    ("--heads", N),
    ("-h", N),
    ("--refs", N),
    ("--quiet", N),
    ("-q", N),
    ("--exit-code", N),
    ("--get-url", N),
    ("--symref", N),
    ("--sort", S),
];
const GIT_STATUS: FlagTable = &[
    ("--short", N),
    ("-s", N),
    ("--branch", N),
    ("-b", N),
    ("--porcelain", N),
    ("--long", N),
    ("--verbose", N),
    ("-v", N),
    ("--untracked-files", S),
    ("-u", S),
    ("--ignored", N),
    ("--ignore-submodules", S),
    ("--column", N),
    ("--no-column", N),
    ("--ahead-behind", N),
    ("--no-ahead-behind", N),
    ("--renames", N),
    ("--no-renames", N),
    ("--find-renames", S),
    ("-M", S),
];
const GIT_BLAME: FlagTable = &[
    ("-L", S),
    ("--porcelain", N),
    ("-p", N),
    ("--line-porcelain", N),
    ("--incremental", N),
    ("--root", N),
    ("--show-stats", N),
    ("--show-name", N),
    ("--show-number", N),
    ("-n", N),
    ("--show-email", N),
    ("-e", N),
    ("-f", N),
    ("--date", S),
    ("-w", N),
    ("--ignore-rev", S),
    ("--ignore-revs-file", S),
    ("-M", N),
    ("-C", N),
    ("--score-debug", N),
    ("--abbrev", NUM),
    ("-s", N),
    ("-l", N),
    ("-t", N),
];
const GIT_LS_FILES: FlagTable = &[
    ("--cached", N),
    ("-c", N),
    ("--deleted", N),
    ("-d", N),
    ("--modified", N),
    ("-m", N),
    ("--others", N),
    ("-o", N),
    ("--ignored", N),
    ("-i", N),
    ("--stage", N),
    ("-s", N),
    ("--killed", N),
    ("-k", N),
    ("--unmerged", N),
    ("-u", N),
    ("--directory", N),
    ("--no-empty-directory", N),
    ("--eol", N),
    ("--full-name", N),
    ("--abbrev", NUM),
    ("--debug", N),
    ("-z", N),
    ("-t", N),
    ("-v", N),
    ("-f", N),
    ("--exclude", S),
    ("-x", S),
    ("--exclude-from", S),
    ("-X", S),
    ("--exclude-per-directory", S),
    ("--exclude-standard", N),
    ("--error-unmatch", N),
    ("--recurse-submodules", N),
];
const GIT_CONFIG_GET: FlagTable = &[
    ("--local", N),
    ("--global", N),
    ("--system", N),
    ("--worktree", N),
    ("--default", S),
    ("--type", S),
    ("--bool", N),
    ("--int", N),
    ("--bool-or-int", N),
    ("--path", N),
    ("--expiry-date", N),
    ("-z", N),
    ("--null", N),
    ("--name-only", N),
    ("--show-origin", N),
    ("--show-scope", N),
];
const GIT_MERGE_BASE: FlagTable = &[
    ("--is-ancestor", N),
    ("--fork-point", N),
    ("--octopus", N),
    ("--independent", N),
    ("--all", N),
];
const GIT_REV_PARSE: FlagTable = &[
    ("--verify", N),
    ("--short", S),
    ("--abbrev-ref", N),
    ("--symbolic", N),
    ("--symbolic-full-name", N),
    ("--show-toplevel", N),
    ("--show-cdup", N),
    ("--show-prefix", N),
    ("--git-dir", N),
    ("--git-common-dir", N),
    ("--absolute-git-dir", N),
    ("--show-superproject-working-tree", N),
    ("--is-inside-work-tree", N),
    ("--is-inside-git-dir", N),
    ("--is-bare-repository", N),
    ("--is-shallow-repository", N),
    ("--is-shallow-update", N),
    ("--path-prefix", N),
];
const GIT_REV_LIST: FlagTable = &[
    ("--count", N),
    ("--reverse", N),
    ("--first-parent", N),
    ("--ancestry-path", N),
    ("--merges", N),
    ("--no-merges", N),
    ("--min-parents", NUM),
    ("--max-parents", NUM),
    ("--no-min-parents", N),
    ("--no-max-parents", N),
    ("--skip", NUM),
    ("--max-age", NUM),
    ("--min-age", NUM),
    ("--walk-reflogs", N),
    ("--oneline", N),
    ("--abbrev-commit", N),
    ("--pretty", S),
    ("--format", S),
    ("--abbrev", NUM),
    ("--full-history", N),
    ("--dense", N),
    ("--sparse", N),
    ("--source", N),
    ("--graph", N),
];
const GIT_DESCRIBE: FlagTable = &[
    ("--tags", N),
    ("--match", S),
    ("--exclude", S),
    ("--long", N),
    ("--abbrev", NUM),
    ("--always", N),
    ("--contains", N),
    ("--first-match", N),
    ("--exact-match", N),
    ("--candidates", NUM),
    ("--dirty", N),
    ("--broken", N),
];
const GIT_CAT_FILE: FlagTable = &[
    ("-t", N),
    ("-s", N),
    ("-p", N),
    ("-e", N),
    ("--batch-check", N),
    ("--allow-undetermined-type", N),
];
const GIT_FOR_EACH_REF: FlagTable = &[
    ("--format", S),
    ("--sort", S),
    ("--count", NUM),
    ("--contains", S),
    ("--no-contains", S),
    ("--merged", S),
    ("--no-merged", S),
    ("--points-at", S),
];
const GIT_GREP: FlagTable = &[
    ("-e", S),
    ("-E", N),
    ("--extended-regexp", N),
    ("-G", N),
    ("--basic-regexp", N),
    ("-F", N),
    ("--fixed-strings", N),
    ("-P", N),
    ("--perl-regexp", N),
    ("-i", N),
    ("--ignore-case", N),
    ("-v", N),
    ("--invert-match", N),
    ("-w", N),
    ("--word-regexp", N),
    ("-n", N),
    ("--line-number", N),
    ("-c", N),
    ("--count", N),
    ("-l", N),
    ("--files-with-matches", N),
    ("-L", N),
    ("--files-without-match", N),
    ("-h", N),
    ("-H", N),
    ("--heading", N),
    ("--break", N),
    ("--full-name", N),
    ("--color", N),
    ("--no-color", N),
    ("-o", N),
    ("--only-matching", N),
    ("-A", NUM),
    ("--after-context", NUM),
    ("-B", NUM),
    ("--before-context", NUM),
    ("-C", NUM),
    ("--context", NUM),
    ("--and", N),
    ("--or", N),
    ("--not", N),
    ("--max-depth", NUM),
    ("--untracked", N),
    ("--no-index", N),
    ("--recurse-submodules", N),
    ("--cached", N),
    ("--threads", NUM),
    ("-q", N),
    ("--quiet", N),
];
const GIT_STASH_SHOW: FlagTable = &[
    ("--word-diff", N),
    ("--word-diff-regex", S),
    ("--diff-filter", S),
    ("--abbrev", NUM),
];
const GIT_WORKTREE_LIST: FlagTable = &[
    ("--porcelain", N),
    ("-v", N),
    ("--verbose", N),
    ("--expire", S),
];
const GIT_TAG: FlagTable = &[
    ("-l", N),
    ("--list", N),
    ("-n", NUM),
    ("--contains", S),
    ("--no-contains", S),
    ("--merged", S),
    ("--no-merged", S),
    ("--sort", S),
    ("--format", S),
    ("--points-at", S),
    ("--column", N),
    ("--no-column", N),
    ("-i", N),
    ("--ignore-case", N),
];
const GIT_BRANCH: FlagTable = &[
    ("-l", N),
    ("--list", N),
    ("-a", N),
    ("--all", N),
    ("-r", N),
    ("--remotes", N),
    ("-v", N),
    ("-vv", N),
    ("--verbose", N),
    ("--color", N),
    ("--no-color", N),
    ("--column", N),
    ("--no-column", N),
    ("--abbrev", NUM),
    ("--no-abbrev", N),
    ("--contains", S),
    ("--no-contains", S),
    ("--merged", N),
    ("--no-merged", N),
    ("--points-at", S),
    ("--sort", S),
    ("--show-current", N),
    ("-i", N),
    ("--ignore-case", N),
];

const XARGS: FlagTable = &[
    ("-I", Braces),
    ("-n", NUM),
    ("-P", NUM),
    ("-L", NUM),
    ("-s", NUM),
    ("-E", Eof),
    ("-0", N),
    ("-t", N),
    ("-r", N),
    ("-x", N),
    ("-d", Char),
];
const FILE: FlagTable = &[
    ("--brief", N),
    ("-b", N),
    ("--mime", N),
    ("-i", N),
    ("--mime-type", N),
    ("--mime-encoding", N),
    ("--apple", N),
    ("--check-encoding", N),
    ("-c", N),
    ("--exclude", S),
    ("--exclude-quiet", S),
    ("--print0", N),
    ("-0", N),
    ("-f", S),
    ("-F", S),
    ("--separator", S),
    ("--help", N),
    ("--version", N),
    ("-v", N),
    ("--no-dereference", N),
    ("-h", N),
    ("--dereference", N),
    ("-L", N),
    ("--magic-file", S),
    ("-m", S),
    ("--keep-going", N),
    ("-k", N),
    ("--list", N),
    ("-l", N),
    ("--no-buffer", N),
    ("-n", N),
    ("--preserve-date", N),
    ("-p", N),
    ("--raw", N),
    ("-r", N),
    ("-s", N),
    ("--special-files", N),
    ("--uncompress", N),
    ("-z", N),
];
const SED: FlagTable = &[
    ("--expression", S),
    ("-e", S),
    ("--quiet", N),
    ("--silent", N),
    ("-n", N),
    ("--regexp-extended", N),
    ("-r", N),
    ("--posix", N),
    ("-E", N),
    ("--line-length", NUM),
    ("-l", NUM),
    ("--zero-terminated", N),
    ("-z", N),
    ("--separate", N),
    ("-s", N),
    ("--unbuffered", N),
    ("-u", N),
    ("--debug", N),
    ("--help", N),
    ("--version", N),
];
const SORT: FlagTable = &[
    ("--ignore-leading-blanks", N),
    ("-b", N),
    ("--dictionary-order", N),
    ("-d", N),
    ("--ignore-case", N),
    ("-f", N),
    ("--general-numeric-sort", N),
    ("-g", N),
    ("--human-numeric-sort", N),
    ("-h", N),
    ("--ignore-nonprinting", N),
    ("-i", N),
    ("--month-sort", N),
    ("-M", N),
    ("--numeric-sort", N),
    ("-n", N),
    ("--random-sort", N),
    ("-R", N),
    ("--reverse", N),
    ("-r", N),
    ("--sort", S),
    ("--stable", N),
    ("-s", N),
    ("--unique", N),
    ("-u", N),
    ("--version-sort", N),
    ("-V", N),
    ("--zero-terminated", N),
    ("-z", N),
    ("--key", S),
    ("-k", S),
    ("--field-separator", S),
    ("-t", S),
    ("--check", N),
    ("-c", N),
    ("--check-char-order", N),
    ("-C", N),
    ("--merge", N),
    ("-m", N),
    ("--buffer-size", S),
    ("-S", S),
    ("--parallel", NUM),
    ("--batch-size", NUM),
    ("--help", N),
    ("--version", N),
];
const MAN: FlagTable = &[
    ("-a", N),
    ("--all", N),
    ("-d", N),
    ("-f", N),
    ("--whatis", N),
    ("-h", N),
    ("-k", N),
    ("--apropos", N),
    ("-l", S),
    ("-w", N),
    ("-S", S),
    ("-s", S),
];
const HELP: FlagTable = &[("-d", N), ("-m", N), ("-s", N)];
const NETSTAT: FlagTable = &[
    ("-a", N),
    ("-L", N),
    ("-l", N),
    ("-n", N),
    ("-f", S),
    ("-g", N),
    ("-i", N),
    ("-I", S),
    ("-s", N),
    ("-r", N),
    ("-m", N),
    ("-v", N),
];
const PS: FlagTable = &[
    ("-e", N),
    ("-A", N),
    ("-a", N),
    ("-d", N),
    ("-N", N),
    ("--deselect", N),
    ("-f", N),
    ("-F", N),
    ("-l", N),
    ("-j", N),
    ("-y", N),
    ("-w", N),
    ("-ww", N),
    ("--width", NUM),
    ("-c", N),
    ("-H", N),
    ("--forest", N),
    ("--headers", N),
    ("--no-headers", N),
    ("-n", S),
    ("--sort", S),
    ("-L", N),
    ("-T", N),
    ("-m", N),
    ("-C", S),
    ("-G", S),
    ("-g", S),
    ("-p", S),
    ("--pid", S),
    ("-q", S),
    ("--quick-pid", S),
    ("-s", S),
    ("--sid", S),
    ("-t", S),
    ("--tty", S),
    ("-U", S),
    ("-u", S),
    ("--user", S),
    ("--help", N),
    ("--info", N),
    ("-V", N),
    ("--version", N),
];
const BASE64: FlagTable = &[
    ("-d", N),
    ("-D", N),
    ("--decode", N),
    ("-b", NUM),
    ("--break", NUM),
    ("-w", NUM),
    ("--wrap", NUM),
    ("-i", S),
    ("--input", S),
    ("--ignore-garbage", N),
    ("-h", N),
    ("--help", N),
    ("--version", N),
];
const GREP: FlagTable = &[
    ("-e", S),
    ("--regexp", S),
    ("-f", S),
    ("--file", S),
    ("-F", N),
    ("--fixed-strings", N),
    ("-G", N),
    ("--basic-regexp", N),
    ("-E", N),
    ("--extended-regexp", N),
    ("-P", N),
    ("--perl-regexp", N),
    ("-i", N),
    ("--ignore-case", N),
    ("--no-ignore-case", N),
    ("-v", N),
    ("--invert-match", N),
    ("-w", N),
    ("--word-regexp", N),
    ("-x", N),
    ("--line-regexp", N),
    ("-c", N),
    ("--count", N),
    ("--color", S),
    ("--colour", S),
    ("-L", N),
    ("--files-without-match", N),
    ("-l", N),
    ("--files-with-matches", N),
    ("-m", NUM),
    ("--max-count", NUM),
    ("-o", N),
    ("--only-matching", N),
    ("-q", N),
    ("--quiet", N),
    ("--silent", N),
    ("-s", N),
    ("--no-messages", N),
    ("-b", N),
    ("--byte-offset", N),
    ("-H", N),
    ("--with-filename", N),
    ("-h", N),
    ("--no-filename", N),
    ("--label", S),
    ("-n", N),
    ("--line-number", N),
    ("-T", N),
    ("--initial-tab", N),
    ("-u", N),
    ("--unix-byte-offsets", N),
    ("-Z", N),
    ("--null", N),
    ("-z", N),
    ("--null-data", N),
    ("-A", NUM),
    ("--after-context", NUM),
    ("-B", NUM),
    ("--before-context", NUM),
    ("-C", NUM),
    ("--context", NUM),
    ("--group-separator", S),
    ("--no-group-separator", N),
    ("-a", N),
    ("--text", N),
    ("--binary-files", S),
    ("-D", S),
    ("--devices", S),
    ("-d", S),
    ("--directories", S),
    ("--exclude", S),
    ("--exclude-from", S),
    ("--exclude-dir", S),
    ("--include", S),
    ("-r", N),
    ("--recursive", N),
    ("-R", N),
    ("--dereference-recursive", N),
    ("--line-buffered", N),
    ("-U", N),
    ("--binary", N),
    ("--help", N),
    ("-V", N),
    ("--version", N),
];
const RIPGREP: FlagTable = &[
    ("-e", S),
    ("--regexp", S),
    ("-f", S),
    ("-i", N),
    ("--ignore-case", N),
    ("-S", N),
    ("--smart-case", N),
    ("-F", N),
    ("--fixed-strings", N),
    ("-w", N),
    ("--word-regexp", N),
    ("-v", N),
    ("--invert-match", N),
    ("-c", N),
    ("--count", N),
    ("-l", N),
    ("--files-with-matches", N),
    ("--files-without-match", N),
    ("-n", N),
    ("--line-number", N),
    ("-o", N),
    ("--only-matching", N),
    ("-A", NUM),
    ("--after-context", NUM),
    ("-B", NUM),
    ("--before-context", NUM),
    ("-C", NUM),
    ("--context", NUM),
    ("-H", N),
    ("-h", N),
    ("--heading", N),
    ("--no-heading", N),
    ("-q", N),
    ("--quiet", N),
    ("--column", N),
    ("-g", S),
    ("--glob", S),
    ("-t", S),
    ("--type", S),
    ("-T", S),
    ("--type-not", S),
    ("--type-list", N),
    ("--hidden", N),
    ("--no-ignore", N),
    ("-u", N),
    ("-m", NUM),
    ("--max-count", NUM),
    ("-d", NUM),
    ("--max-depth", NUM),
    ("-a", N),
    ("--text", N),
    ("-z", N),
    ("-L", N),
    ("--follow", N),
    ("--color", S),
    ("--json", N),
    ("--stats", N),
    ("--help", N),
    ("--version", N),
    ("--debug", N),
    ("--", N),
];
const CHECKSUM: FlagTable = &[
    ("-b", N),
    ("--binary", N),
    ("-t", N),
    ("--text", N),
    ("-c", N),
    ("--check", N),
    ("--ignore-missing", N),
    ("--quiet", N),
    ("--status", N),
    ("--strict", N),
    ("-w", N),
    ("--warn", N),
    ("--tag", N),
    ("-z", N),
    ("--zero", N),
    ("--help", N),
    ("--version", N),
];
const TREE: FlagTable = &[
    ("-a", N),
    ("-d", N),
    ("-l", N),
    ("-f", N),
    ("-x", N),
    ("-L", NUM),
    ("-P", S),
    ("-I", S),
    ("--gitignore", N),
    ("--gitfile", S),
    ("--ignore-case", N),
    ("--matchdirs", N),
    ("--metafirst", N),
    ("--prune", N),
    ("--info", N),
    ("--infofile", S),
    ("--noreport", N),
    ("--charset", S),
    ("--filelimit", NUM),
    ("-q", N),
    ("-N", N),
    ("-Q", N),
    ("-p", N),
    ("-u", N),
    ("-g", N),
    ("-s", N),
    ("-h", N),
    ("--si", N),
    ("--du", N),
    ("-D", N),
    ("--timefmt", S),
    ("-F", N),
    ("--inodes", N),
    ("--device", N),
    ("-v", N),
    ("-t", N),
    ("-c", N),
    ("-U", N),
    ("-r", N),
    ("--dirsfirst", N),
    ("--filesfirst", N),
    ("--sort", S),
    ("-i", N),
    ("-A", N),
    ("-S", N),
    ("-n", N),
    ("-C", N),
    ("-X", N),
    ("-J", N),
    ("-H", S),
    ("--nolinks", N),
    ("--hintro", S),
    ("--houtro", S),
    ("-T", S),
    ("--hyperlink", N),
    ("--scheme", S),
    ("--authority", S),
    ("--fromfile", N),
    ("--fromtabfile", N),
    ("--fflinks", N),
    ("--help", N),
    ("--version", N),
];
const DATE: FlagTable = &[
    ("-d", S),
    ("--date", S),
    ("-r", S),
    ("--reference", S),
    ("-u", N),
    ("--utc", N),
    ("--universal", N),
    ("-I", N),
    ("--iso-8601", S),
    ("-R", N),
    ("--rfc-email", N),
    ("--rfc-3339", S),
    ("--debug", N),
    ("--help", N),
    ("--version", N),
];
const HOSTNAME: FlagTable = &[
    ("-f", N),
    ("--fqdn", N),
    ("--long", N),
    ("-s", N),
    ("--short", N),
    ("-i", N),
    ("--ip-address", N),
    ("-I", N),
    ("--all-ip-addresses", N),
    ("-a", N),
    ("--alias", N),
    ("-d", N),
    ("--domain", N),
    ("-A", N),
    ("--all-fqdns", N),
    ("-v", N),
    ("--verbose", N),
    ("-h", N),
    ("--help", N),
    ("-V", N),
    ("--version", N),
];
const INFO: FlagTable = &[
    ("-f", S),
    ("--file", S),
    ("-d", S),
    ("--directory", S),
    ("-n", S),
    ("--node", S),
    ("-a", N),
    ("--all", N),
    ("-k", S),
    ("--apropos", S),
    ("-w", N),
    ("--where", N),
    ("--location", N),
    ("--show-options", N),
    ("--vi-keys", N),
    ("--subnodes", N),
    ("-h", N),
    ("--help", N),
    ("--usage", N),
    ("--version", N),
];
const LSOF: FlagTable = &[
    ("-?", N),
    ("-h", N),
    ("-v", N),
    ("-a", N),
    ("-b", N),
    ("-C", N),
    ("-l", N),
    ("-n", N),
    ("-N", N),
    ("-O", N),
    ("-P", N),
    ("-Q", N),
    ("-R", N),
    ("-t", N),
    ("-U", N),
    ("-V", N),
    ("-X", N),
    ("-H", N),
    ("-E", N),
    ("-F", N),
    ("-g", N),
    ("-i", N),
    ("-K", N),
    ("-L", N),
    ("-o", N),
    ("-r", N),
    ("-s", N),
    ("-S", N),
    ("-T", N),
    ("-x", N),
    ("-A", S),
    ("-c", S),
    ("-d", S),
    ("-e", S),
    ("-k", S),
    ("-p", S),
    ("-u", S),
];
const PGREP: FlagTable = &[
    ("-d", S),
    ("--delimiter", S),
    ("-l", N),
    ("--list-name", N),
    ("-a", N),
    ("--list-full", N),
    ("-v", N),
    ("--inverse", N),
    ("-w", N),
    ("--lightweight", N),
    ("-c", N),
    ("--count", N),
    ("-f", N),
    ("--full", N),
    ("-g", S),
    ("--pgroup", S),
    ("-G", S),
    ("--group", S),
    ("-i", N),
    ("--ignore-case", N),
    ("-n", N),
    ("--newest", N),
    ("-o", N),
    ("--oldest", N),
    ("-O", S),
    ("--older", S),
    ("-P", S),
    ("--parent", S),
    ("-s", S),
    ("--session", S),
    ("-t", S),
    ("--terminal", S),
    ("-u", S),
    ("--euid", S),
    ("-U", S),
    ("--uid", S),
    ("-x", N),
    ("--exact", N),
    ("-F", S),
    ("--pidfile", S),
    ("-L", N),
    ("--logpidfile", N),
    ("-r", S),
    ("--runstates", S),
    ("--ns", S),
    ("--nslist", S),
    ("--help", N),
    ("-V", N),
    ("--version", N),
];
const TPUT: FlagTable = &[("-T", S), ("-V", N), ("-x", N)];
const SS: FlagTable = &[
    ("-h", N),
    ("--help", N),
    ("-V", N),
    ("--version", N),
    ("-n", N),
    ("--numeric", N),
    ("-r", N),
    ("--resolve", N),
    ("-a", N),
    ("--all", N),
    ("-l", N),
    ("--listening", N),
    ("-o", N),
    ("--options", N),
    ("-e", N),
    ("--extended", N),
    ("-m", N),
    ("--memory", N),
    ("-p", N),
    ("--processes", N),
    ("-i", N),
    ("--info", N),
    ("-s", N),
    ("--summary", N),
    ("-4", N),
    ("--ipv4", N),
    ("-6", N),
    ("--ipv6", N),
    ("-0", N),
    ("--packet", N),
    ("-t", N),
    ("--tcp", N),
    ("-M", N),
    ("--mptcp", N),
    ("-S", N),
    ("--sctp", N),
    ("-u", N),
    ("--udp", N),
    ("-d", N),
    ("--dccp", N),
    ("-w", N),
    ("--raw", N),
    ("-x", N),
    ("--unix", N),
    ("--tipc", N),
    ("--vsock", N),
    ("-f", S),
    ("--family", S),
    ("-A", S),
    ("--query", S),
    ("--socket", S),
    ("-Z", N),
    ("--context", N),
    ("-z", N),
    ("--contexts", N),
    ("-b", N),
    ("--bpf", N),
    ("-E", N),
    ("--events", N),
    ("-H", N),
    ("--no-header", N),
    ("-O", N),
    ("--oneline", N),
    ("--tipcinfo", N),
    ("--tos", N),
    ("--cgroup", N),
    ("--inet-sockopt", N),
];
const FD: FlagTable = &[
    ("-h", N),
    ("--help", N),
    ("-V", N),
    ("--version", N),
    ("-H", N),
    ("--hidden", N),
    ("-I", N),
    ("--no-ignore", N),
    ("--no-ignore-vcs", N),
    ("--no-ignore-parent", N),
    ("-s", N),
    ("--case-sensitive", N),
    ("-i", N),
    ("--ignore-case", N),
    ("-g", N),
    ("--glob", N),
    ("--regex", N),
    ("-F", N),
    ("--fixed-strings", N),
    ("-a", N),
    ("--absolute-path", N),
    ("-L", N),
    ("--follow", N),
    ("-p", N),
    ("--full-path", N),
    ("-0", N),
    ("--print0", N),
    ("-d", NUM),
    ("--max-depth", NUM),
    ("--min-depth", NUM),
    ("--exact-depth", NUM),
    ("-t", S),
    ("--type", S),
    ("-e", S),
    ("--extension", S),
    ("-S", S),
    ("--size", S),
    ("--changed-within", S),
    ("--changed-before", S),
    ("-o", S),
    ("--owner", S),
    ("-E", S),
    ("--exclude", S),
    ("--ignore-file", S),
    ("-c", S),
    ("--color", S),
    ("-j", NUM),
    ("--threads", NUM),
    ("--max-buffer-time", S),
    ("--max-results", NUM),
    ("-1", N),
    ("-q", N),
    ("--quiet", N),
    ("--show-errors", N),
    ("--strip-cwd-prefix", N),
    ("--one-file-system", N),
    ("--prune", N),
    ("--search-path", S),
    ("--base-directory", S),
    ("--path-separator", S),
    ("--batch-size", NUM),
    ("--no-require-git", N),
    ("--hyperlink", S),
    ("--and", S),
    ("--format", S),
];
const PYRIGHT: FlagTable = &[
    ("--outputjson", N),
    ("--project", S),
    ("-p", S),
    ("--pythonversion", S),
    ("--pythonplatform", S),
    ("--typeshedpath", S),
    ("--venvpath", S),
    ("--level", S),
    ("--stats", N),
    ("--verbose", N),
    ("--version", N),
    ("--dependencies", N),
    ("--warnings", N),
];
const DOCKER_LOGS: FlagTable = &[
    ("--follow", N),
    ("-f", N),
    ("--tail", S),
    ("-n", S),
    ("--timestamps", N),
    ("-t", N),
    ("--since", S),
    ("--until", S),
    ("--details", N),
];
const DOCKER_INSPECT: FlagTable = &[
    ("--format", S),
    ("-f", S),
    ("--type", S),
    ("--size", N),
    ("-s", N),
];

static HOSTNAME_ARGUMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^hostname(?:\s+(?:-[a-zA-Z]|--[a-zA-Z-]+))*\s*$").expect("hostname pattern")
});
static BSD_ENVIRONMENT_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z]*e[a-zA-Z]*$").expect("ps pattern"));
static REMOTE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("remote pattern"));

const TPUT_DANGEROUS_CAPABILITIES: &[&str] = &[
    "init", "reset", "rs1", "rs2", "rs3", "is1", "is2", "is3", "iprog", "if", "rf", "clear",
    "flash", "mc0", "mc4", "mc5", "mc5i", "mc5p", "pfkey", "pfloc", "pfx", "pfxl", "smcup",
    "rmcup",
];

pub(super) const SAFE_XARGS_TARGETS: &[&str] = &["echo", "printf", "wc", "grep", "head", "tail"];

pub(super) static COMMAND_ALLOWLIST: LazyLock<Vec<CommandConfig>> = LazyLock::new(|| {
    let mut hostname = CommandConfig::new("hostname", &[HOSTNAME]);
    hostname.regex = Some(&HOSTNAME_ARGUMENTS);
    vec![
        CommandConfig::new("xargs", &[XARGS]),
        CommandConfig::new("git diff", &[GIT_STAT, GIT_COLOR, GIT_DIFF]),
        CommandConfig::new(
            "git log",
            &[
                GIT_LOG_DISPLAY,
                GIT_REF_SELECTION,
                GIT_DATE_FILTER,
                GIT_COUNT,
                GIT_STAT,
                GIT_COLOR,
                GIT_PATCH,
                GIT_AUTHOR_FILTER,
                GIT_LOG,
            ],
        ),
        CommandConfig::new(
            "git show",
            &[GIT_LOG_DISPLAY, GIT_STAT, GIT_COLOR, GIT_PATCH, GIT_SHOW],
        ),
        CommandConfig::new(
            "git shortlog",
            &[GIT_REF_SELECTION, GIT_DATE_FILTER, GIT_SHORTLOG],
        ),
        CommandConfig::new(
            "git reflog",
            &[
                GIT_LOG_DISPLAY,
                GIT_REF_SELECTION,
                GIT_DATE_FILTER,
                GIT_COUNT,
                GIT_AUTHOR_FILTER,
            ],
        )
        .dangerous_when(git_reflog_is_dangerous),
        CommandConfig::new(
            "git stash list",
            &[GIT_LOG_DISPLAY, GIT_REF_SELECTION, GIT_COUNT],
        ),
        CommandConfig::new("git ls-remote", &[GIT_LS_REMOTE]),
        CommandConfig::new("git status", &[GIT_STATUS]),
        CommandConfig::new("git blame", &[GIT_COLOR, GIT_BLAME]),
        CommandConfig::new("git ls-files", &[GIT_LS_FILES]),
        CommandConfig::new("git config --get", &[GIT_CONFIG_GET]),
        CommandConfig::new("git remote show", &[&[("-n", N)]])
            .dangerous_when(git_remote_show_is_dangerous),
        CommandConfig::new("git remote", &[&[("-v", N), ("--verbose", N)]])
            .dangerous_when(git_remote_is_dangerous),
        CommandConfig::new("git merge-base", &[GIT_MERGE_BASE]),
        CommandConfig::new("git rev-parse", &[GIT_REV_PARSE]),
        CommandConfig::new(
            "git rev-list",
            &[
                GIT_REF_SELECTION,
                GIT_DATE_FILTER,
                GIT_COUNT,
                GIT_AUTHOR_FILTER,
                GIT_REV_LIST,
            ],
        ),
        CommandConfig::new("git describe", &[GIT_DESCRIBE]),
        CommandConfig::new("git cat-file", &[GIT_CAT_FILE]),
        CommandConfig::new("git for-each-ref", &[GIT_FOR_EACH_REF]),
        CommandConfig::new("git grep", &[GIT_GREP]),
        CommandConfig::new(
            "git stash show",
            &[GIT_STAT, GIT_COLOR, GIT_PATCH, GIT_STASH_SHOW],
        ),
        CommandConfig::new("git worktree list", &[GIT_WORKTREE_LIST]),
        CommandConfig::new("git tag", &[GIT_TAG]).dangerous_when(git_tag_is_dangerous),
        CommandConfig::new("git branch", &[GIT_BRANCH]).dangerous_when(git_branch_is_dangerous),
        CommandConfig::new("file", &[FILE]),
        CommandConfig::new("sed", &[SED])
            .dangerous_when(|raw_command, _| !sed_command_is_allowed(raw_command, false)),
        CommandConfig::new("sort", &[SORT]),
        CommandConfig::new("man", &[MAN]),
        CommandConfig::new("help", &[HELP]),
        CommandConfig::new("netstat", &[NETSTAT]),
        CommandConfig::new("ps", &[PS]).dangerous_when(|_, arguments| {
            arguments.iter().any(|argument| {
                !argument.starts_with('-') && BSD_ENVIRONMENT_OPTION.is_match(argument)
            })
        }),
        CommandConfig::new("base64", &[BASE64]).ignoring_double_dash(),
        CommandConfig::new("grep", &[GREP]),
        CommandConfig::new("rg", &[RIPGREP]),
        CommandConfig::new("sha256sum", &[CHECKSUM]),
        CommandConfig::new("sha1sum", &[CHECKSUM]),
        CommandConfig::new("md5sum", &[CHECKSUM]),
        CommandConfig::new("tree", &[TREE]),
        CommandConfig::new("date", &[DATE]).dangerous_when(date_is_dangerous),
        hostname,
        CommandConfig::new("info", &[INFO]),
        CommandConfig::new("lsof", &[LSOF]).dangerous_when(|_, arguments| {
            arguments.iter().any(|argument| argument.starts_with("+m"))
        }),
        CommandConfig::new("pgrep", &[PGREP]),
        CommandConfig::new("tput", &[TPUT]).dangerous_when(tput_is_dangerous),
        CommandConfig::new("ss", &[SS]),
        CommandConfig::new("fd", &[FD]),
        CommandConfig::new("fdfind", &[FD]),
        CommandConfig::new("pyright", &[PYRIGHT])
            .ignoring_double_dash()
            .dangerous_when(|_, arguments| {
                arguments
                    .iter()
                    .any(|argument| argument == "--watch" || argument == "-w")
            }),
        CommandConfig::new("docker logs", &[DOCKER_LOGS]),
        CommandConfig::new("docker inspect", &[DOCKER_INSPECT]),
    ]
});

/// Simple commands whose arguments only need `makeRegexForSafeCommand`.
pub(super) const READONLY_COMMANDS: &[&str] = &[
    "docker ps",
    "docker images",
    "cal",
    "uptime",
    "cat",
    "head",
    "tail",
    "wc",
    "stat",
    "strings",
    "hexdump",
    "od",
    "nl",
    "id",
    "uname",
    "free",
    "df",
    "du",
    "locale",
    "groups",
    "nproc",
    "basename",
    "dirname",
    "realpath",
    "cut",
    "paste",
    "tr",
    "column",
    "tac",
    "rev",
    "fold",
    "expand",
    "unexpand",
    "fmt",
    "comm",
    "cmp",
    "numfmt",
    "readlink",
    "diff",
    "true",
    "false",
    "sleep",
    "which",
    "type",
    "expr",
    "test",
    "getconf",
    "seq",
    "tsort",
    "pr",
];

fn date_is_dangerous(_: &str, arguments: &[String]) -> bool {
    const FLAGS_WITH_ARGUMENTS: &[&str] = &[
        "-d",
        "--date",
        "-r",
        "--reference",
        "--iso-8601",
        "--rfc-3339",
    ];
    let mut index = 0;
    while index < arguments.len() {
        let token = &arguments[index];
        if token.starts_with("--") && token.contains('=') {
            index += 1;
        } else if token.starts_with('-') {
            index += if FLAGS_WITH_ARGUMENTS.contains(&token.as_str()) {
                2
            } else {
                1
            };
        } else {
            if !token.starts_with('+') {
                return true;
            }
            index += 1;
        }
    }
    false
}

fn tput_is_dangerous(_: &str, arguments: &[String]) -> bool {
    let mut index = 0;
    let mut after_double_dash = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if token == "--" {
            after_double_dash = true;
            index += 1;
        } else if !after_double_dash && token.starts_with('-') {
            if token == "-S" || (!token.starts_with("--") && token.len() > 2 && token.contains('S'))
            {
                return true;
            }
            index += if token == "-T" { 2 } else { 1 };
        } else {
            if TPUT_DANGEROUS_CAPABILITIES.contains(&token.as_str()) {
                return true;
            }
            index += 1;
        }
    }
    false
}

fn git_reflog_is_dangerous(_: &str, arguments: &[String]) -> bool {
    arguments
        .iter()
        .find(|token| !token.is_empty() && !token.starts_with('-'))
        .is_some_and(|token| matches!(token.as_str(), "expire" | "delete" | "exists"))
}

fn git_remote_show_is_dangerous(_: &str, arguments: &[String]) -> bool {
    let positional = arguments
        .iter()
        .filter(|argument| *argument != "-n")
        .collect::<Vec<_>>();
    positional.len() != 1 || !REMOTE_NAME.is_match(positional[0])
}

fn git_remote_is_dangerous(_: &str, arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|argument| argument != "-v" && argument != "--verbose")
}

fn is_list_flag(token: &str) -> bool {
    token == "--list"
        || token == "-l"
        || (token.starts_with('-')
            && !token.starts_with("--")
            && token.len() > 2
            && !token.contains('=')
            && token[1..].contains('l'))
}

fn git_tag_is_dangerous(_: &str, arguments: &[String]) -> bool {
    const FLAGS_WITH_ARGUMENTS: &[&str] = &[
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--sort",
        "--format",
        "-n",
    ];
    let mut index = 0;
    let mut seen_list_flag = false;
    let mut seen_double_dash = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if token.is_empty() {
            index += 1;
            continue;
        }
        if token == "--" && !seen_double_dash {
            seen_double_dash = true;
            index += 1;
            continue;
        }
        if !seen_double_dash && token.starts_with('-') {
            seen_list_flag |= is_list_flag(token);
            index += if !token.contains('=') && FLAGS_WITH_ARGUMENTS.contains(&token.as_str()) {
                2
            } else {
                1
            };
        } else {
            if !seen_list_flag {
                return true;
            }
            index += 1;
        }
    }
    false
}

fn git_branch_is_dangerous(_: &str, arguments: &[String]) -> bool {
    const FLAGS_WITH_ARGUMENTS: &[&str] = &["--contains", "--no-contains", "--points-at", "--sort"];
    const FLAGS_WITH_OPTIONAL_ARGUMENTS: &[&str] = &["--merged", "--no-merged"];
    let mut index = 0;
    let mut last_flag = String::new();
    let mut seen_list_flag = false;
    let mut seen_double_dash = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if token.is_empty() {
            index += 1;
            continue;
        }
        if token == "--" && !seen_double_dash {
            seen_double_dash = true;
            last_flag.clear();
            index += 1;
            continue;
        }
        if !seen_double_dash && token.starts_with('-') {
            seen_list_flag |= is_list_flag(token);
            if let Some((flag, _)) = token.split_once('=') {
                last_flag = flag.to_owned();
                index += 1;
            } else {
                last_flag = token.clone();
                index += if FLAGS_WITH_ARGUMENTS.contains(&token.as_str()) {
                    2
                } else {
                    1
                };
            }
        } else {
            if !seen_list_flag && !FLAGS_WITH_OPTIONAL_ARGUMENTS.contains(&last_flag.as_str()) {
                return true;
            }
            index += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn config(command: &str) -> &'static CommandConfig {
        COMMAND_ALLOWLIST
            .iter()
            .find(|config| config.command == command)
            .unwrap()
    }

    #[test]
    fn spread_tables_resolve_flag_argument_types() {
        assert_eq!(config("git log").flag("-n"), Some(FlagArgument::Number));
        assert_eq!(
            config("git log").flag("--oneline"),
            Some(FlagArgument::NoValue)
        );
        assert_eq!(config("git diff").flag("-S"), Some(FlagArgument::Text));
        assert_eq!(config("git diff").flag("--output"), None);
        assert_eq!(config("xargs").flag("-I"), Some(FlagArgument::Braces));
        assert_eq!(config("xargs").flag("-i"), None);
        assert_eq!(config("tree").flag("-R"), None);
        assert!(!config("base64").respects_double_dash);
    }

    #[test]
    fn git_positional_callbacks_block_ref_creation_and_reflog_writes() {
        assert!(git_branch_is_dangerous("", &arguments(&["new-branch"])));
        assert!(git_branch_is_dangerous("", &arguments(&["--", "-l"])));
        assert!(!git_branch_is_dangerous(
            "",
            &arguments(&["-a", "--merged", "main"])
        ));
        assert!(!git_branch_is_dangerous(
            "",
            &arguments(&["--list", "feat/*"])
        ));
        assert!(git_tag_is_dangerous("", &arguments(&["v1.0"])));
        assert!(!git_tag_is_dangerous("", &arguments(&["-l", "v1.*"])));
        assert!(git_reflog_is_dangerous(
            "",
            &arguments(&["expire", "--all"])
        ));
        assert!(!git_reflog_is_dangerous("", &arguments(&["show", "HEAD"])));
        assert!(git_remote_is_dangerous(
            "",
            &arguments(&["add", "x", "url"])
        ));
        assert!(git_remote_show_is_dangerous("", &arguments(&["https://x"])));
        assert!(!git_remote_show_is_dangerous("", &arguments(&["origin"])));
    }

    #[test]
    fn system_command_callbacks_block_state_changes() {
        assert!(date_is_dangerous("", &arguments(&["0101000026"])));
        assert!(!date_is_dangerous("", &arguments(&["-u", "+%Y-%m-%d"])));
        assert!(tput_is_dangerous("", &arguments(&["reset"])));
        assert!(tput_is_dangerous("", &arguments(&["-xS"])));
        assert!(!tput_is_dangerous("", &arguments(&["cols"])));
    }
}
