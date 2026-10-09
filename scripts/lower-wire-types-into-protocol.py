#!/usr/bin/env python3
"""Rewrite the paths of the wire types lowered into agent/protocol (crate-layering tickets 09b and
09e).

The types themselves were moved by hand into `agent/protocol/src`: 09b took 54 from
agent/{model,tools,config}; 09e took the runtime, AgentAccess, hosted and host tool types of
crates/agent and the image records of agent/base. This script rewrites every path that still names
them through their old crate or module:

- `use tickloop_agent_model::{Message, ModelClient, cards::{ModelCard, model_card}};` becomes
  `use tickloop_agent_model::{ModelClient, cards::model_card};` plus
  `use tickloop_agent_protocol::{Message, ModelCard};`, attributes such as `#[cfg(test)]` copied;
- relative paths resolve against the file's module: in `runtime/branch.rs`,
  `use super::{Agent, ChatSnapshot};` becomes `use super::Agent;` plus a protocol import;
- inline paths such as `tickloop_agent_tools::permissions::Decision::Allow` become
  `tickloop_agent_protocol::Decision::Allow`; inside the crate that owned a type,
  `crate::runtime::LoopState::Idle` or `super::LoopState::Idle` becomes `LoopState::Idle` with a
  top-level protocol import, unless the file has its own item of that name;
- a glob such as `use tickloop_agent_model::*;` or a top-level `use super::*;` gains an explicit
  protocol import of the moved names the file mentions;
- a `pub use` of a moved type is dropped: the protocol crate is the only place that names it.

Top-level protocol imports of a file are merged into one statement; format the changed files
afterwards. Usage, from any directory:

    python3 scripts/lower-wire-types-into-protocol.py

Paths are rewritten only where they still name an old location, so a second run leaves them
alone. A glob gains its imports by the names the file mentions, which a second run would add back
after unused ones were removed, so run the script once, then let the compiler drive the cleanup.
It does not touch Git. Hand edits follow it: imports of names a file used unqualified because it
defined them, paths through a module a file reached by a glob (`runtime::LoopState`), the protocol
modules `local_messages` and `projection` whose functions keep their module path, and the
extension traits (`ModelConfigExt`, `LoopSnapshotExt`, ...) that now carry the behaviour.
"""

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PROTOCOL = "tickloop_agent_protocol"

CHAT = ["AgentHostOptions", "AutoReviewerNotice", "ChatSnapshot", "DirectoryTrustPolicy", "Event",
        "EventKind", "LoopId", "OperationRecord", "OperationState", "PendingCommand",
        "PendingInput", "PermissionApprovalSource", "PermissionExecution", "PermissionRequest",
        "PromptPolicy", "ProposedToolCall", "QueuedInput", "QueuedInputKind", "Receipt", "RunId",
        "ToolCallRecovery", "UserShellReceipt", "UserShellRecord", "UserShellState"]
LOOPS = ["EventClaimOperation", "EventClaimStatus", "EventSourceBinding", "FrozenAssociationRequest",
         "LoopSnapshot", "LoopState", "SourceConnection", "SourcePublication"]
