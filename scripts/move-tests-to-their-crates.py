#!/usr/bin/env python3
"""Move every integration test to the crate it tests (crate-layering phase 6).

- Splits `agent/tickloop/tests/all/*` by the code under test: lower-crate tests to model or
  config, transport and AgentAccess tests to daemon, Console tests to console, Loop and
  subscription tests to runtime. Tests that start the full executable or the cli_fixture stay in
  `agent/tickloop/tests`. Two files hold tests of several crates and are split item by item.
- Makes `agent/test-support` the one home of the shared test code: the scripted model and its
  helpers (the former `tests/support/common.rs`), PTY screens, input dialogs, legacy model
  files, `TRANSPORT_LIMITS` and `test_path!`. Unit tests of console and daemon use it instead of
  compiling `agent/tickloop/tests/support` through `#[path]` or keeping their own copies. Helpers
  only one crate uses go to that crate's `tests/all/support`.
- Merges the per-file test binaries of `tickhost/connect` and `ui` into one `all` binary.
- Gives each crate that receives tests one `tests/all.rs` with a `#[path]` module per file, and
  declares the dev-dependencies the moved tests use; dev-dependencies no test uses any more are
  dropped from agent/tickloop.

Usage, from any directory:

    python3 scripts/move-tests-to-their-crates.py && cargo fmt --all

Idempotent: moves are skipped once done and rewrites only match the old paths, so it can be
re-run after a rebase brings tests in the old place. A test file in agent/tickloop/tests/all
that no list below names stops the script, so every new test gets a deliberate home. It does not
touch Git.
"""

import re
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENT = REPO / "agent"
TICKLOOP = AGENT / "tickloop"
TICKLOOP_ALL = TICKLOOP / "tests/all"
TICKLOOP_SUPPORT = TICKLOOP / "tests/support"
TEST_SUPPORT = AGENT / "test-support"
TEST_SUPPORT_CRATE = "tickloop_agent_test_support"

# Test modules of agent/tickloop/tests/all by the crate under test.
DESTINATIONS = {
    "model": [
        "codex_auth", "codex_models", "google_models", "judgment", "minimax_models",
        "model_credentials", "model_messages", "model_protocols", "model_usage_observations",
        "openai_models", "provider_expansion",
    ],
    "config": ["model_configuration"],
    "daemon": [
        "agent_access", "agent_history_transport", "agent_message_daemon_diagnosis",
        "agent_titles", "busy_inputs", "mcp_tools", "native_host_tools", "permission_claims",
        "streaming_snapshot", "tick_tool_routes",
    ],
    "console": [
        "agent_console", "agent_family_navigation", "agent_messages", "artifact_auto_attachment",
        "delegation", "monitor_membership",
    ],
    "runtime": [
        "approval_assessment", "artifact_live_model", "artifact_tick_updates",
        "auto_configuration", "auto_fallback", "auto_reviewer_responses", "auto_rule_behavior",
        "auto_rules_management", "automation_live", "chat_branching", "chat_switching",
        "command_archive", "compaction_boundaries", "concurrent_chats", "context",
        "conversation", "conversation_local_messages", "copilot_models", "denial_history",
        "diff_review", "directed_summary", "execution_modes", "explicit_ask_scopes",
        "failed_replies", "file_tools", "glm_models", "goal_routing_live", "host_tools",
        "image_limits", "input_context", "interaction", "interleaved_context", "jev_auto",
        "loop_chain", "loop_trigger_safety", "mcp_approval", "model_error_retry",
        "model_image_capability", "model_live", "model_stream_replay", "module_boundaries_live",
        "native_observation", "observers", "output_limit_recovery", "permissions",
        "permissions_management", "plan_approval", "plans", "project_rule_freshness",
        "provider_error_security", "provider_errors", "queued_inputs", "read_file_images",
        "recovery", "request_usage", "rewind_file_safety", "rewind_points", "rewind_restore",
        "session_permission_branching", "shell_read_only", "shell_tools", "skills",
        "switching_http", "task_notification_runs", "temp_storage",
        "tick_subscriptions", "tool_results", "transcripts", "triggers",
        "trusted_approval_context", "unpersisted_state", "user_settings", "waiting_runs",
        "web_tools", "workspace_host_context",
    ],
}
# They start the tickloop executable (`CARGO_BIN_EXE_tickloop`) or the cli_fixture.
STAYS_IN_TICKLOOP = [
    "agent_console_pty", "agent_daemon", "agent_daemon_control", "agent_startup_errors",
    "artifact_terminal", "artifact_terminal_feedback", "codex_terminal",
    "command_consolidation", "hosted_bounds", "hosted_ticks", "monitor_attach", "monitor_popup",
    "monitor_pty", "provider_picker", "terminal", "workspace_trust_pty",
]

