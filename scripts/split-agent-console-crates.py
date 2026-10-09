#!/usr/bin/env python3
"""Split crates/agent into agent/{client,daemon,console,runtime,tickloop} (crate-layering phase 4).

Moves every module of crates/agent into the crate that owns it and rewrites each path that named
a moved module (`crate::runtime::X`, `tickloop_agent::terminal::X`,
`use crate::{agent_access::X, runtime::Y}` ...) to the owning crate. No alias is introduced:

| old module of crates/agent                                     | new crate                       |
| -------------------------------------------------------------- | ------------------------------- |
| `agent_access` (client root)                                   | `tickloop_agent_client`         |
| `agent_daemon` (daemon root)                                   | `tickloop_agent_daemon`         |
| `terminal`, `agent_console`, `monitor_rows`                    | `tickloop_agent_console::<same>` |
| `runtime` (runtime root) and every other library module        | `tickloop_agent_runtime[::<same>]` |
| `entry` (bin library root), `main.rs`, `tests/`, `examples/`   | `agent/tickloop` (`tickloop_agent`) |

Usage, from any directory:

    python3 scripts/split-agent-console-crates.py

The script is idempotent: moves are skipped once done and the rewrites only match the old paths,
so it can be re-run after rebasing onto a tree that still has the old layout (with `agent_access`
already holding only the client and `agent_daemon` the daemon side, as issue 10 step 6 left them).
It does not touch Git. Hand edits follow it: widening `pub(crate)` to `pub` where the compiler
reports an item used across the new crate boundaries.
"""

import re
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENT = REPO / "crates/agent"

CLIENT = "tickloop_agent_client"
DAEMON = "tickloop_agent_daemon"
CONSOLE = "tickloop_agent_console"
RUNTIME = "tickloop_agent_runtime"
BIN_LIBRARY = "tickloop_agent"

RUNTIME_MODULES = ["file_history", "host_tools", "hosted", "mcp_http", "mcp_tools", "native_host",
                   "tick_adapter", "tick_stream", "tick_tool_router", "transcript"]
CONSOLE_MODULES = ["agent_console", "monitor_rows", "terminal"]

# Old module of crates/agent -> (crate identifier, module path inside that crate).
MOVED_MODULES = {
    "agent_access": (CLIENT, ""),
    "agent_daemon": (DAEMON, ""),
    "runtime": (RUNTIME, ""),
    "entry": (BIN_LIBRARY, ""),
    **{module: (CONSOLE, module) for module in CONSOLE_MODULES},
    **{module: (RUNTIME, module) for module in RUNTIME_MODULES},
}

# (source relative to crates/agent, destination relative to the repo root)
MOVES = [
    ("src/agent_access/mod.rs", "agent/client/src/lib.rs"),
    ("src/agent_access", "agent/client/src"),
    # Reads saved Transcripts into wire records, so it stays beside the Transcript format.
    ("src/agent_daemon/records.rs", "agent/runtime/src/transcript/records.rs"),
    ("src/agent_daemon.rs", "agent/daemon/src/lib.rs"),
    ("src/agent_daemon", "agent/daemon/src"),
    ("src/terminal.rs", "agent/console/src/terminal.rs"),
    ("src/terminal", "agent/console/src/terminal"),
    ("src/agent_console.rs", "agent/console/src/agent_console.rs"),
    ("src/agent_console_tests.rs", "agent/console/src/agent_console_tests.rs"),
    ("src/monitor_rows", "agent/console/src/monitor_rows"),
    ("src/runtime.rs", "agent/runtime/src/lib.rs"),
    ("src/runtime", "agent/runtime/src"),
] + [
    (f"src/{module}{suffix}", f"agent/runtime/src/{module}{suffix}")
    for module in RUNTIME_MODULES
    for suffix in (".rs", "")
] + [
    ("src/entry.rs", "agent/tickloop/src/lib.rs"),
    ("src/main.rs", "agent/tickloop/src/main.rs"),
    ("tests", "agent/tickloop/tests"),
    ("examples", "agent/tickloop/examples"),
    ("README.md", "agent/tickloop/README.md"),
]
# Files of crates/agent that the new crates replace.
RETIRED = ["src/lib.rs", "Cargo.toml"]

