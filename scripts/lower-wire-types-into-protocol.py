#!/usr/bin/env python3
"""Rewrite the paths of the wire types lowered from agent/{model,tools,config} into agent/protocol
(crate-layering ticket 09b).

The 54 types themselves were moved by hand into `agent/protocol/src`; this script rewrites every
path that still names them through their old crate:

- `use tickloop_agent_model::{Message, ModelClient, cards::{ModelCard, model_card}};` becomes
  `use tickloop_agent_model::{ModelClient, cards::model_card};` plus
  `use tickloop_agent_protocol::{Message, ModelCard};`, attributes such as `#[cfg(test)]` copied;
- inline paths such as `tickloop_agent_tools::permissions::Decision::Allow` become
  `tickloop_agent_protocol::Decision::Allow`;
- a glob such as `use tickloop_agent_model::*;` gains an explicit protocol import of the moved
  names the file mentions;
- inside the crate that used to own a type, `crate::cards::ModelCard`, `cards::ModelCard` and a
  top-level `use super::ModelCard` (in a file directly under `src/`) become `ModelCard` with a
  top-level `use tickloop_agent_protocol::ModelCard;`.

Top-level protocol imports of a file are merged into one statement; run `cargo fmt --all`
afterwards. Usage, from any directory:

    python3 scripts/lower-wire-types-into-protocol.py

The script is idempotent: it only matches the old paths, so a second run changes nothing. It does
not touch Git. Hand edits follow it: imports of names a file used unqualified because it defined
them, and the `use` of the extension traits (`ModelConfigExt`, `PermissionPolicyExt`, ...) that
now carry the behaviour.
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PROTOCOL = "tickloop_agent_protocol"

# Owning crate -> module path inside it (as a tuple, () for the crate root) -> moved type names.
MOVED = {
    "tickloop_agent_model": {
        (): ["Content", "ImageSource", "InputModality", "Message", "ModelAuthSource",
             "ModelConfig", "ModelIdentity", "ModelProvider", "PrivateHistorySource", "Protocol",
             "ReasoningEffort", "Role", "ToolResultBlock", "ToolResultContent", "Usage",
             "ProviderFailureClass", "ProviderHttpFailure"],
        ("availability",): ["AccountModelDirectory", "DirectorySemantics", "ModelAvailability"],
        ("cards",): ["Adapter", "ModelCard", "ModelPatch", "ProviderPatch", "ThinkingBudgets",
                     "ThinkingControl"],
        ("connection",): ["ConnectionRegion", "ModelDiscovery", "ProductKind",
                          "ProviderConnection"],
        ("credentials",): ["ApiKeyStatus"],
        ("provider_failure",): ["ProviderFailureClass", "ProviderHttpFailure"],
    },
    "tickloop_agent_tools": {
        (): ["ToolResult", "FileReadRecord", "CommandSnapshot", "CommandState", "SearchConfig"],
        ("permissions",): ["Decision", "ExecutionMode", "PermissionPolicy", "PermissionRule",
                           "PermissionScope"],
        ("file_change",): ["FileChangeAction", "FileChangeArtifactRef", "FileChangeReviewSummary",
                           "FileIdentity", "FileVersion", "PreparedFileChange"],
        ("read",): ["FileReadRecord"],
        ("shell",): ["CommandSnapshot", "CommandState"],
        ("web",): ["SearchConfig"],
    },
    "tickloop_agent_config": {
        (): ["AutoReviewerPreference", "AutoRuleChange", "AutoRuleGroupSettings", "AutoRuleKind",
             "AutoRuleSettings", "AutoSettingsRevision", "CustomAutoRule"],
    },
}
OWNING_DIRECTORIES = {
    "agent/model": "tickloop_agent_model",
    "agent/tools": "tickloop_agent_tools",
    "agent/config": "tickloop_agent_config",
}
SOURCE_ROOTS = ["crates/agent/src", "crates/agent/tests", "agent/model", "agent/tools",
                "agent/config"]

USE_STATEMENT = re.compile(
    r"^(?P<indent>[ \t]*)(?P<vis>pub(?:\([^)]*\))?\s+)?use\s+(?P<tree>[^;]+);", re.MULTILINE)
TOKEN = re.compile(r"::|[{},*]|\w+")
INLINE_PATH = re.compile(r"(?<!::)(?<!\w)(?P<root>tickloop_agent_(?:model|tools|config)|crate)"
                         r"(?P<rest>(?:::\w+)+)")


def moved_name(crate, segments):
    """The moved type a leaf path (below `crate`) names, or None."""
    if not segments or crate not in MOVED:
        return None
    name, module = segments[-1], tuple(segments[:-1])
    return name if name in MOVED[crate].get(module, []) else None


def parse_tree(tokens):
    """Flattens a use tree into leaves of (segments, alias)."""
    position = 0

    def subtree(prefix):
        nonlocal position
        leaves = []
        if tokens[position] == "{":
            position += 1
            while tokens[position] != "}":
                leaves += subtree(prefix)
                if tokens[position] == ",":
                    position += 1
            position += 1
            return leaves
        if tokens[position] == "*":
            position += 1
            return [(prefix + ["*"], None)]
        segments = prefix + [tokens[position]]
        position += 1
        while position < len(tokens) and tokens[position] == "::":
            position += 1
            if tokens[position] in ("{", "*"):
                return subtree(segments)
            segments.append(tokens[position])
            position += 1
        alias = None
        if position < len(tokens) and tokens[position] == "as":
            alias = tokens[position + 1]
            position += 2
        return [(segments, alias)]

    leaves = subtree([])
    if position != len(tokens):
        raise ValueError(f"unparsed use tree tokens: {tokens[position:]}")
    return leaves


def tree_text(leaves):
    """Renders leaves as a use tree, nesting the ones that share a module as rustfmt writes them."""
    texts, modules = [], {}
    for segments, alias in leaves:
        if len(segments) == 1:
            texts.append(segments[0] + (f" as {alias}" if alias else ""))
        else:
            modules.setdefault(segments[0], []).append((segments[1:], alias))
    texts += [f"{module}::{tree_text(children)}" for module, children in modules.items()]
    return texts[0] if len(texts) == 1 and texts[0] != "self" else "{" + ", ".join(texts) + "}"


def use_statement(indent, visibility, root, leaves):
    return f"{indent}{visibility}use {root}::{tree_text(leaves)};"


def preceding_attributes(text, start, indent):
    lines = text[:start].split("\n")[:-1]
    attributes = []
    while lines and lines[-1].startswith(indent + "#[") and lines[-1].endswith("]"):
        attributes.insert(0, lines.pop())
    return attributes


def crate_of_root(root, owning_crate, names_crate_root_by_super):
    if root.startswith("tickloop_agent_"):
        return root
    if root == "crate" or (root == "super" and names_crate_root_by_super):
        return owning_crate
    return None


def protocol_names_imported_at(text, indent):
    return {
        segments[-1]
        for match in USE_STATEMENT.finditer(text)
        if match["indent"] == indent and TOKEN.findall(match["tree"])[0] == PROTOCOL
        for segments, alias in parse_tree(TOKEN.findall(match["tree"])[2:])
    }


def rewrite_uses(text, owning_crate, is_root_level_file, top_level_imports):
    def replace(match):
        tokens = TOKEN.findall(match["tree"])
        root = tokens[0]
        indent = match["indent"]
        crate = crate_of_root(root, owning_crate, is_root_level_file and indent == "")
        if crate is None or root == PROTOCOL or len(tokens) < 3 or tokens[1] != "::":
            return match[0]
        leaves = parse_tree(tokens[2:])
        kept, moved = [], []
        for segments, alias in leaves:
            name = moved_name(crate, segments)
            if name:
                moved.append(([name], alias))
            elif segments[-1] == "*" and root.startswith("tickloop_agent_"):
                imported = protocol_names_imported_at(text, indent)
                mentioned = [
                    name for name in MOVED[crate].get(tuple(segments[:-1]), [])
                    if name not in imported and re.search(rf"(?<!::)\b{name}\b", text)
                ]
                kept.append((segments, alias))
                moved += [([name], None) for name in mentioned]
            else:
                kept.append((segments, alias))
        if not moved:
            return match[0]
        if match["vis"]:
            sys.exit(f"refusing to rewrite a re-export: {match[0]!r}")
        statements = []
        if kept:
            statements.append(use_statement(indent, "", root, kept))
        attributes = preceding_attributes(text, match.start(), indent)
        if indent == "" and not attributes:
            top_level_imports.update(segments[0] for segments, alias in moved if not alias)
            moved = [(segments, alias) for segments, alias in moved if alias]
        if moved:
            if statements:
                statements += attributes
            statements.append(use_statement(indent, "", PROTOCOL, moved))
        return "\n".join(statements) if statements else "\x00"

    text = USE_STATEMENT.sub(replace, text)
    # A statement left without leaves disappears with its line.
    return re.sub(r"\n?\x00", "", text)


def rewrite_inline_paths(text, owning_crate, top_level_imports):
    modules = {module[0] for module in MOVED.get(owning_crate, {}) if module}

    def replace(match):
        root = match["root"]
        crate = crate_of_root(root, owning_crate, names_crate_root_by_super=False)
        segments = match["rest"].split("::")[1:]
        for end in range(1, len(segments) + 1):
            name = moved_name(crate, segments[:end]) if crate else None
            if name:
                rest = "".join("::" + segment for segment in segments[end:])
                if root.startswith("tickloop_agent_"):
                    return f"{PROTOCOL}::{name}{rest}"
                top_level_imports.add(name)
                return name + rest
        return match[0]

    text = INLINE_PATH.sub(replace, text)
    if modules:
        bare = re.compile(rf"(?<!::)(?<!\w)(?P<module>{'|'.join(sorted(modules))})::(?P<name>\w+)\b")

        def replace_bare(match):
            name = moved_name(owning_crate, [match["module"], match["name"]])
            if not name:
                return match[0]
            top_level_imports.add(name)
            return name

        text = bare.sub(replace_bare, text)
    return text


def merge_top_level_imports(text, names):
    """Folds `names` and every unattributed top-level protocol import into one statement."""
    statements = []
    for match in USE_STATEMENT.finditer(text):
        tokens = TOKEN.findall(match["tree"])
        if (match["indent"] == "" and not match["vis"] and tokens[0] == PROTOCOL
                and not preceding_attributes(text, match.start(), "")):
            statements.append(match)
    if not names and len(statements) < 2:
        return text
    leaves = {(tuple(segments), alias) for segments, alias in
              (leaf for match in statements for leaf in parse_tree(TOKEN.findall(match["tree"])[2:]))}
    leaves |= {((name,), None) for name in names}
    merged = use_statement("", "", PROTOCOL, sorted((list(s), a) for s, a in leaves))
    if statements:
        first, rest = statements[0], statements[1:]
        for match in reversed(rest):
            end = match.end() + (1 if text[match.end():match.end() + 1] == "\n" else 0)
            text = text[:match.start()] + text[end:]
        return text[:first.start()] + merged + text[first.end():]
    return insert_top_level_use(text, merged)


def insert_top_level_use(text, statement):
    lines = text.split("\n")
    last_use_end = None
    for match in USE_STATEMENT.finditer(text):
        if match["indent"] == "":
            last_use_end = text[:match.end()].count("\n")
        elif last_use_end is not None:
            break
    if last_use_end is None:
        index = 0
        while index < len(lines) and (lines[index].startswith("//!") or lines[index].startswith("#![")
                                      or not lines[index].strip()):
            index += 1
        lines[index:index] = [statement, ""]
    else:
        lines.insert(last_use_end + 1, statement)
    return "\n".join(lines)


def owning_crate_of(path):
    relative = path.relative_to(REPO).as_posix()
    for directory, crate in OWNING_DIRECTORIES.items():
        if relative.startswith(directory + "/src/"):
            return crate
    return None


def rewrite_file(path):
    original = path.read_text()
    owning_crate = owning_crate_of(path)
    is_root_level_file = path.parent.name == "src" and path.name not in ("lib.rs", "main.rs")
    top_level_imports = set()
    text = rewrite_uses(original, owning_crate, is_root_level_file, top_level_imports)
    text = rewrite_inline_paths(text, owning_crate, top_level_imports)
    text = merge_top_level_imports(text, top_level_imports)
    if text != original:
        path.write_text(text)
        return True
    return False


def main():
    changed = [
        path for root in SOURCE_ROOTS for path in sorted((REPO / root).rglob("*.rs"))
        if rewrite_file(path)
    ]
    for path in changed:
        print(path.relative_to(REPO))
    print(f"{len(changed)} files rewritten", file=sys.stderr)


if __name__ == "__main__":
    main()