# Files whose tests belong to several crates. Each part names its imports and the top-level items
# (functions, types, impls by their type) it takes; a part without names keeps the file's imports
# and every item no other part takes. Without such a part, every item must be named once.
SPLITS = {
    "builtin_catalog": [
        ("config", """\
use serde_json::json;
use std::{fs, path::Path};
use tickloop_agent_config::UserSettings;
use tickloop_agent_model::cards::ModelCatalog;
""", [
            "builtin_selection_with_ignored_overrides",
            "builtin_selection_ignores_definition_and_tuning_files_with_one_notice",
            "unknown_external_selection_and_invalid_definition_contents_fall_back_without_mutation",
            "legacy_builtin_snapshot_keeps_identity_but_never_definition_parameters",
            "canonical_builtin_and_alias_selections_need_no_definition_file_or_notice",
        ]),
        ("daemon", """\
use tickloop_agent_model::cards::ModelCatalog;
use tickloop_agent_protocol::AgentAccess;
""", ["builtin_worker_refuses_context_tuning_even_without_a_settings_root"]),
        ("tickloop", """\
use crate::support;
use std::{fs, path::Path};
""", ["command", "startup_rejects_removed_config_argument_before_loading_its_file"]),
    ],
    "tick_adapters": [
        ("console", None, None),
        ("runtime", """\
use crate::support;
use serde_json::json;
use std::sync::Arc;
use tick_protocol::InstanceRef;
use tickloop_agent_runtime::{Agent, tick_adapter::TickAdapters};
use tickloop_agent_test_support::{ScriptedModel, config};
""", ["shell_subscription_preserves_its_policy_including_final_admission"]),
    ],
}

# The built-in definition the executable test used to read from agent/model/src.
STARTUP_DEFINITION = (
    'include_str!("../../../../agent/model/src/providers/deepseek-api.yaml")',
    '"id: deepseek-api\\nmodels:\\n  - id: deepseek-flash\\n"',
)

# Helpers of agent/tickloop/tests/support/mod.rs that move with their only users.
# `TRANSPORT_LIMITS` is defined by test-support, `CONNECTS_PER_WATCH` by the runtime itself.
SUPPORT_ITEMS = {
    "dropped": ["CONNECTS_PER_WATCH", "TRANSPORT_LIMITS"],
    "runtime": ["model_script_requests"],
}
SUPPORT_MODULES = {
    "runtime": ["provider_transport", "tick_admission_contract"],
}
TEST_SUPPORT_MODULES = ["input_dialog", "routed", "screen"]
LEGACY_MODELS = {
    "deepseek-config.json": "DEEPSEEK_CONFIG",
    "deepseek-responses-config.json": "DEEPSEEK_RESPONSES_CONFIG",
    "deepseek-chat-config.json": "DEEPSEEK_CHAT_CONFIG",
    "deepseek-v4-pro-config.json": "DEEPSEEK_V4_PRO_CONFIG",
}

# The connection budget of a watch, which the runtime enforces and two tests count, becomes a
# runtime constant instead of a test helper. A test that re-executes itself names its new path.
LITERAL_EDITS = {
    "agent/runtime/src/tick_stream.rs": [
        ("pub const RECONNECTS_PER_WATCH: u32 = 3;\n",
         "pub const RECONNECTS_PER_WATCH: usize = 3;\n"
         "/// The connections a watch makes at most: its first connect and every reconnect.\n"
         "pub const CONNECTS_PER_WATCH: usize = 1 + RECONNECTS_PER_WATCH;\n"),
        ("    Made(u32),", "    Made(usize),"),
        ("*made < 1 + RECONNECTS_PER_WATCH", "*made < CONNECTS_PER_WATCH"),
    ],
    "agent/runtime/tests/all/native_observation.rs": [
        ("use support::{CONNECTS_PER_WATCH, ScriptedModel,", "use support::{ScriptedModel,"),
        ("        NativeTickStream, RECONNECT_PAUSE,",
         "        CONNECTS_PER_WATCH, NativeTickStream, RECONNECT_PAUSE,"),
    ],
    "agent/tickloop/tests/all/hosted_ticks.rs": [
        ("    use super::support::{\n        CONNECTS_PER_WATCH,\n", "    use super::support::{\n"),
        ("            HostedTickStream, RECONNECT_PAUSE,",
         "            CONNECTS_PER_WATCH, HostedTickStream, RECONNECT_PAUSE,"),
    ],
    "tickhost/connect/tests/all/host_adapter.rs": [
        ('"--exact", "open_forwards_the_agents_locale_to_the_new_pane"',
         '"--exact", "host_adapter::open_forwards_the_agents_locale_to_the_new_pane"'),
    ],
}