# Child modules each new library root declares after the move, with their visibility.
ROOT_MODULES = {
    "agent/runtime/src/lib.rs": [
        ("pub", "file_history"), ("pub", "host_tools"), ("pub", "hosted"), ("", "mcp_http"),
        ("pub", "mcp_tools"), ("pub", "native_host"), ("pub", "tick_adapter"), ("pub", "tick_stream"),
        ("pub", "tick_tool_router"), ("pub", "transcript"),
    ],
    "agent/runtime/src/transcript.rs": [("pub", "records")],
}

CONSOLE_LIB = """\
//! The Agent Console: the terminal View of a daemon's Agents and the Monitor rows it computes.
//! It reaches every Agent through AgentAccess and never links the runtime.

pub mod agent_console;
pub mod monitor_rows;
pub mod terminal;
"""

# Paths in moved files that crossed into another crate or its directory.
MOVED_FILE_EDITS = {
    "agent/runtime/src/transcript/records.rs": [("crate::transcript::", "super::")],
    "agent/daemon/src/daemon_server.rs": [("super::records::", "tickloop_agent_runtime::transcript::records::")],
    "agent/daemon/src/embedded.rs": [("super::records::", "tickloop_agent_runtime::transcript::records::")],
    "agent/daemon/src/lib.rs": [("mod receipts;\nmod records;\n", "mod receipts;\n")],
    "agent/console/src/terminal.rs": [
        ('#[path = "../tests/support/common.rs"]', '#[path = "../../tickloop/tests/support/common.rs"]'),
    ],
    "agent/console/src/terminal/settings.rs": [
        ('"../../tests/support/legacy_models/', '"../../../tickloop/tests/support/legacy_models/'),
    ],
    "agent/tickloop/tests/all/main.rs": [("//! Every crates/agent integration test", "//! Every agent/tickloop integration test")],
    "agent/tickloop/tests/support/fixture_cli.rs": [
        ("so a test binary and a fixture agree only when one Cargo build produced both. The library\n"
         "/// covers its own dependencies; the dev-dependencies that export a type are listed one by one.",
         "so a test binary and a fixture agree only when one Cargo build produced both. Each library\n"
         "/// the executable links covers its own dependencies; the dev-dependencies that export a type are\n"
         "/// listed one by one."),
        ("            TypeId::of::<tickloop_agent_daemon::EmbeddedAgentAccess>(),\n",
         "            TypeId::of::<tickloop_agent_client::LocalAgentClient>(),\n"
         "            TypeId::of::<tickloop_agent_console::agent_console::AgentConsole>(),\n"
         "            TypeId::of::<tickloop_agent_daemon::EmbeddedAgentAccess>(),\n"),
    ],
}

WORKSPACE_DEPENDENCY = 'tickloop-agent-{name} = {{ path = "../{name}" }}'

MANIFESTS = {
    "agent/client": """\
[package]
name = "tickloop-agent-client"
description = "AgentAccess client: the Unix socket transport, recovery and starting the local daemon"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
async-trait = "0.1"
base64 = "0.22"
libc = "0.2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
shell-words = "1"
tempfile = "3"
tickloop-agent-protocol = { path = "../protocol" }
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
uuid = { version = "1", features = ["v4", "serde"] }
""",
    "agent/daemon": """\
[package]
name = "tickloop-agent-daemon"
description = "Agent daemon and worker processes: the AgentAccess server and the embedded Agent"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
@DAEMON_DEPENDENCIES@
""",
    "agent/console": """\
[package]
name = "tickloop-agent-console"
description = "Agent Console: the terminal View that reaches Agents through AgentAccess"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
@CONSOLE_DEPENDENCIES@
""",
    "agent/runtime": """\
[package]
name = "tickloop-agent-runtime"
description = "Agent runtime: Loops, Subagents, Transcripts, Tick streams and host tools"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
@RUNTIME_DEPENDENCIES@
""",
    "agent/tickloop": """\
[package]
name = "tickloop-agent"
description = "The tickloop executable: the Console, the daemon and worker entries, and their end-to-end tests"
version.workspace = true
edition.workspace = true
publish.workspace = true
default-run = "tickloop"
autobins = false
autotests = false

[lib]
doctest = false

[[bin]]
name = "tickloop"
path = "src/main.rs"

[[test]]
name = "all"
path = "tests/all/main.rs"

[[test]]
name = "cli_fixture"
path = "tests/support/cli_fixture_main.rs"
harness = false

[dependencies]
@BIN_DEPENDENCIES@
""",
}

