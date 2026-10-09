#!/usr/bin/env python3
"""Split the lower layers out of crates/agent (crate-layering phase 2).

Creates agent/{base,protocol,model,tools,config,test-support}, moves the modules into them and
rewrites every path that named a moved module (`crate::model::X`, `tickloop_agent::tools::X`,
`use crate::{model::X, runtime::Y}` ...) to the owning crate. No alias is introduced.

Usage, from any directory:

    python3 scripts/split-lower-agent-crates.py

The script is idempotent: moves are skipped once done and the rewrites only match the old paths,
so it can be re-run after rebasing onto a tree that still has the old layout. It does not touch
Git. Hand edits follow it: widening `pub(crate)` to `pub` where the compiler reports an item used
across the new crate boundaries, and putting model's test seams behind its `test-support` feature.
"""

import re
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENT = REPO / "crates/agent"
AGENT_SRC = AGENT / "src"

# Old module of crates/agent -> (crate identifier, module path inside that crate).
MOVED_MODULES = {
    "private_fs": ("tickloop_agent_base", "private_fs"),
    "network": ("tickloop_agent_base", "network"),
    "image": ("tickloop_agent_base", "image"),
    "output_style": ("tickloop_agent_base", "output_style"),
    "protocol": ("tickloop_agent_protocol", ""),
    "model": ("tickloop_agent_model", ""),
    "auth": ("tickloop_agent_model", "auth"),
    "judgment": ("tickloop_agent_model", "judgment"),
    "tools": ("tickloop_agent_tools", ""),
    "permissions": ("tickloop_agent_tools", "permissions"),
    "file_change": ("tickloop_agent_tools", "file_change"),
    "settings": ("tickloop_agent_config", ""),
}
TEST_SUPPORT_CRATE = "tickloop_agent_test_support"

# (source relative to crates/agent/src or crates/agent, destination relative to the repo root)
MOVES = [
    ("src/private_fs.rs", "agent/base/src/private_fs.rs"),
    ("src/network.rs", "agent/base/src/network.rs"),
    ("src/image.rs", "agent/base/src/image.rs"),
    ("src/output_style.rs", "agent/base/src/output_style.rs"),
    ("src/protocol/mod.rs", "agent/protocol/src/lib.rs"),
    ("src/protocol", "agent/protocol/src"),
    ("src/model.rs", "agent/model/src/lib.rs"),
    ("src/model", "agent/model/src"),
    ("src/auth.rs", "agent/model/src/auth.rs"),
    ("src/auth", "agent/model/src/auth"),
    ("src/judgment.rs", "agent/model/src/judgment.rs"),
    ("tests/fixtures/google-test-private-key.pem", "agent/model/tests/fixtures/google-test-private-key.pem"),
    ("src/tools.rs", "agent/tools/src/lib.rs"),
    ("src/tools", "agent/tools/src"),
    ("src/permissions.rs", "agent/tools/src/permissions.rs"),
    ("src/file_change.rs", "agent/tools/src/file_change.rs"),
    ("src/settings.rs", "agent/config/src/lib.rs"),
    ("tests/model_http_support/mod.rs", "agent/test-support/src/model_http_support.rs"),
] + [
    (f"examples/{probe}_probe.rs", f"agent/model/examples/{probe}_probe.rs")
    for probe in ["copilot", "glm", "minimax", "openai", "provider"]
]

# Documentation paths of moved files, outside .scratch (history keeps its old paths).
DOCUMENT_PATHS = [
    ("crates/agent/src/model.rs", "agent/model/src/lib.rs"),
    ("crates/agent/src/model/", "agent/model/src/"),
    ("crates/agent/src/auth/", "agent/model/src/auth/"),
    ("crates/agent/src/tools.rs", "agent/tools/src/lib.rs"),
    ("crates/agent/src/tools/", "agent/tools/src/"),
    ("crates/agent/src/settings.rs", "agent/config/src/lib.rs"),
    ("](src/model/", "](../../agent/model/src/"),
    ("[src/model/", "[agent/model/src/"),
    ("-p tickloop-agent --example", "-p tickloop-agent-model --example"),
    ("crates/agent/examples/minimax_probe.rs", "agent/model/examples/minimax_probe.rs"),
]
DOCUMENTS = [".tickloop/skills/model-integration/SKILL.md", "crates/agent/README.md", "docs/design/agent.md",
             "docs/design/codex-subscription.md", "docs/agents/model-integration/model-card.md",
             "docs/agents/model-integration/acceptance.md",
             "docs/agents/model-integration/provider-connections.md"]