# Crates outside agent/ whose per-file test binaries merge into one.
MERGED_TEST_CRATES = ["tickhost/connect", "ui"]

TEST_SUPPORT_MANIFEST = """\
[package]
name = "tickloop-agent-test-support"
description = "Test code shared by the Agent crates' tests: a scripted model, PTY screens and model HTTP servers"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
async-trait = "0.1"
regex = "1"
serde_json = "1"
# The tests of every Agent crate reach the model's test-only seams through this crate.
tickloop-agent-model = { path = "../model", features = ["test-support"] }
tickloop-agent-protocol = { path = "../protocol" }
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
unicode-width = "0.2"
uuid = { version = "1", features = ["v4", "serde"] }
"""

TEST_SUPPORT_HEADER = """\
//! Test code shared by the Agent crates' unit and integration tests: a scripted model with its
//! replies, runtime events, Transcript files, PTY screens, transport limits and model HTTP
//! servers. Helpers that only one crate's tests use stay in that crate.

pub mod input_dialog;
pub mod legacy_models;
pub mod model_http_support;
pub mod routed;
pub mod screen;

use async_trait::async_trait;
use std::{collections::VecDeque, sync::Mutex};
use tickloop_agent_model::*;
use tickloop_agent_protocol::{Content, InputModality, ModelConfig, Protocol, Role, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Small transport limits that let tests cross the frame and chunk boundaries with little data.
pub const TRANSPORT_LIMITS: tickloop_agent_protocol::AccessLimits =
    match tickloop_agent_protocol::AccessLimits::new(64 << 10, 16 << 10) {
        Ok(limits) => limits,
        Err(_) => panic!("the test AccessLimits are valid"),
    };

/// The libtest name of a test function of the calling test binary, for tests that re-execute
/// themselves with `--exact`; naming a function that does not exist fails to compile.
#[macro_export]
macro_rules! test_path {
    ($test:ident) => {{
        let _: fn() = $test;
        format!(
            "{}::{}",
            module_path!()
                .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
                .unwrap(),
            stringify!($test)
        )
    }};
}
"""

LEGACY_MODELS_HEADER = """\
//! Model files saved by earlier versions, which settings migrate into built-in selections.
"""

TICKLOOP_MAIN_HEADER = """\
//! The agent/tickloop integration tests: they start the tickloop executable or the cli_fixture,
//! compiled as one binary. Tests of a single crate live in that crate's `tests/all.rs`.

#[path = "../support/mod.rs"]
mod support;

"""

# Package and spec of each crate a test may name, for the dev-dependencies of its crate.
DEPENDENCY_SPECS = {
    "async_trait": 'async-trait = "0.1"',
    "base64": 'base64 = "0.22"',
    "futures_util": 'futures-util = "0.3"',
    "icu_normalizer": 'icu_normalizer = "2"',
    "image": 'image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }',
    "jsonschema": 'jsonschema = { version = "0.56", default-features = false }',
    "libc": 'libc = "0.2"',
    "percent_encoding": 'percent-encoding = "2"',
    "portable_pty": 'portable-pty = "0.9"',
    "regex": 'regex = "1"',
    "reqwest": 'reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "system-proxy"] }',
    "rmcp": 'rmcp = { version = "3.5.0", default-features = false, features = ["client", "transport-streamable-http-client"] }',
    "serde": 'serde = { version = "1", features = ["derive"] }',
    "serde_json": 'serde_json = "1"',
    "serde_yaml_ng": 'serde_yaml_ng = "0.10"',
    "sha2": 'sha2 = "0.10"',
    "shell_words": 'shell-words = "1"',
    "tempfile": 'tempfile = "3"',
    "tickloop_agent_artifacts": 'tickloop-agent-artifacts = { path = "../artifacts" }',
    "tickloop_agent_base": 'tickloop-agent-base = { path = "../base" }',
    "tickloop_agent_client": 'tickloop-agent-client = { path = "../client" }',
    "tickloop_agent_config": 'tickloop-agent-config = { path = "../config" }',
    "tickloop_agent_console": 'tickloop-agent-console = { path = "../console" }',
    "tickloop_agent_daemon": 'tickloop-agent-daemon = { path = "../daemon" }',
    "tickloop_agent_model": 'tickloop-agent-model = { path = "../model" }',
    "tickloop_agent_protocol": 'tickloop-agent-protocol = { path = "../protocol" }',
    "tickloop_agent_runtime": 'tickloop-agent-runtime = { path = "../runtime" }',
    "tickloop_agent_test_support": 'tickloop-agent-test-support = { path = "../test-support" }',
    "tickloop_agent_tools": 'tickloop-agent-tools = { path = "../tools" }',
    "tick_protocol": 'tickloop-tick-protocol = { path = "../../tick/protocol" }',
    "tickhost": 'tickloop-tickhost = { path = "../../tickhost/host" }',
    "tickhost_connect": 'tickloop-tickhost-connect = { path = "../../tickhost/connect" }',
    "time": 'time = { version = "0.3", features = ["formatting", "parsing"] }',
    "tokio": 'tokio = { version = "1", features = ["full", "test-util"] }',
    "tokio_tungstenite": 'tokio-tungstenite = { version = "0.27", default-features = false, features = ["connect", "rustls-tls-webpki-roots"] }',
    "tokio_util": 'tokio-util = "0.7"',
    "unicode_width": 'unicode-width = "0.2"',
    "uuid": 'uuid = { version = "1", features = ["v4", "serde"] }',
    "vt100": 'vt100 = "0.16.2"',
}