RUNTIME_MODULES = {
    "agent_messages": ["AgentMessageSource", "FamilyMessageRejection", "FamilyMessageRejectionCode",
                       "SEND_AGENT_MESSAGE_TOOL_NAME", "VerifiedUserRelay"],
    "approval_assessment": ["ApprovalAssessmentDetails", "ApprovalAttemptDetails",
                            "ApprovalAttemptOutcome", "ApprovalFailureKind",
                            "ApprovalFallbackReason", "ApprovalOutcome", "ApprovalReviewer",
                            "ApprovalReviewerKind"],
    "approval_context": ["OriginalUserInput"],
    "auto_health": ["AutoReviewerHealth"],
    "auto_rules": ["AutoBlockKind", "AutoEnvironment", "AutoEnvironmentSource", "AutoPolicy",
                   "AutoRule", "AutoRuleSource", "RuleApplicability", "RuleFinding",
                   "SoftDenyClearance", "TaskScopeFinding", "VerdictEvidence"],
    "branch": ["BranchCreation", "BranchOrigin", "BranchOutcome", "BranchPublicItem",
               "BranchPublicSource", "BranchRequest", "BranchSessionPermissionRegistration"],
    "chat_title": ["chat_display_title"],
    "compaction": ["COMPACT_BOUNDARY_TEXT", "CompactBoundary", "CompactTrigger",
                   "CompactionMarker", "SummarizeMetadata"],
    "context": ["CompactionOutcome", "CompactionStatus", "HistoryCall", "ModelChange"],
    "delegation": ["DelegationCeiling", "DelegationMetadata", "SUBAGENT_TOOL_NAME",
                   "SubagentRegistration"],
    "denials": ["ApprovalHistoryView", "ApprovalPath", "ApprovalRecord", "ApprovalResult",
                "DenialAutoFinding", "DenialAutoRuleSummary", "DenialCategory",
                "DenialPersistence", "DenialRecord", "DenialReference", "DenialReviewFacts",
                "DenialRuleSource", "DenialRuleSummary", "DenialsView", "DisplaySummary",
                "MAX_RECENT_DENIALS", "render_reference"],
    "directed_summary": ["ActiveConversationItem", "DirectedSummaryDraft",
                         "DirectedSummaryOutcome", "DirectedSummaryRequest", "SummaryDirection"],
    "event_sources": ["EventSourceRecord"],
    "family": ["DirectedHumanSource", "FamilyCompletion", "FamilyInputRecord",
               "SubagentNotificationDelivery"],
    "family_mailbox": ["MessageCause"],
    "input_context": ["ImageContext", "ImageOrigin", "InputContext", "TextContext"],
    "interaction": ["Question", "QuestionAnswer", "QuestionOption", "QuestionRequest"],
    "local_messages": ["FOLDED_TICK_REPLY_CHARACTERS", "INTERRUPT_MESSAGE",
                       "INTERRUPT_MESSAGE_FOR_TOOL_USE", "LocalMessage", "TaskNotification",
                       "TaskNotificationStatus", "TickDeliveryLine"],
    "loop_lease": ["LoopTriggerNotice", "LoopTriggerNoticeKind"],
    "monitor_members": ["MonitorMember", "MonitorMemberSource", "MonitorMembership"],
    "permission_management": ["PermissionChange", "PermissionRuleScope", "PermissionRuleSource",
                              "PermissionRuleView", "PermissionsRevision", "PermissionsView"],
    "persistence": ["SavedChat"],
    "plan": ["Plan", "Progress", "TodoItem", "TodoStatus"],
    "rewind": ["CodeRewind", "RewindDraft", "RewindMode", "RewindOutcome", "RewindPoint"],
    "session_permissions": ["SessionPermissionDenial", "WorkspaceScope"],
    "shell_task_center": ["SHELL_TASK_OUTPUT_MAX_BYTES", "SHELL_TASK_PROMPT_QUIET_PERIOD",
                          "ShellTask", "ShellTaskAction", "ShellTaskCapabilities",
                          "ShellTaskCenterError", "ShellTaskDelta", "ShellTaskNotice",
                          "ShellTaskOrigin", "ShellTaskOutputChunk", "ShellTaskOwner",
                          "ShellTaskPromptWatchdog", "ShellTaskSnapshot", "ShellTaskState",
                          "ShellTaskSubscriptionEvent", "recover_shell_update_time"],
    "tick_subscriptions": ["DEFAULT_CADENCE_SECONDS", "MAX_CADENCE_SECONDS",
                           "MAX_DELIVERY_PAYLOAD_BYTES", "MIN_CADENCE_SECONDS",
                           "TICK_SUBSCRIPTION_SOURCE", "TickLinkState", "TickSubscription",
                           "TickSubscriptionChange", "TickSubscriptionStatus"],
    "triggers": ["Trigger", "TriggerSource"],
    "usage": ["CounterSummary", "CounterTotal", "LedgerError", "ModelRequestKind",
              "ModelUsageSummary", "ObservationEffect", "ProtocolFailure", "RequestFailure",
              "RequestOutcome", "RequestOwner", "RequestSequence", "RequestUsageLedger",
              "RequestUsageRecord", "ResponseRejection", "UsageCounter", "UsageCoverage",
              "UsageSummary"],
}
ACCESS = ["AccessCapability", "AccessError", "AccessErrorCode", "AfterTurnAction", "AgentCommand",
          "AgentDetailQuery", "AgentFilter", "AgentSummary", "ChunkedResponseKind", "ClientMessage",
          "CommandEnvelope", "CommandReceipt", "CommandResult", "ConfigurationChange", "Cursor",
          "DEFAULT_PAGE_LIMIT", "DisplayReceipt", "DisplayUpdate", "DisplayedView",
          "EnvironmentValue", "ExecutionAvailability", "ExecutionRef", "HistoryAction",
          "HostLocation", "HostWorkContext", "InteractionSource", "LaunchSpec", "LiveChatSnapshot",
          "LoopAction", "LoopRef", "MAX_PAGE_LIMIT", "ObservationChange", "ObservationRequest",
          "ObservationUpdate", "ObservationValue", "PROTOCOL_MAJOR", "PROTOCOL_MINOR", "Page",
          "PendingRequestKind", "PersistenceState", "ProtocolLimits", "ProviderCredentialScope",
          "QueuedCommandFailure", "RECEIPT_CAPACITY", "RECEIPT_RETENTION_SECONDS",
          "REPLAY_CAPACITY", "ReceiptState", "RecordedMessage", "RecordedTranscript", "RequestRef",
          "ResetReason", "ServerHello", "ServerMessage", "TaskAction", "TaskOrigin", "TaskRef",
          "TranscriptRef", "UserInput", "VIEW_LEASE_SECONDS", "ViewRef"]