INCLUDE_PATHS = [
    ('"../../src/model/providers/', '"../../../../agent/model/src/providers/'),
]

BASE_LIB = """\
//! Leaf utilities shared by every Agent crate: private files, HTTP policy, images and output styles.

pub mod image;
pub mod network;
pub mod output_style;
pub mod private_fs;
"""

TEST_SUPPORT_LIB = """\
//! Test fixtures shared by the Agent crates' tests; nothing here depends on an Agent crate.

pub mod model_http_support;
"""

MANIFESTS = {
    "agent/base": """\
[package]
name = "tickloop-agent-base"
description = "Leaf utilities of the Agent: private files, HTTP policy, images and output styles"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
base64 = "0.22"
hyper-util = { version = "0.1", features = ["client-proxy"] }
image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }
libc = "0.2"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "system-proxy"] }
serde = { version = "1", features = ["derive"] }
thiserror = "2"
time = { version = "0.3", features = ["formatting", "parsing"] }
uuid = { version = "1", features = ["v4", "serde"] }

[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full"] }
""",
    "agent/protocol": """\
[package]
name = "tickloop-agent-protocol"
description = "Agent domain types and AgentAccess wire definitions shared by every Agent layer"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
uuid = { version = "1", features = ["v4", "serde"] }
""",
    "agent/model": """\
[package]
name = "tickloop-agent-model"
description = "Agent model clients, provider sign-in and typed judgments"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
async-trait = "0.1"
base64 = "0.22"
futures-util = "0.3"
htmd = "0.5"
http = "1"
icu_normalizer = "2"
icu_properties = "2"
libc = "0.2"
percent-encoding = "2"
regex = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "system-proxy"] }
ring = "0.17"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml_ng = "0.10"
sha2 = "0.10"
tempfile = { version = "3", optional = true }
thiserror = "2"
tickloop-agent-base = { path = "../base" }
tickloop-agent-protocol = { path = "../protocol" }
time = { version = "0.3", features = ["formatting", "parsing"] }
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = { version = "0.27", default-features = false, features = ["connect", "rustls-tls-webpki-roots"] }
tokio-util = "0.7"
uuid = { version = "1", features = ["v4", "serde"] }
web_atoms = "0.2"

[dev-dependencies]
hyper-util = { version = "0.1", features = ["client-proxy"] }
image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }
tempfile = "3"
tickloop-agent-test-support = { path = "../test-support" }
tokio = { version = "1", features = ["full", "test-util"] }

[features]
# Exposes test-only seams (loopback Codex clients, fixture titles, test endpoints) to the tests of
# the Agent crates above; never enabled by a build.
test-support = ["dep:tempfile"]
""",
    "agent/tools": """\
[package]
name = "tickloop-agent-tools"
description = "Agent tools, permission rules and file change records"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
async-trait = "0.1"
grep-printer = "0.3"
grep-regex = "0.1"
grep-searcher = "0.1"
htmd = "0.5"
ignore = "0.4"
libc = "0.2"
regex = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "system-proxy"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
similar = "2.7"
thiserror = "2"
tickloop-agent-base = { path = "../base" }
tickloop-agent-protocol = { path = "../protocol" }
time = { version = "0.3", features = ["formatting", "parsing"] }
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
uuid = { version = "1", features = ["v4", "serde"] }

[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full", "test-util"] }
""",
    "agent/config": """\
[package]
name = "tickloop-agent-config"
description = "Agent user settings"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "2"
tickloop-agent-base = { path = "../base" }
tickloop-agent-model = { path = "../model" }
tickloop-agent-tools = { path = "../tools" }
uuid = { version = "1", features = ["v4", "serde"] }

[dev-dependencies]
tempfile = "3"
time = { version = "0.3", features = ["formatting", "parsing"] }
""",
    "agent/test-support": """\
[package]
name = "tickloop-agent-test-support"
description = "Test fixtures shared by the Agent crates' tests"
version.workspace = true
edition.workspace = true
publish.workspace = true

[lib]
doctest = false

[dependencies]
serde_json = "1"
tokio = { version = "1", features = ["full"] }
""",
}