# The fixture's build identity names one type of each dev-dependency that exports one.
BUILD_IDENTITY_TYPES = {
    "percent_encoding": "TypeId::of::<percent_encoding::AsciiSet>(),",
    "serde_yaml_ng": "TypeId::of::<serde_yaml_ng::Value>(),",
    "tickloop_agent_test_support": f"TypeId::of::<{TEST_SUPPORT_CRATE}::ScriptedModel>(),",
}


def crate_directory(crate: str) -> Path:
    return TICKLOOP if crate == "tickloop" else AGENT / crate


def test_directory(crate: str) -> Path:
    return TICKLOOP_ALL if crate == "tickloop" else crate_directory(crate) / "tests/all"


# --- Rust source items ---------------------------------------------------------------------


RAW_STRING = re.compile(r'b?r(#*)"')
# Not preceded by a path (`a::name`) or an identifier; a single `:` (a JSON key) may precede.
NOT_A_PATH_TAIL = r"(?<![\w])(?<!::)"
# The entries of a `use path::{...};` list, nested at most one level.
USE_LIST = r"((?:[^{}]|\{[^{}]*\})*)"


def top_level_items(source: str) -> list[str]:
    """Splits a Rust file into its top-level items, each with its leading attributes and docs."""
    items, start, depth, index, length = [], 0, 0, 0, len(source)
    while index < length:
        char = source[index]
        if source.startswith("//", index):
            index = source.find("\n", index)
            index = length if index < 0 else index
            continue
        if source.startswith("/*", index):
            nesting, index = 1, index + 2
            while nesting:
                if source.startswith("/*", index):
                    nesting, index = nesting + 1, index + 2
                elif source.startswith("*/", index):
                    nesting, index = nesting - 1, index + 2
                else:
                    index += 1
            continue
        raw = RAW_STRING.match(source, index) if char in "br" else None
        if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            terminator = '"' + raw[1]
            index = source.index(terminator, index + raw.end()) + len(terminator)
            continue
        if char == '"':
            index += 1
            while source[index] != '"':
                index += 2 if source[index] == "\\" else 1
            index += 1
            continue
        if char == "'":
            if source[index + 1] == "\\":
                index = source.index("'", index + 3) + 1
            elif index + 2 < length and source[index + 2] == "'":
                index += 3
            else:
                index += 1
            continue
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                if re.match(r"\s*;", source[index + 1:index + 64]):
                    index = source.index(";", index)
                items.append(source[start:index + 1])
                start = index + 1
        elif char == ";" and depth == 0:
            items.append(source[start:index + 1])
            start = index + 1
        index += 1
    if source[start:].strip():
        sys.exit(f"unterminated item at the end of:\n{source[start:][:200]}")
    return items


def item_name(item: str) -> str:
    code = re.sub(r"(?m)^\s*(//.*|#\[.*\])$", "", item)
    impl = re.search(r"\bimpl\b(?:<[^>]*>)?\s+(?:[\w:]+\s+for\s+)?(\w+)", code)
    declared = re.search(
        r"\b(?:fn|struct|enum|const|static|mod|trait|type|macro_rules!)\s+(\w+)", code)
    if impl and (not declared or impl.start() < declared.start()):
        return impl[1]
    if re.match(r"\s*(pub\s+)?use\b", code):
        return "use"
    if not declared:
        sys.exit(f"cannot name the item:\n{item[:200]}")
    return declared[1]