# Everything `crate::runtime::X` could name: the types runtime.rs defined and the ones it
# re-exported. Names it never re-exported are harmless here, as no such path compiled.
RUNTIME_ROOT = sorted(set(CHAT + LOOPS).union(*RUNTIME_MODULES.values()))

# Owning crate -> module path inside it (as a tuple, () for the crate root) -> moved names.
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
    "tickloop_agent_base": {
        ("image",): ["Image", "ImageMediaType"],
    },
    "tickloop_agent": {
        ("runtime",): RUNTIME_ROOT,
        **{("runtime", module): names for module, names in RUNTIME_MODULES.items()},
        ("agent_access",): ACCESS + ["AgentAccess", "Observation", "bounded_update"],
        ("agent_access", "protocol"): ACCESS,
        ("hosted", "tools"): ["HostedMember", "HostedMemberState", "HostedSnapshot"],
        ("hosted", "controller"): ["HostedViewGrant"],
        ("host_tools",): ["HerdrInteraction", "HostCallContext", "HostEndpointIdentity",
                          "HostToolAuthority"],
        ("tick_stream",): ["ObservationKind"],
    },
}
OWNING_DIRECTORIES = {
    "agent/model": "tickloop_agent_model",
    "agent/tools": "tickloop_agent_tools",
    "agent/config": "tickloop_agent_config",
    "agent/base": "tickloop_agent_base",
    "crates/agent": "tickloop_agent",
}
SOURCE_ROOTS = ["crates/agent/src", "crates/agent/tests", "agent/model", "agent/tools",
                "agent/config", "agent/base", "agent/artifacts", "agent/test-support"]

USE_STATEMENT = re.compile(
    r"^(?P<indent>[ \t]*)(?P<vis>pub(?:\([^)]*\))?\s+)?use\s+(?P<tree>[^;]+);", re.MULTILINE)
TOKEN = re.compile(r"::|[{},*]|\w+")
CRATE_ROOTS = "tickloop_agent(?:_(?:model|tools|config|base))?"
INLINE_PATH = re.compile(rf"(?<!::)(?<![\w.])(?P<root>{CRATE_ROOTS}|crate|super)"
                         r"(?P<rest>(?:::\w+)+)")
LOCAL_ITEM = r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|type|trait|fn|const|static)\s+{}\b"
INLINE_MODULE = re.compile(r"^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?mod \w+ \{$", re.MULTILINE)


def moved_name(crate, segments):
    """The moved item a leaf path (below `crate`) names, or None."""
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


class SourceFile:
    """Where a file sits: its crate and module path, which relative paths resolve against."""

    def __init__(self, path, text):
        relative = path.relative_to(REPO).as_posix()
        self.crate, self.module = None, []
        # Child modules the file declares; a path may start from one of them.
        self.children = set(re.findall(r"^(?:pub(?:\([^)]*\))?\s+)?mod (\w+);", text, re.MULTILINE))
        for directory, crate in OWNING_DIRECTORIES.items():
            if relative.startswith(directory + "/src/"):
                self.crate = crate
                parts = relative[len(directory + "/src/"):].removesuffix(".rs").split("/")
                if parts[-1] in ("lib", "main", "mod"):
                    parts = parts[:-1]
                self.module = parts

    def resolve(self, segments, depth):
        """(crate, absolute segments) of a path written `depth` inline modules below the file's
        module, or None when it leaves the known crates."""
        root = segments[0]
        if root.startswith("tickloop_agent"):
            return root, segments[1:]
        if self.crate is None:
            return None
        if root == "crate":
            return self.crate, segments[1:]
        if root in self.children:
            return self.crate, self.module + segments
        if root not in ("super", "self"):
            return None
        module = self.module + ["<inline>"] * depth
        rest = list(segments)
        while rest and rest[0] in ("super", "self"):
            if rest.pop(0) == "super":
                if not module:
                    return None
                module = module[:-1]
        if "<inline>" in module:
            return None
        return self.crate, module + rest


def inline_modules(text):
    """Character ranges of the inline modules (`mod tests { ... }`) of a file."""
    ranges = []
    for match in INLINE_MODULE.finditer(text):
        close = re.compile(rf"^{match['indent']}}}$", re.MULTILINE).search(text, match.end())
        ranges.append((match.start(), close.end() if close else len(text)))
    return ranges


def module_depth(ranges, position):
    return sum(start <= position < end for start, end in ranges)


def protocol_names_imported_at(text, indent):
    return {
        segments[-1]
        for match in USE_STATEMENT.finditer(text)
        if match["indent"] == indent and TOKEN.findall(match["tree"])[0] == PROTOCOL
        for segments, alias in parse_tree(TOKEN.findall(match["tree"])[2:])
    }