AGENT_DEPENDENCIES = [
    'tickloop-agent-base = { path = "../../agent/base" }',
    'tickloop-agent-config = { path = "../../agent/config" }',
    'tickloop-agent-model = { path = "../../agent/model" }',
    'tickloop-agent-protocol = { path = "../../agent/protocol" }',
    'tickloop-agent-tools = { path = "../../agent/tools" }',
]
AGENT_DEV_DEPENDENCIES = [
    'percent-encoding = "2"',
    'serde_yaml_ng = "0.10"',
    'tickloop-agent-model = { path = "../../agent/model", features = ["test-support"] }',
    'tickloop-agent-test-support = { path = "../../agent/test-support" }',
]
# External crates only the moved modules used; grep-matcher was already unused.
AGENT_UNUSED_DEPENDENCIES = ["grep-matcher", "grep-printer", "grep-regex", "grep-searcher", "htmd",
                             "hyper-util", "icu_properties", "ignore", "percent-encoding", "ring",
                             "serde_yaml_ng", "similar", "web_atoms"]

USE_GROUP = re.compile(
    r"(?m)^(?P<attrs>(?:[ \t]*#\[[^\n]*\]\n)*)(?P<indent>[ \t]*)(?P<vis>pub(?:\([^)]*\))? )?"
    r"use (?P<root>crate|tickloop_agent)::\{"
)
MODULE_PATH = re.compile(
    r"(?<!\w)(?<!::)(?:crate|tickloop_agent)::(?P<module>" + "|".join(MOVED_MODULES) + r")\b(?P<rest>::)?"
)


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
    for leftover in ["tests/model_http_support", "tests/fixtures"]:
        directory = AGENT / leftover
        if directory.is_dir() and not any(directory.iterdir()):
            directory.rmdir()