def take_items(source: str, names: list[str]) -> tuple[str, str]:
    """Returns the named items and the rest of `source`."""
    taken, kept = [], []
    for item in top_level_items(source):
        (taken if item_name(item) in names else kept).append(item)
    missing = set(names) - {item_name(item) for item in taken}
    if missing:
        sys.exit(f"items not found: {sorted(missing)}")
    return "".join(taken), "".join(kept)


# --- Shared and crate-local support --------------------------------------------------------


def public_names(source: str) -> set[str]:
    names = set(re.findall(
        r"(?m)^pub (?:async )?(?:fn|struct|enum|const|static|mod|trait|type) (\w+)", source))
    return names | set(re.findall(r"(?m)^macro_rules! (\w+)", source))


def write_test_support() -> None:
    source = TEST_SUPPORT / "src"
    (TEST_SUPPORT / "Cargo.toml").write_text(TEST_SUPPORT_MANIFEST)
    common = TICKLOOP_SUPPORT / "common.rs"
    if common.exists():
        body = common.read_text()
        body = body[body.index("pub struct ScriptedModel"):]
        (source / "lib.rs").write_text(TEST_SUPPORT_HEADER + "\n" + body)
        common.unlink()
    for module in TEST_SUPPORT_MODULES:
        if (TICKLOOP_SUPPORT / f"{module}.rs").exists():
            shutil.move(TICKLOOP_SUPPORT / f"{module}.rs", source / f"{module}.rs")
    legacy = TICKLOOP_SUPPORT / "legacy_models"
    if legacy.exists():
        shutil.move(legacy, source / "legacy_models")
    (source / "legacy_models.rs").write_text(LEGACY_MODELS_HEADER + "\n" + "".join(
        f'pub const {name}: &str = include_str!("legacy_models/{file}");\n'
        for file, name in LEGACY_MODELS.items()))


def move_support_helpers() -> None:
    support_mod = TICKLOOP_SUPPORT / "mod.rs"
    source = support_mod.read_text()
    for crate, modules in SUPPORT_MODULES.items():
        directory = test_directory(crate) / "support"
        directory.mkdir(parents=True, exist_ok=True)
        for module in modules:
            if (TICKLOOP_SUPPORT / f"{module}.rs").exists():
                shutil.move(TICKLOOP_SUPPORT / f"{module}.rs", directory / f"{module}.rs")
            source = source.replace(f"pub mod {module};\n", "")
    for crate, names in SUPPORT_ITEMS.items():
        if not any(item_name(item) in names for item in top_level_items(source)):
            continue
        taken, source = take_items(source, names)
        if crate != "dropped":
            local = test_directory(crate) / "support/mod.rs"
            local.parent.mkdir(parents=True, exist_ok=True)
            local.write_text((local.read_text() if local.exists() else "") + taken)
    for module in TEST_SUPPORT_MODULES:
        source = source.replace(f"pub mod {module};\n", "")
    source = source.replace("mod common;\n", "").replace("pub use common::*;\n", "")
    support_mod.write_text(source)
    for crate, modules in SUPPORT_MODULES.items():
        local = test_directory(crate) / "support/mod.rs"
        existing = local.read_text() if local.exists() else ""
        declarations = "".join(f"pub mod {module};\n" for module in modules
                               if f"pub mod {module};" not in existing)
        local.write_text(declarations + existing)


def shared_names() -> set[str]:
    lib = (TEST_SUPPORT / "src/lib.rs").read_text()
    return public_names(lib) | {"test_path"}


def local_names(crate: str) -> set[str]:
    directory = TICKLOOP_SUPPORT if crate == "tickloop" else test_directory(crate) / "support"
    local = directory / "mod.rs"
    return public_names(local.read_text()) if local.exists() else set()


def use_list(path: str, entries: list[str]) -> str:
    if len(entries) == 1:
        return f"use {path}::{entries[0]};"
    return f"use {path}::{{{', '.join(entries)}}};"


def split_top_level(entries: str) -> list[str]:
    parts, depth, current = [], 0, ""
    for char in entries:
        depth += {"{": 1, "}": -1}.get(char, 0)
        if char == "," and depth == 0:
            parts.append(current.strip())
            current = ""
        else:
            current += char
    return [part for part in parts + [current.strip()] if part]