IMAGE = 'image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }'
REQWEST = ('reqwest = { version = "0.12", default-features = false, '
           'features = ["json", "stream", "rustls-tls", "system-proxy"] }')
RMCP = ('rmcp = { version = "3.5.0", default-features = false, '
        'features = ["client", "transport-streamable-http-client"] }')
SERDE = 'serde = { version = "1", features = ["derive"] }'
TIME = 'time = { version = "0.3", features = ["formatting", "parsing"] }'
TOKIO = 'tokio = { version = "1", features = ["full"] }'
UUID = 'uuid = { version = "1", features = ["v4", "serde"] }'
# Tests of every Agent crate run with model's test seams and Tokio's paused clock.
TEST_DEPENDENCIES = [
    'tickloop-agent-model = { path = "../model", features = ["test-support"] }',
    'tickloop-agent-test-support = { path = "../test-support" }',
    'tokio = { version = "1", features = ["full", "test-util"] }',
]
DEPENDENCIES = {
    "@DAEMON_DEPENDENCIES@": [
        'async-trait = "0.1"', 'base64 = "0.22"', 'libc = "0.2"', SERDE, 'serde_json = "1"',
        'sha2 = "0.10"', 'tempfile = "3"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in
          ["artifacts", "client", "config", "model", "protocol", "runtime"]),
        'tickloop-tick-protocol = { path = "../../tick/protocol" }',
        'tickloop-tickhost = { path = "../../tickhost/host" }',
        'tickloop-tickhost-connect = { path = "../../tickhost/connect" }',
        TOKIO, 'tokio-util = "0.7"', UUID,
        "", "[dev-dependencies]", *TEST_DEPENDENCIES,
    ],
    "@CONSOLE_DEPENDENCIES@": [
        'arboard = { version = "3", features = ["wayland-data-control"] }', 'async-trait = "0.1"',
        'base64 = "0.22"', 'crossterm = "0.29"', 'futures-util = "0.3"', IMAGE, 'libc = "0.2"',
        'regex = "1"', REQWEST, SERDE, 'serde_json = "1"', 'sha2 = "0.10"', 'tempfile = "3"',
        'thiserror = "2"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in
          ["artifacts", "base", "client", "config", "model", "protocol", "tools"]),
        'tickloop-tick-protocol = { path = "../../tick/protocol" }',
        'tickloop-tickhost-connect = { path = "../../tickhost/connect" }',
        'tickloop-ui = { path = "../../ui" }',
        TIME, TOKIO, 'tokio-util = "0.7"', 'unicode-segmentation = "1"', 'unicode-width = "0.2"', UUID,
        "", "[dev-dependencies]", 'portable-pty = "0.9"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in ["daemon", "runtime"]),
        *TEST_DEPENDENCIES,
    ],
    "@RUNTIME_DEPENDENCIES@": [
        'async-trait = "0.1"', 'fastrand = "2"', 'futures-util = "0.3"', 'http = "1"',
        'icu_normalizer = "2"', IMAGE, 'jsonschema = { version = "0.56", default-features = false }',
        'libc = "0.2"', REQWEST,
        RMCP,
        SERDE, 'serde_json = "1"', 'sha2 = "0.10"', 'sse-stream = "0.2.4"', 'thiserror = "2"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in
          ["artifacts", "base", "config", "model", "protocol", "tools"]),
        'tickloop-tick-protocol = { path = "../../tick/protocol" }',
        'tickloop-tickhost = { path = "../../tickhost/host" }',
        'tickloop-tickhost-connect = { path = "../../tickhost/connect" }',
        TIME, TOKIO, 'tokio-util = "0.7"', UUID,
        "", "[dev-dependencies]", 'tempfile = "3"',
        ('tokio-tungstenite = { version = "0.27", default-features = false, '
         'features = ["connect", "rustls-tls-webpki-roots"] }'),
        *TEST_DEPENDENCIES,
    ],
    "@BIN_DEPENDENCIES@": [
        'crossterm = "0.29"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in ["client", "console", "daemon"]),
        TOKIO,
        "", "[dev-dependencies]", 'async-trait = "0.1"', 'base64 = "0.22"', 'futures-util = "0.3"',
        'icu_normalizer = "2"', IMAGE, 'jsonschema = { version = "0.56", default-features = false }',
        'libc = "0.2"', RMCP, 'percent-encoding = "2"', 'portable-pty = "0.9"', 'regex = "1"', REQWEST,
        SERDE, 'serde_json = "1"', 'serde_yaml_ng = "0.10"', 'sha2 = "0.10"', 'shell-words = "1"',
        'tempfile = "3"',
        *(WORKSPACE_DEPENDENCY.format(name=name) for name in
          ["artifacts", "base", "config", "protocol", "runtime", "tools"]),
        'tickloop-tick-protocol = { path = "../../tick/protocol" }',
        'tickloop-tickhost = { path = "../../tickhost/host" }',
        'tickloop-tickhost-connect = { path = "../../tickhost/connect" }',
        TIME, 'tokio-util = "0.7"', 'unicode-width = "0.2"', UUID, 'vt100 = "0.16.2"',
        *TEST_DEPENDENCIES,
    ],
}