def has_own_item(text, name):
    if re.search(LOCAL_ITEM.format(re.escape(name)), text, re.MULTILINE):
        return True
    for match in USE_STATEMENT.finditer(text):
        tokens = TOKEN.findall(match["tree"])
        if tokens[0] == PROTOCOL or len(tokens) < 3:
            continue
        for segments, alias in parse_tree(tokens[2:]):
            if (alias or segments[-1]) == name:
                return True
    return False


def rewrite_uses(text, source, top_level_imports):
    modules = inline_modules(text)

    def replace(match):
        tokens = TOKEN.findall(match["tree"])
        indent = match["indent"]
        if tokens[0] == PROTOCOL or len(tokens) < 3 or tokens[1] != "::":
            return match[0]
        root = tokens[0]
        kept, moved = [], []
        for segments, alias in parse_tree(tokens[2:]):
            resolved = source.resolve([root] + segments, module_depth(modules, match.start()))
            name = resolved and moved_name(*resolved)
            if name:
                moved.append(([name], alias))
            elif resolved and segments[-1] == "*" and not match["vis"] and (
                    root.startswith("tickloop_agent") or indent == ""):
                crate, path = resolved
                imported = protocol_names_imported_at(text, indent)
                mentioned = [
                    name for name in MOVED.get(crate, {}).get(tuple(path[:-1]), [])
                    if name not in imported and re.search(rf"(?<!::)\b{name}\b", text)
                ]
                kept.append((segments, alias))
                moved += [([name], None) for name in mentioned]
            else:
                kept.append((segments, alias))
        if not moved:
            return match[0]
        statements = []
        if kept:
            statements.append(use_statement(indent, match["vis"] or "", root, kept))
        if match["vis"]:
            # A re-export of a moved type disappears: callers name the protocol crate.
            return "\n".join(statements) if statements else "\x00"
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
    # A statement left without leaves disappears with its line, attributes included.
    text = re.sub(r"(?:\n[ \t]*#\[[^\n]*\])*\n?[ \t]*\x00", "", text)
    return text


def module_aliases(text, source):
    """Modules a file names by a bare identifier, as (crate, absolute path): its own child
    modules and the modules it imports by name (`use super::compaction::{self, ...}`)."""
    aliases = {}
    if source.crate:
        for name in source.children:
            aliases[name] = (source.crate, source.module + [name])
    for match in USE_STATEMENT.finditer(text):
        tokens = TOKEN.findall(match["tree"])
        if match["indent"] or len(tokens) < 3 or tokens[1] != "::":
            continue
        for segments, alias in parse_tree(tokens[2:]):
            if segments[-1] == "self":
                segments = segments[:-1]
            resolved = source.resolve([tokens[0]] + segments, 0)
            if resolved and resolved[0] in MOVED and tuple(resolved[1]) in MOVED[resolved[0]]:
                aliases[alias or resolved[1][-1]] = resolved
    return aliases


def rewrite_inline_paths(text, source, top_level_imports):
    modules = inline_modules(text)
    aliases = module_aliases(text, source)

    def qualified(crate, path, keep_qualified):
        for end in range(1, len(path) + 1):
            name = moved_name(crate, path[:end])
            if name:
                rest = "".join("::" + segment for segment in path[end:])
                if keep_qualified or has_own_item(text, name):
                    return f"{PROTOCOL}::{name}{rest}"
                top_level_imports.add(name)
                return name + rest
        return None

    def replace(match):
        root = match["root"]
        segments = [root] + match["rest"].split("::")[1:]
        depth = module_depth(modules, match.start())
        resolved = source.resolve(segments, depth)
        if not resolved:
            return match[0]
        # Inside an inline module a top-level import would go unused outside its configuration.
        return qualified(*resolved, root.startswith("tickloop_agent") or depth > 0) or match[0]

    text = INLINE_PATH.sub(replace, text)
    if aliases:
        bare = re.compile(rf"(?<![:\w.])(?P<module>{'|'.join(map(re.escape, sorted(aliases)))})"
                          r"::(?P<name>\w+)\b")

        use_spans = [(use.start(), use.end()) for use in USE_STATEMENT.finditer(text)]

        def replace_bare(match):
            if any(start <= match.start() < end for start, end in use_spans):
                return match[0]
            crate, path = aliases[match["module"]]
            depth = module_depth(modules, match.start())
            return qualified(crate, path + [match["name"]], depth > 0) or match[0]

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


def rewrite_file(path):
    original = path.read_text()
    source = SourceFile(path, original)
    top_level_imports = set()
    text = rewrite_uses(original, source, top_level_imports)
    text = rewrite_inline_paths(text, source, top_level_imports)
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