def reference_shared_support(source: str, shared: set[str], local: set[str], parent: str) -> str:
    """Points `support::X` (`super::X` in a support module) at test-support when X is shared."""
    owner = rf"{NOT_A_PATH_TAIL}(?:crate::|(?:super::)+)?{parent}"

    def rewrite_use(path: str, entries: list[str]) -> str:
        shared_entries = [entry for entry in entries if re.match(r"\w+", entry)[0] in shared]
        local_entries = [entry for entry in entries if entry not in shared_entries]
        statements = [use_list(TEST_SUPPORT_CRATE, shared_entries)] if shared_entries else []
        if local_entries:
            statements.append(use_list(path, local_entries))
        return "\n".join(statements)

    def rewrite_glob(match: re.Match) -> str:
        statements = [f"use {TEST_SUPPORT_CRATE}::*;"]
        used_local = sorted(
            name for name in local if re.search(rf"{NOT_A_PATH_TAIL}{name}\b", source))
        if used_local:
            statements.append(use_list(match[1], used_local))
        return "\n".join(statements)

    source = re.sub(rf"\buse ({owner})::\*;", rewrite_glob, source)
    source = re.sub(rf"\buse ({owner})::\{{{USE_LIST}\}};",
                    lambda match: rewrite_use(match[1], split_top_level(match[2])), source)
    source = re.sub(rf"\buse ({owner})::(\w+(?:::\w+)*(?: as \w+)?);",
                    lambda match: rewrite_use(match[1], [match[2]]), source)
    source = re.sub(rf"{owner}::(\w+)",
                    lambda match: f"{TEST_SUPPORT_CRATE}::{match[1]}"
                    if match[1] in shared else match[0], source)
    return re.sub(rf"{NOT_A_PATH_TAIL}test_path!", f"{TEST_SUPPORT_CRATE}::test_path!", source)


def drop_unused_support_import(source: str) -> str:
    rest = source.replace("use crate::support;\n", "")
    if re.search(rf"{NOT_A_PATH_TAIL}support::", rest):
        return source
    return rest


def reference_legacy_models(source: str) -> str:
    return re.sub(
        r'include_str!\(\s*"[^"]*legacy_models/([\w.-]+)"\s*,?\s*\)',
        lambda match: f"{TEST_SUPPORT_CRATE}::legacy_models::{LEGACY_MODELS[match[1]]}",
        source)


# --- Moving the tests ----------------------------------------------------------------------


def move_test_files() -> None:
    listed = {module for modules in DESTINATIONS.values() for module in modules}
    listed |= set(STAYS_IN_TICKLOOP) | set(SPLITS)
    present = {path.stem for path in TICKLOOP_ALL.glob("*.rs") if path.name != "main.rs"}
    if present - listed:
        sys.exit(f"name a crate for these tests first: {sorted(present - listed)}")
    for crate, modules in DESTINATIONS.items():
        target = test_directory(crate)
        target.mkdir(parents=True, exist_ok=True)
        for module in modules:
            source = TICKLOOP_ALL / f"{module}.rs"
            if not source.exists():
                continue
            if (TICKLOOP_ALL / module).is_dir() or (target / source.name).exists():
                sys.exit(f"move {source} by hand")
            shutil.move(source, target / source.name)


def split_test_files() -> None:
    for module, parts in SPLITS.items():
        source = TICKLOOP_ALL / f"{module}.rs"
        moved = [crate for crate, _, _ in parts if crate != "tickloop"]
        if not source.exists() or all(
                (test_directory(crate) / source.name).exists() for crate in moved):
            continue
        items = top_level_items(source.read_text())
        named = {name for _, _, names in parts if names for name in names}
        rest = [item for item in items if item_name(item) not in named]
        if all(names is not None for _, _, names in parts):
            unassigned = [item_name(item) for item in rest if item_name(item) != "use"]
            if unassigned:
                sys.exit(f"{source}: give these items a part: {unassigned}")
        if "tickloop" not in {crate for crate, _, _ in parts}:
            source.unlink()
        for crate, header, names in parts:
            if names is None:
                text = "".join(rest).strip() + "\n"
            else:
                taken = [item for item in items if item_name(item) in names]
                text = header + "\n" + "".join(taken).strip() + "\n"
            destination = test_directory(crate) / source.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(text)
    startup = TICKLOOP_ALL / "builtin_catalog.rs"
    startup.write_text(startup.read_text().replace(*STARTUP_DEFINITION))