USE_GROUP = re.compile(
    r"(?m)^(?P<attrs>(?:[ \t]*#\[[^\n]*\]\n)*)(?P<indent>[ \t]*)(?P<vis>pub(?:\([^)]*\))? )?"
    r"use (?P<root>crate|tickloop_agent)::\{"
)
MODULE_PATH = re.compile(
    r"(?<!\w)(?<!::)(?P<root>crate|tickloop_agent)::(?P<module>" + "|".join(MOVED_MODULES) + r")\b(?P<rest>::)?"
)

DOCUMENT_PATHS = [
    ("crates/agent/README.md", "agent/tickloop/README.md"),
    ("crates/agent/tests/", "agent/tickloop/tests/"),
    ("crates/agent/src/terminal/", "agent/console/src/terminal/"),
    ("crates/agent/src/terminal.rs", "agent/console/src/terminal.rs"),
    ("crates/agent/src/agent_console.rs", "agent/console/src/agent_console.rs"),
    ("crates/agent/src/runtime/", "agent/runtime/src/"),
    ("crates/agent/src/runtime.rs", "agent/runtime/src/lib.rs"),
] + [
    (f"crates/agent/src/{module}{suffix}", f"agent/runtime/src/{module}{suffix}")
    for module in RUNTIME_MODULES
    for suffix in (".rs", "/")
]
DOCUMENTS = [".tickloop/skills/model-integration/SKILL.md", "README.md", "docs/design/agent.md",
             "docs/design/image-input.md", "docs/design/agent-layout-integration.md",
             "docs/design/agent-ui.md", "docs/design/ui-theme.md", "docs/claude-code/adoption-proposal.md",
             "docs/agents/model-integration/model-card.md", "docs/agents/model-integration/acceptance.md"]


def move_files():
    for source, destination in MOVES:
        source_path = AGENT / source
        destination_path = REPO / destination
        if not source_path.exists():
            continue
        destination_path.parent.mkdir(parents=True, exist_ok=True)
        if source_path.is_dir():
            destination_path.mkdir(parents=True, exist_ok=True)
            for child in sorted(source_path.iterdir()):
                shutil.move(str(child), str(destination_path / child.name))
            source_path.rmdir()
        else:
            shutil.move(str(source_path), str(destination_path))
    for retired in RETIRED:
        path = AGENT / retired
        if path.exists():
            path.unlink()
    for directory in [AGENT / "src", AGENT]:
        if directory.is_dir() and not any(directory.iterdir()):
            directory.rmdir()