def write_if_changed(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists() or path.read_text() != text:
        path.write_text(text)


def write_crate_roots():
    for directory, manifest in MANIFESTS.items():
        write_if_changed(REPO / directory / "Cargo.toml", manifest)
    write_if_changed(REPO / "agent/base/src/lib.rs", BASE_LIB)
    write_if_changed(REPO / "agent/test-support/src/lib.rs", TEST_SUPPORT_LIB)
    add_child_modules(REPO / "agent/model/src/lib.rs", ["auth", "judgment"])
    add_child_modules(REPO / "agent/tools/src/lib.rs", ["file_change", "permissions"])
    narrow_root_super_visibility()
    model_lib = REPO / "agent/model/src/lib.rs"
    write_if_changed(
        model_lib,
        model_lib.read_text().replace(
            '#[cfg(test)]\n#[path = "../tests/model_http_support/mod.rs"]\npub(crate) mod model_http_support;\n', ""
        ),
    )


def narrow_root_super_visibility():
    """`pub(super)` in a former module file that became a crate root has no parent to name."""
    for lib in ["agent/protocol/src/lib.rs", "agent/model/src/lib.rs", "agent/tools/src/lib.rs",
                "agent/config/src/lib.rs"]:
        path = REPO / lib
        text = path.read_text()
        inline_module = re.search(r"(?m)^(?:pub(?:\([^)]*\))? )?mod \w+ \{", text)
        end = inline_module.start() if inline_module else len(text)
        write_if_changed(path, text[:end].replace("pub(super)", "pub(crate)") + text[end:])


def add_child_modules(lib, modules):
    text = lib.read_text()
    missing = [module for module in modules if not re.search(rf"(?m)^pub mod {module};$", text)]
    if not missing:
        return
    first_module = re.search(r"(?m)^(?:pub(?:\([^)]*\))? )?mod \w+;$", text)
    declarations = "".join(f"pub mod {module};\n" for module in missing)
    lib.write_text(text[: first_module.start()] + declarations + text[first_module.start():])


def update_agent_crate():
    lib = AGENT_SRC / "lib.rs"
    text = lib.read_text()
    for module in MOVED_MODULES:
        text = re.sub(rf"(?m)^(?:pub(?:\([^)]*\))? )?mod {module};\n", "", text)
    write_if_changed(lib, text)

    main = AGENT / "tests/all/main.rs"
    write_if_changed(
        main, main.read_text().replace('#[path = "../model_http_support/mod.rs"]\nmod model_http_support;\n', "")
    )

    manifest = AGENT / "Cargo.toml"
    text = manifest.read_text()
    for dependency in AGENT_UNUSED_DEPENDENCIES:
        text = re.sub(rf"(?m)^{re.escape(dependency)} = .*\n", "", text)
    text = add_dependency_lines(text, "[dependencies]", AGENT_DEPENDENCIES)
    text = add_dependency_lines(text, "[dev-dependencies]", AGENT_DEV_DEPENDENCIES)
    write_if_changed(manifest, text)

    workspace = REPO / "Cargo.toml"
    text = workspace.read_text()
    if '"agent/*"' not in text:
        text = text.replace('    "crates/agent",\n', '    "crates/agent",\n    "agent/*",\n')
    write_if_changed(workspace, text)


def add_dependency_lines(manifest, table, lines):
    start = manifest.index(table + "\n") + len(table) + 1
    end_match = re.compile(r"(?m)^\[").search(manifest, start)
    end = end_match.start() if end_match else len(manifest)
    section = manifest[start:end]
    for line in lines:
        name = line.split(" = ", 1)[0]
        section = re.sub(rf"(?m)^{re.escape(name)} = .*\n", "", section)
    entries = [entry for entry in section.split("\n") if entry.strip()] + lines
    entries.sort(key=lambda entry: entry.split(" = ", 1)[0])
    trailing = "\n\n" if end_match else "\n"
    return manifest[:start] + "\n".join(entries) + trailing + manifest[end:]


def home_crate(path):
    """The library a file belongs to; examples and integration tests use every library by name."""
    relative = path.relative_to(REPO).parts
    if relative[0] == "agent":
        return "tickloop_agent_" + relative[1].replace("-", "_") if relative[2] == "src" else None
    return "tickloop_agent"


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
    root = "crate" if target == home else target
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
        for root in sorted(groups, key=lambda root: (root not in ("crate", "tickloop_agent"), root)):
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
        target, inner = MOVED_MODULES[match["module"]]
        root = "crate" if target == home else target
        if inner:
            return f"{root}::{inner}{match['rest'] or ''}"
        if match["rest"]:
            return f"{root}::"
        crate_roots.add(match["module"])
        return match.group(0)

    return MODULE_PATH.sub(replace, text)


def drop_crate_root_imports(text, crate_roots):
    """`use crate::model;` cannot name a crate root, so its uses are spelled out in full."""
    for module in crate_roots:
        target = MOVED_MODULES[module][0]
        text = re.sub(rf"(?m)^[ \t]*use (?:crate|tickloop_agent)::{module};\n", "", text)
        text = re.sub(rf"(?<!\w)(?<!::){module}::", f"{target}::", text)
    return text


def rewrite_file(path):
    text = path.read_text()
    home = home_crate(path)
    original = text
    text = re.sub(
        r"(?<!\w)(?<!::)(?:crate::model::|crate::)model_http_support\b", f"{TEST_SUPPORT_CRATE}::model_http_support", text
    )
    for module, (target, inner) in MOVED_MODULES.items():
        if target == home and not inner:
            text = text.replace(f"pub(in crate::{module})", "pub(crate)")
            # libtest names of tests that re-execute themselves with `--exact`
            text = text.replace(f'"{module}::', '"')
    crate_roots = set()
    text = rewrite_use_groups(text, home, crate_roots)
    text = rewrite_module_paths(text, home, crate_roots)
    text = drop_crate_root_imports(text, crate_roots)
    for old, new in INCLUDE_PATHS:
        text = text.replace(old, new)
    if text != original:
        path.write_text(text)
        if crate_roots:
            print(f"{path.relative_to(REPO)}: spelled out crate-root paths for {sorted(crate_roots)}")


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
    update_agent_crate()
    roots = [AGENT / "src", AGENT / "tests"] + [REPO / "agent" / name for name in
                                                ["base", "protocol", "model", "tools", "config"]]
    for root in roots:
        for path in sorted(root.rglob("*.rs")):
            rewrite_file(path)
    rewrite_documents()


if __name__ == "__main__":
    sys.exit(main())