def rewrite_test_sources() -> None:
    shared = shared_names()
    for crate in [*DESTINATIONS, "tickloop"]:
        local = local_names(crate)
        directory = test_directory(crate)
        sources = [path for path in directory.rglob("*.rs") if "support" not in path.parts]
        if crate == "tickloop":
            sources += list(TICKLOOP_SUPPORT.glob("*.rs"))
        for path in sources:
            source = path.read_text()
            rewritten = reference_shared_support(source, shared, local, "support")
            rewritten = drop_unused_support_import(reference_legacy_models(rewritten))
            if rewritten != source:
                path.write_text(rewritten)
        support = TICKLOOP_SUPPORT if crate == "tickloop" else directory / "support"
        for path in support.glob("*.rs"):
            source = path.read_text()
            rewritten = reference_shared_support(source, shared, local, "super")
            if rewritten != source:
                path.write_text(rewritten)


def apply_literal_edits() -> None:
    for file, replacements in LITERAL_EDITS.items():
        path = REPO / file
        source = path.read_text()
        for old, new in replacements:
            source = source.replace(old, new)
        path.write_text(source)


def merge_test_binaries() -> None:
    for crate in MERGED_TEST_CRATES:
        tests = REPO / crate / "tests"
        (tests / "all").mkdir(exist_ok=True)
        for source in sorted(tests.glob("*.rs")):
            if source.name != "all.rs":
                shutil.move(source, tests / "all" / source.name)


def write_all_binaries() -> None:
    for directory in [*(crate_directory(crate) for crate in DESTINATIONS),
                      *(REPO / crate for crate in MERGED_TEST_CRATES)]:
        all_directory = directory / "tests/all"
        modules = sorted(path.stem for path in all_directory.glob("*.rs"))
        declarations = []
        if (all_directory / "support/mod.rs").exists():
            declarations.append('#[path = "all/support/mod.rs"]\nmod support;\n')
        declarations += [f'#[path = "all/{module}.rs"]\nmod {module};\n' for module in modules]
        crate = directory.relative_to(REPO)
        (directory / "tests/all.rs").write_text(
            f"//! Every {crate} integration test, compiled as one binary.\n\n"
            + "".join(declarations))
    modules = sorted(path.stem for path in TICKLOOP_ALL.glob("*.rs") if path.name != "main.rs")
    (TICKLOOP_ALL / "main.rs").write_text(
        TICKLOOP_MAIN_HEADER + "".join(f"mod {module};\n" for module in modules))


# --- Unit tests of console and daemon ------------------------------------------------------

TRANSPORT_LIMITS_COPY = re.compile(
    r"/// Small limits that let unit tests[^\n]*\n#\[cfg\(test\)\]\n"
    r"pub\(crate\) const TRANSPORT_LIMITS[^;]*;\n\n?")


def without_entries(source: str, removed: str) -> str:
    """Drops `removed` (e.g. `test_support as support`) from every `use path::{...};` list."""

    def rewrite(match: re.Match) -> str:
        entries = split_top_level(match[2])
        kept = [entry for entry in entries if entry != removed]
        if kept == entries:
            return match[0]
        return use_list(match[1], kept) if kept else ""

    source = re.sub(rf"(?m)^\s*use [\w:]+::{removed};\n", "", source)
    return re.sub(rf"\buse ([\w:]+)::\{{{USE_LIST}\}};", rewrite, source)


def rewrite_unit_tests() -> None:
    for path in sorted((AGENT / "console/src").rglob("*.rs")):
        source = path.read_text()
        rewritten = re.sub(
            r'#\[cfg\(test\)\]\n#\[path = "[^"]*tests/support/common\.rs"\]\nmod test_support;\n',
            "", source)
        rewritten = TRANSPORT_LIMITS_COPY.sub("", rewritten)
        if "test_support as support" in rewritten:
            rewritten = without_entries(rewritten, "test_support as support")
            rewritten = re.sub(rf"{NOT_A_PATH_TAIL}support::", f"{TEST_SUPPORT_CRATE}::", rewritten)
        rewritten = without_entries(rewritten, "test_support")
        rewritten = re.sub(rf"{NOT_A_PATH_TAIL}(?:(?:crate::terminal|super)::)?test_support::",
                           f"{TEST_SUPPORT_CRATE}::", rewritten)
        rewritten = rewritten.replace("crate::terminal::TRANSPORT_LIMITS",
                                      f"{TEST_SUPPORT_CRATE}::TRANSPORT_LIMITS")
        rewritten = reference_legacy_models(rewritten)
        if rewritten != source:
            path.write_text(rewritten)
    for path in sorted((AGENT / "daemon/src").rglob("*.rs")):
        source = path.read_text()
        rewritten = TRANSPORT_LIMITS_COPY.sub("", source)
        rewritten = rewritten.replace("crate::TRANSPORT_LIMITS",
                                      f"{TEST_SUPPORT_CRATE}::TRANSPORT_LIMITS")
        if rewritten != source:
            path.write_text(rewritten)