def write_if_changed(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists() or path.read_text() != text:
        path.write_text(text)


def write_crate_roots():
    for directory, manifest in MANIFESTS.items():
        for marker, dependencies in DEPENDENCIES.items():
            manifest = manifest.replace(marker, dependency_tables(dependencies))
        write_if_changed(REPO / directory / "Cargo.toml", manifest)
    console_lib = REPO / "agent/console/src/lib.rs"
    if not console_lib.exists():
        console_lib.write_text(CONSOLE_LIB)
    for lib, modules in ROOT_MODULES.items():
        add_child_modules(REPO / lib, modules)
    for lib in ["agent/client/src/lib.rs", "agent/daemon/src/lib.rs", "agent/runtime/src/lib.rs",
                "agent/tickloop/src/lib.rs"]:
        narrow_root_super_visibility(REPO / lib)
    workspace = REPO / "Cargo.toml"
    write_if_changed(workspace, workspace.read_text().replace('    "crates/agent",\n', ""))


def dependency_tables(lines):
    """The `[dependencies]` entries, then each later table, every table sorted by name."""
    tables, current = [], []
    for line in lines:
        if line.startswith("["):
            tables.append(sorted(current))
            current = [line]
        elif line:
            current.append(line)
    tables.append(current[:1] + sorted(current[1:]) if tables else sorted(current))
    return "\n\n".join("\n".join(table) for table in tables)


def narrow_root_super_visibility(path):
    """`pub(super)` in a former module file that became a crate root has no parent to name."""
    text = path.read_text()
    inline_module = re.search(r"(?m)^(?:pub(?:\([^)]*\))? )?mod \w+ \{", text)
    end = inline_module.start() if inline_module else len(text)
    write_if_changed(path, text[:end].replace("pub(super)", "pub(crate)") + text[end:])


def add_child_modules(lib, modules):
    text = lib.read_text()
    missing = [(visibility, module) for visibility, module in modules
               if not re.search(rf"(?m)^(?:pub(?:\([^)]*\))? )?mod {module};$", text)]
    if not missing:
        return
    first_module = re.search(r"(?m)^(?:pub(?:\([^)]*\))? )?mod \w+;$", text)
    declarations = "".join(f"{visibility + ' ' if visibility else ''}mod {module};\n"
                           for visibility, module in missing)
    lib.write_text(text[: first_module.start()] + declarations + text[first_module.start():])


def home_crate(path):
    """The library a file belongs to; `main.rs`, examples and integration tests name every
    library, their own included."""
    relative = path.relative_to(REPO).parts
    if relative[0] == "agent" and relative[2] == "src" and relative[-1] != "main.rs":
        return BIN_LIBRARY if relative[1] == "tickloop" else "tickloop_agent_" + relative[1].replace("-", "_")
    return None


def root_name(target, home):
    return "crate" if target == home else target


def matching_brace(text, open_index):
    depth = 0
    for index in range(open_index, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError("unbalanced use group")


def split_top_level(body):
    items, depth, current = [], 0, []
    for character in body:
        if character == "," and depth == 0:
            items.append("".join(current).strip())
            current = []
            continue
        depth += (character == "{") - (character == "}")
        current.append(character)
    items.append("".join(current).strip())
    return [item for item in items if item]


def relocate_item(item, home, crate_roots):
    """Returns (root, items) for one item of a `use crate::{...}` group."""
    module = re.match(r"\w+", item).group(0)
    if module not in MOVED_MODULES:
        return None
    target, inner = MOVED_MODULES[module]
    root = root_name(target, home)
    rest = item[len(module):]
    if inner:
        return root, [inner + rest]
    if rest.startswith("::{"):
        nested = split_top_level(rest[3:-1])
        if "self" in nested:
            crate_roots.add(module)
            nested.remove("self")
        return root, nested
    if rest:
        return root, [rest[2:]]
    crate_roots.add(module)
    return root, []


def rewrite_use_groups(text, home, crate_roots):
    output, position = [], 0
    for match in USE_GROUP.finditer(text):
        if match.start() < position:
            continue
        if match["root"] == "crate" and home is None:
            continue
        close = matching_brace(text, match.end() - 1)
        assert text[close + 1] == ";", text[match.start():close + 2]
        items = split_top_level(text[match.end():close])
        groups, changed = {}, False
        for item in items:
            relocated = relocate_item(item, home, crate_roots)
            root, new_items = relocated if relocated else (match["root"], [item])
            changed |= root != match["root"] or new_items != [item]
            groups.setdefault(root, []).extend(new_items)
        if not changed:
            continue
        statements = []
        for root in sorted(groups, key=lambda root: (root != "crate", root)):
            new_items = groups[root]
            if not new_items:
                continue
            tree = new_items[0] if len(new_items) == 1 else "{" + ", ".join(new_items) + "}"
            statements.append(f"{match['attrs']}{match['indent']}{match['vis'] or ''}use {root}::{tree};\n")
        output.append(text[position:match.start()])
        output.append("".join(statements))
        position = close + 2
        if text[position:position + 1] == "\n":
            position += 1
    output.append(text[position:])
    return "".join(output)


def rewrite_module_paths(text, home, crate_roots):
    def replace(match):
        if match["root"] == "crate" and home is None:
            return match.group(0)
        target, inner = MOVED_MODULES[match["module"]]
        root = root_name(target, home)
        if inner:
            return f"{root}::{inner}{match['rest'] or ''}"
        if match["rest"]:
            return f"{root}::"
        crate_roots.add(match["module"])
        return match.group(0)

    return MODULE_PATH.sub(replace, text)


def drop_crate_root_imports(text, home, crate_roots):
    """`use crate::runtime;` cannot name a crate root, so its uses are spelled out in full."""
    for module in crate_roots:
        target = root_name(MOVED_MODULES[module][0], home)
        text = re.sub(rf"(?m)^[ \t]*use (?:crate|tickloop_agent)::{module};\n", "", text)
        text = re.sub(rf"(?<!\w)(?<!::){module}::", f"{target}::", text)
    return text


def rewrite_file(path):
    text = path.read_text()
    home = home_crate(path)
    original = text
    for module, (target, inner) in MOVED_MODULES.items():
        if target == home and not inner:
            text = text.replace(f"pub(in crate::{module})", "pub(crate)")
            # libtest names of tests that re-execute themselves with `--exact`
            text = text.replace(f'"{module}::', '"')
    crate_roots = set()
    text = rewrite_use_groups(text, home, crate_roots)
    text = rewrite_module_paths(text, home, crate_roots)
    text = drop_crate_root_imports(text, home, crate_roots)
    for old, new in MOVED_FILE_EDITS.get(str(path.relative_to(REPO)), []):
        if new not in text:
            text = text.replace(old, new)
    if text != original:
        path.write_text(text)
    # Child modules reach the dropped import through `use super::*`.
    children = path.with_suffix("")
    if crate_roots and children.is_dir():
        for child in sorted(children.rglob("*.rs")):
            child_text = child.read_text()
            spelled = drop_crate_root_imports(child_text, home, crate_roots)
            if spelled != child_text:
                child.write_text(spelled)


def rewrite_documents():
    for document in DOCUMENTS:
        path = REPO / document
        text = path.read_text()
        for old, new in DOCUMENT_PATHS:
            text = text.replace(old, new)
        write_if_changed(path, text)


def main():
    move_files()
    write_crate_roots()
    for crate in ["client", "daemon", "console", "runtime", "tickloop"]:
        for path in sorted((REPO / "agent" / crate).rglob("*.rs")):
            rewrite_file(path)
    rewrite_documents()


if __name__ == "__main__":
    sys.exit(main())