# --- Manifests -----------------------------------------------------------------------------


def package_of(spec: str) -> str:
    return spec.split(" =", 1)[0]


def section_entries(manifest: str, section: str) -> tuple[re.Match | None, dict[str, str]]:
    match = re.search(rf"(?ms)^\[{re.escape(section)}\]\n(.*?)(?=^\[|\Z)", manifest)
    if not match:
        return None, {}
    lines = [line for line in match[1].splitlines() if line.strip()]
    return match, {package_of(line): line for line in lines}


def used_crates(sources: list[Path]) -> set[str]:
    text = "".join(path.read_text() for path in sources)
    used = {ident for ident in DEPENDENCY_SPECS
            if re.search(rf"{NOT_A_PATH_TAIL}(?:::)?{ident}(::|!)", text)
            or re.search(rf"\buse {ident}\b", text)}
    if re.search(r"start_paused|tokio::time::(pause|advance|resume)", text):
        used.add("tokio-test-util")
    return used


def declare_dev_dependencies(crate: str, prune: bool) -> set[str]:
    directory = crate_directory(crate)
    manifest_path = directory / "Cargo.toml"
    manifest = manifest_path.read_text()
    package = re.search(r'(?m)^name = "([^"]+)"', manifest)[1]
    sources = list((directory / "tests").rglob("*.rs"))
    used = used_crates(sources)
    _, normal = section_entries(manifest, "dependencies")
    dev_match, dev = section_entries(manifest, "dev-dependencies")
    needed = {}
    for ident in used - {"tokio-test-util"}:
        spec = DEPENDENCY_SPECS[ident]
        if package_of(spec) != package:
            needed[package_of(spec)] = spec
    if "tokio-test-util" in used:
        needed["tokio"] = DEPENDENCY_SPECS["tokio"]
    # test-support turns on the model's test seams for every crate that tests with it.
    if "test-support" in dev.get("tickloop-agent-model", ""):
        del dev["tickloop-agent-model"]
    for name, spec in needed.items():
        if name not in dev and (name not in normal or name == "tokio" and
                                "tokio-test-util" in used):
            dev[name] = spec
    if prune:
        dev = {name: line for name, line in dev.items() if name in needed}
    body = "".join(f"{dev[name]}\n" for name in sorted(dev))
    section = f"[dev-dependencies]\n{body}"
    if dev_match:
        rest = manifest[dev_match.end():]
        manifest = manifest[:dev_match.start()] + section + ("\n" if rest else "") + rest
    elif dev:
        manifest = manifest.rstrip("\n") + "\n\n" + section
    manifest_path.write_text(manifest)
    return set(dev)


def update_build_identity(dev_dependencies: set[str]) -> None:
    path = TICKLOOP_SUPPORT / "fixture_cli.rs"
    source = path.read_text()
    match = re.search(r"(?s)(\[\n)(\s*TypeId::of::<tickloop_agent_client.*?)(\n\s*\]\n)", source)
    lines = [line.strip() for line in match[2].splitlines()]
    product = [line for line in lines
               if re.match(r"TypeId::of::<tickloop_agent_(client|console|daemon)::", line)]
    others = [line for line in lines
              if line not in product and line not in BUILD_IDENTITY_TYPES.values()]
    others += [line for ident, line in BUILD_IDENTITY_TYPES.items()
               if package_of(DEPENDENCY_SPECS[ident]) in dev_dependencies]
    indent = re.match(r"\s*", match[2])[0]
    body = "\n".join(indent + line for line in product + sorted(others))
    path.write_text(source[:match.start(2)] + body + source[match.end(2):])


def main() -> None:
    write_test_support()
    move_support_helpers()
    move_test_files()
    split_test_files()
    merge_test_binaries()
    apply_literal_edits()
    rewrite_test_sources()
    rewrite_unit_tests()
    write_all_binaries()
    for crate in DESTINATIONS:
        declare_dev_dependencies(crate, prune=False)
    update_build_identity(declare_dev_dependencies("tickloop", prune=True))


if __name__ == "__main__":
    main()
