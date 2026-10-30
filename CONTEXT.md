# tickloop.ai

Tickloop's product glossary. This file gives only term definitions and naming conventions; the public summary of behavior contracts is in [AGENTS.md](./AGENTS.md), and detailed behavior is in the maintainers' local design records (`docs/design/`, not tracked in Git). Where either conflicts with the source code, the source code wins. Product and component names always use their original English names, in both Chinese and English docs and in the UI.

## Language

### Tick

**Tick**:
A running object that the Agent can describe, read, and call, and from which it receives Tick Updates when it changes. There are only four Tick types (see "Tick Types"); a program a person starts themselves in a Herdr Pane or another terminal is not a Tick.

**Tick Instance**:
One independent run of a Tick, holding its own business identity and runtime state; its identity is written `<type>:<id>` (`hosted:`, `shell:`, `artifact:`). The identity is independent of the card that shows it; closing and reopening produces a new instance.
_Avoid_: using a Pane ID to refer to a Tick

**Tick Standard**:
The unified contract shared by all Tick types: instance identity, the four capabilities describe, read, call, and update (each type provides only those it can), and the rule that updates reach the Loop through a Tick Subscription or an Artifact Association.
_Avoid_: treating the Tick Standard as the name of the wire protocol

**Tick Protocol**:
The wire convention by which a program provides state, tools, and changes to the host over line-delimited JSON-RPC on stdin/stdout; currently protocol 3. A program that implements it is an Extension Tick. The specification is `tick/protocol/SPECIFICATION.md`.

**Tick Adapter**:
A component the host registers for an instance prefix that supplies the Loop with that type's stream of Tick Updates (TickStreamPort). There are currently only two, `hosted` and `shell`, both observing instances through Tickhost; Artifact has no Tick Adapter, and its feedback enters the Loop through an Artifact Association. Adapters grant no permissions and add no model tools.

**Tick Update**:
One change in data or state produced by a Tick Instance, carrying its source and time; it is new information on which the Loop can continue executing. State-type updates keep only the latest one (an Extension Tick's business changes, a Terminal Tick's screen-text changes and exit, a Shell Tick's exit); Artifact Feedback is delivered item by item and never merged.
_Avoid_: event (in the general sense), notification

**Tick Model Tools**:
The model tools the Agent uses to operate Ticks: TickDescribe queries capabilities and the tool catalog, TickRead reads the current state, TickCall calls a Tick Tool declared by an Extension Tick, and TickManage manages instances, terminal input, and subscriptions. Shell and Artifact keep their own native tools (Bash/Read/TaskStop; ArtifactRead/ArtifactEdit/ArtifactComment/ArtifactUpload). TickUpdate and TickSubscription are names of events and relationships in the content stream, not model tools.

**Tick Tool**:
A capability an Extension Tick declares in its own description for the Agent to call through TickCall, either a query (returns data) or an action (changes that instance's content or presentation). A Tick Tool does not change the Layout and brings no additional authorization.
_Avoid_: Action (refers only to the action kind)

**Tick Subscription**:
An explicitly established association between a Chat's Loop and a Tick Instance; it is one kind of Trigger for the Loop. There are only four frequencies: immediate, high (1 minute), medium (10 minutes), and low (30 minutes); immediate is used only for Shell Ticks. Opening, discovering, or reading a Tick is not subscribing; Artifacts do not accept Tick Subscriptions.
_Avoid_: listen, Watch, poll

**Tickhost**:
The local service that runs all local Ticks (Extension Ticks, Terminal Ticks, Shell Ticks) and holds their terminals and screens; the Daemon starts it on demand, one per config root. Tick Cards in the Monitor are the only views attached to it. The program, process, and command are always written `tickhost`.
_Avoid_: Tick host, tick-host, tick_host

**Tick Card**:
The only view of a local Tick: a card under the Tick's row in the Agent View's Monitor column, attached through Tickhost, showing the Tick's own screen below a rule. Showing a card and entering it (giving it keyboard input) are separate; several cards can be open, and hiding or leaving a card does not end the Tick.
_Avoid_: popup, view Pane

**Key Bubbling**:
A Tick handing a key it does not use (Esc, an arrow, Tab or Shift+Tab) back to the client that shows it, through a private OSC sequence in its terminal output, after declaring that it does so; the Monitor then moves the focus from the card to its list or the input.

**Tick Ownership**:
A persistent Chat's responsibility for the lifetime of the Ticks it opens, determined jointly by the config root and the Chat identity; a Subagent's Chat independently owns its own Ticks.

### Tick Types

**Extension Tick**:
A program that implements the Tick Protocol, is opened by the Agent with TickManage `open`, and runs in Tickhost; it provides structured state, its own Tick Tools, and business updates.
_Avoid_: Tick Command, protocol Tick

**Example Tick**:
A program shipped with the project that demonstrates how to use Extension Ticks (`tick/examples/counter.py`).

**Terminal Tick**:
An interactive program that does not speak the Tick Protocol, which the Agent has Tickhost run in a terminal (PTY), such as htop or a REPL; reading it yields screen text, and input goes through TickManage.
_Avoid_: Terminal Observation (retired)

**Shell Tick**:
The Tick formed by one shell command the Agent runs; Tickhost runs it over pipes (no terminal), and it updates only on exit. It is named after the Bash call's Description. It occupies no Pane and is a different type from Terminal Tick.
_Avoid_: background task, Task (these describe only how it runs)

**Artifact Tick**:
A Tick whose instance is one Artifact in Tickloop Artifacts. Its only updates are Artifact Feedback, delivered to the Loop through an Artifact Association; edits to the content by a person or the Agent are not Tick Updates.
_Avoid_: "associated Artifact" (Artifact Association refers specifically to the feedback relationship)

### Plugins

**Plugin**:
An installable unit of components that extend the Agent's available Skills (including Commands), Tick programs, MCP Servers, or Hooks. A plugin can also describe components that Tickloop does not yet support. Installation and enablement are separate; enabling a plugin consents to running its programs and MCP Servers, but a plugin grants no additional permissions.

**Plugin Command**:
A plugin's Markdown instruction registered as a Skill, named `<plugin>:<command>` and invocable through the Skill tool or a slash command.
_Avoid_: model tool (a Command supplies guidance through Skill), CLI command (a separate executable interface)

**Plugin User Config**:
Values a plugin declares that the user fills in, such as an API key or a server address, used by its components. Sensitive values are kept with the plugin's credentials and never reach content the model can see.
_Avoid_: plugin settings (a plugin's `settings` component is a different, unsupported thing)

**Plugin Marketplace**:
A named catalog of plugins and their sources. Tickloop includes `tickloop` and `claude-plugins-official`; users can register additional marketplaces.

**Plugin ID**:
A plugin's name qualified by its marketplace, written `<name>@<marketplace>`. Installed plugin names are globally unique; marketplace entries determine their installed names, while a manifest may supply a different display name. Local installations belong to `local`.

**Tick Package**:
A plugin with a Tick component: a program the Agent can open as a Tick, with a manifest and optional Skills teaching the Agent to use it. Authentication belongs to the plugin, not the Tick Protocol.
_Avoid_: builtin provider (Terminal, Shell, Artifact, and the protocol are Tick types, not data sources)

**Tick Marketplace**:
The built-in `tickloop` Plugin Marketplace, listing Tick Packages and their prebuilt downloads from [tickloopai/ticks](https://github.com/tickloopai/ticks).
_Avoid_: Store

### MCP and Connectors

**MCP Server**:
A Model Context Protocol endpoint the Agent connects to, either a local program over stdio or a remote address over streamable HTTP, configured in the user's settings or by an enabled plugin. Its tools, resources, and instructions reach the Agent; it is not a Tick, and its processes belong to the Agent that uses them. Repository content cannot add or enable one.
_Avoid_: MCP plugin (a plugin may carry MCP Servers, but they are separate things)

**MCP Tool**:
A tool an MCP Server offers, named `mcp__<server>__<tool>` for the model; a plugin's servers are named `plugin_<plugin>_<server>`. Its self-declared hints, such as read-only, grant nothing: approval treats it as a tool with side effects.

**Connector**:
The product layer over a remote MCP Server that makes "give an address, log in, and use it" work: login, renewal and logout of the user's authorization, and management of those credentials. The MCP Server is the protocol endpoint; the Connector is the user's authorized connection to it.
_Avoid_: integration, app

**Tool Exposure**:
How an MCP Server's tools reach the model, set per server: direct (listed among the model's tools), search (found and called through Tool Search), or hidden (not callable). Exposure changes only how tools are reached, never their permissions.

**Tool Search**:
Reaching tools on demand without changing the model's tool list. It uses the model's native search when the model card supports it, and otherwise Fold.
_Avoid_: deferred loading (that names the native form only), lazy tools

**Fold**:
The Tool Search form that asks nothing of the model: three fixed tools, SearchTools, DescribeTools and ExecuteTools, find, describe and call the tools, which never enter the tool list. ExecuteTools runs a batch of independent calls.

### Hooks

**Hook**:
A user-configured command attached to a fixed event in the Agent's work, supplied by the user's settings, an enabled plugin or the Folder's repository configuration. A Hook is neither a model tool nor a Tick or Shell Tick.
_Avoid_: Git hook (a separate mechanism)

**Hook Trust**:
The user's content-bound confirmation that a plugin or repository Hook may run.
_Avoid_: Working Directory Trust, plugin enablement, tool approval (separate decisions)

### Loop and Trigger

**Loop**:
The unit of work, exactly one per Chat, that holds that Chat's execution state: it runs turn by turn from the Input Queue, waits when idle, and continues when a Trigger arrives. A Loop has no terminal end state; only the current turn can fail or be interrupted.
_Avoid_: multiple Loops, goal list, StartGoal

**Interrupt**:
Stops execution of the current turn, issued by Esc or by Ctrl+C on empty input while running; it also pauses the active Goal and ends a self-paced `/loop`, without changing Receiving.

**Stop (`/stop`)**:
Brings the Loop to a full stop: interrupts the current turn, pauses the active Goal, ends a self-paced `/loop`, pauses Receiving, and stops all of this Agent's descendants. The Agent keeps running; ending it is Stopping an Agent.
_Avoid_: using "Stop" to mean leaving the view

**Input Queue**:
The Loop's single queue of inputs awaiting execution, taken in priority order: user input, Agent Messages (including Subagent Completion Notifications), then Trigger input.
_Avoid_: inbox, to-do queue

**Send Now**:
A way of submitting while the Agent is working that makes a message start a new turn immediately (`ctrl+enter`): it first interrupts the current turn, then runs this message. A normal submission only queues.
_Avoid_: "jump the queue", Steer

**Receiving**:
The Loop's single switch for whether it accepts Triggers, with only two states: receiving and paused. `/stop`, ordinary history loading, and Trigger input that cannot be written to the Transcript for a long time pause it; Continue resumes it.
_Avoid_: listening, armed

**Continue**:
The user's explicit action to resume Receiving: choosing Continue in the UI, or submitting a new user input in that Chat. Slash commands, reattaching a view, and `/goal resume` do not count as Continue.

**Trigger**:
A source, controlled by Receiving, that makes the Loop continue without user input. There are five: Tick Subscriptions, a Tick's current content delivered once by the user with Send Update in the Monitor, Artifact Associations, Goal Continuation, and Scheduled Tasks. The first three deliver data; the last two are user-authorized instructions. User input and Agent Messages are not Triggers.
_Avoid_: event trigger, timer, clock rule

**Send Update**:
The user, in the Monitor, hands the current content of an unsubscribed Extension Tick or Terminal Tick to the Agent once, without creating a subscription.

**Scheduled Task**:
A schedule that makes the Loop run a user-authorized instruction repeatedly on a schedule or at a point in time; it is the time source of Triggers and belongs only to the root Agent. A schedule can be recurring (a `/loop` interval or a 5-field cron), one-time, or self-paced. A Scheduled Task is not a Tick.
_Avoid_: clock rule, scheduled Tick

**Self-paced `/loop`**:
A `/loop <instruction>` with no interval: a Scheduled Task in which the Agent uses ScheduleWakeup each turn to decide the next wake-up time; at most one per Chat.

**Goal**:
A completion condition the user sets for a Chat and asks the Agent to keep pursuing until it is met; at most one per Chat at a time, and it belongs only to the root Agent. Its states are active, paused, blocked, budget_limited, and complete.
_Avoid_: using "task" or "Loop goal" to refer to a Goal

**Goal Continuation**:
A Trigger input the host delivers, when the Goal is active and the Loop is idle, to have the Agent keep making progress.

**Completion Check**:
An independent model judgment the host starts after the Agent reports a Goal complete; it looks only at the condition and existing evidence and runs no tools.

### Agent and Interface

**Agent**:
The AI executor that understands goals, makes judgments, and calls tools to get work done; it works in the Loop, both conversing with the user and continuing work when a Trigger arrives or on Goal Continuation.

**Chat**:
A persistent conversation between the user and one Agent, through which the user starts, observes, and controls its single Loop; the same Chat can be open in several Agent Views at once. A Chat is not a Tick and cannot be subscribed to.

**Managed Execution**:
An Agent invocation whose lifecycle belongs to a native ACP or Print client, scoped to its Chat and client control lease. ACP sessions name Chats and each prompt starts a turn of that Chat's single Loop; neither a session nor an execution profile is a Tick type or an execution mode.

**Transcript**:
The persistent record of a Chat's conversation, storing conversation content and Loop state changes in the order they happen; the Chat is restored from it after a restart. When the same Transcript is continued in several places it can form several conversation chains, all still belonging to the same Chat.
_Avoid_: Session, session archive

**Branch**:
A new Chat obtained by copying a conversation from some position in an existing Chat; it records its origin and the two then evolve independently.
_Avoid_: Fork (that word refers to multiple conversation chains within one Transcript)

**Agent View**:
The terminal interface in which the user views and interacts with an Agent; one Agent can be connected to several views. Leaving the view (`/exit`, closing the Agent Pane, Ctrl+C on empty input while idle) does not affect the Agent's execution.
_Avoid_: using "close the Agent" to mean leaving the view

**Daemon**:
The local background process, one per config root (with the default config root, one per system user), that holds the Agent directory and identities, View connections, and display registrations; it starts, stops, and supervises Workers and starts Tickhost on demand. It does not call models or tools itself.
_Avoid_: owner, owner daemon, Agent execution service

**Worker**:
The process the Daemon starts for each root Agent; it calls the Harness as a library and runs model calls, tools, approvals, Triggers, and the Transcript for that Agent and its Subagents and Branches. Processes that run the Harness in other hosts are not called Workers.

**Monitor**:
The unified list in the Agent View of the Tick Instances, Artifacts, background Shells, and Scheduled Tasks this Chat has opened or subscribed to, with the Goal pinned at the bottom; local Ticks are shown in it as Tick Cards. Membership is managed separately from subscription and ownership.
_Avoid_: Ticks panel, task list

**Unread Update**:
The state in which an instance's most recent content update in the Monitor is later than the last time this Chat processed it; unread belongs to the instance within this Chat, not to the subscription.

**Input Dialog**:
The single interactive container at the bottom of the Agent View; when open it replaces the input editor and hosts permission requests, questions, pickers, confirmations, and management screens. Only one is shown at a time and the rest queue. Full-page content screens (Agents Overview, Transcript, Diff) are not Input Dialogs.
_Avoid_: Modal, Popup

**Prompt Suggestion**:
An optional draft for the user's next prompt, offered after the Agent finishes a turn; it becomes user input only once accepted and submitted. Generating and displaying it starts no execution and grants no authorization.

**File Mention**:
Selecting a file or directory with `@` in the input box to add it to the current input.

**Agent Work Context**:
Host environment information the Agent uses to understand the scope of its work, including the default Workspace and known Tabs and Panes; it is expressed separately from the file working directory and from where the Agent is being viewed.

**Agents Overview**:
An overview, machine-wide, of all Agents and their parent-child relationships, from which the user can enter a selected Agent's view.

**Agent State**:
How the Agents Overview names a root Agent, after Claude Code's agent view: Needs input, Working, Idle, Completed (its last turn finished, with nothing pending even after its execution ended), Failed (its run failed, its execution was lost, or it was working or had pending work when the daemon stopped) or Stopped (the user stopped it, or it ended while the daemon was off). The Overview groups them under Needs input, Working and Completed, which holds the last four.
_Avoid_: Done, Errors, Empty, Unknown

**Ended Agent**:
A root Agent whose execution ended but which the Agents Overview still lists from the registry in the configuration root, as Failed or Stopped, until the user continues it from its saved history or removes the row.

**Stopping an Agent**:
Ending a root Agent's execution from the Agents Overview: the Loops and Worker of its family end, and it stays listed as a Stopped Ended Agent.
_Avoid_: confusing it with Stop (`/stop`), which keeps the Agent running

**Current Agent**:
The Agent an Agent View was showing before it opened the Overview, i.e. the one it returns to on leaving the Overview; expressed separately from the cursor selection in the Overview and from host focus.

**Output Style**:
How the Agent expresses its replies to the user (three built-in: Standard, Concise, Detailed); it changes only detail, structure, and tone, not capabilities or authorization.
_Avoid_: style mode, Mode

**Instruction File**:
A Markdown file of standing instructions that a Chat reads when it starts and again after compaction: the user's own `AGENTS.md` in the config root, and the `AGENTS.md` of each directory down to the Folder, or that directory's `CLAUDE.md` files when it has none. Its instructions guide the Agent but grant no authorization.
_Avoid_: memory, rules file

### Subagent and Family

**Subagent**:
An Agent that accepts delegation from a parent Agent and works within the delegation relationship and its authorization scope. A delegation runs in the background: it returns immediately and notifies the parent Agent on completion. Only on a non-interactive host may a delegation instead wait for that run's report.

**Agent Family**:
A root Agent and all descendants it delegates to directly or indirectly (at most three levels); membership is determined by actual delegation, and a Subagent delegating further does not create a new family.
_Avoid_: Team (cross-family relationships do not exist)

**Family Temp Space**:
A private directory of task results and temporary files (including `scratchpad/`) shared by one Agent Family; it contains no other family's files and does not change members' authorization.

**Agent Handle**:
A short name the parent model chooses for a Subagent (such as `sleep-30-2`), unique within the family and never reused; it is an alias for a stable Agent identity. In the UI `@` is a display prefix, while model tools use the bare handle; a handle differs from the task Description and the Chat Title.

**Agent Color**:
A display color assigned when a Subagent is created that distinguishes members within the family and never changes afterward; it identifies the member only and does not express state.

**Agent Message**:
Task direction, clarification, or results sent from one Agent in a family to another; its source is recorded separately from user input, and it cannot serve as user authorization or human approval.

**Directed Human Input**:
Real human input the user submits from one member's view directly to another member of the same family with `@handle`; the recipient remains bound by its delegated authorization ceiling.

**User Relay**:
Content an Agent relays to a member by quoting verified real user input; a relay is still an Agent Message, not human approval or new user authorization.

**StopSubagent**:
Stops execution and waiting for the target member and its descendants, keeping their context, handles, and views; the root can stop any descendant, while a member can stop only its direct children. It does not end background Shells or Ticks.

**Subagent Completion Notification**:
The record of result, status, and usage a Subagent delivers to its parent Agent each time a run ends (completed, failed, or stopped); it enters the parent Agent's Input Queue and does not serve as user input or add authorization.

**Shared Task**:
A work item within one root Loop that can be assigned, claimed, tracked for dependencies, and have its result recorded, shared by solo and family use; enabled by the task tools switch in Settings, off by default. The claimant is the current owner, and a task does not expand its authorization.
_Avoid_: TodoWrite, to-do

### Permissions and Approval

**Execution Mode**:
Controls how the Agent obtains permission to run tools in the current Chat; there are four: Auto (Automatic Approval), Ask (allows reads and edits in the working directory, other protected operations need human confirmation), Plan (read-only planning), and Full Access (skips ordinary approval, keeping explicit Ask/Deny and confirmations that require the user). Execution Mode does not expand a tool's execution boundary or the authorization scope.
_Avoid_: Permission Mode

**Automatic Approval**:
In Auto mode, the process of judging, by user intent and authorization scope, operations not already decided by deterministic rules or an allowed fast path; it grants no new permissions.

**Approval Method**:
The judgment method used by Automatic Approval: LLM (the currently connected auxiliary model, or the current Chat model if there is none) or Judgment (currently provided by Jev). Approval Method and Execution Mode are set separately.
_Avoid_: LLM mode, Jev mode (neither is an Execution Mode)

**Permission Rule**:
A deterministic Allow, Ask, or Deny rule that the program matches against the tool and operation content; conflicts resolve as Deny → Ask → Allow. Sources are host rules, managed constraints, project constraints, and user rules; an explicit Ask cannot be approved on the user's behalf by Automatic Approval.
_Avoid_: using "rule" generically for Auto Approval Rules

**Auto Approval Rule**:
A natural-language rule the Approval Method interprets together with user intent, in three kinds: Soft Deny (blocks by default; lifted by the user's explicit authorization or a Soft Allow), Soft Allow (lifts a matching Soft Deny), and Hard Deny (cannot be lifted by user intent or a Soft Allow). They cannot override deterministic Permission Rules or host restrictions.

**Approval Evidence**:
The immutable snapshot one Automatic Approval hands to the Approval Method: the proposed operation, the rules in effect and the Approval Environment, directory and permission revisions, the current Loop's real user inputs, and prior tool calls, replies, and Agent Messages; when over capacity it is trimmed by rule and omissions are marked.
_Avoid_: approval context compaction (compaction refers to summarizing conversation history)

**Approval Environment**:
Trusted targets, sensitive resources, and usage-context descriptions the user explicitly provides for Automatic Approval; its source is kept distinct from the proposed operation and from data returned by tools.

**Permission Decision Record**:
A record of the final permission decision for one tool operation (allowed directly, Automatic Approval, human confirmation, or denied), with the decision method and reason; being allowed does not mean execution succeeded, and the record is not a standing rule.

**Approval Fallback**:
The process in which Jev, on technical failure or capacity problems, temporarily switches to LLM approval and tries to restore Jev after a cooldown; valid denials do not fall back.

**Manual Approval Escalation**:
Handing an operation to the user to decide after Automatic Approval is blocked; escalation does not create a standing Allow rule.

**Durable Approval**:
A durable host's human confirmation of a model tool call: the Run waits in the Transcript until the person allows or denies the call, whichever host instance holds the Chat then takes the answer, and an allowed call runs at most once. A third-party tool without a Permission Rule goes to the person rather than Automatic Approval; prompts other than a tool call's approval are denied.

**Auto Denial Tracking**:
The count of consecutive and cumulative valid automatic denials in the current Chat; reaching the threshold triggers Manual Approval Escalation. It is not counted per command or tool, and does not imply automatic retry.

**Authorization Scope**:
The explicit boundary within which the user allows the Agent to act autonomously, including accessible resources, permitted actions, and risk limits; Automatic Approval cannot override operations that require human confirmation or are forbidden.

**Working Directory Trust**:
The user's decision to let a working directory supply the Agent's project configuration and instructions; the user must confirm it on first entry, and neither Auto nor the directory's contents can substitute for this confirmation.
_Avoid_: Workspace Trust, automatic trust

### Skill

**Skill**:
A set of instructions teaching the Agent how to combine existing tools to accomplish a kind of work; it provides no state access, tool capability, or extra permissions of its own. The host controls on-demand loading by the model and explicit invocation by the user separately.

**Tickloop Skill**:
The built-in system-level Skill, named `tickloop`, that teaches the Agent to open, operate, and subscribe to Ticks, independent of the working directory; it activates automatically when Tickhost is available, and the user can also start a new Agent with it via `/tickloop`.

### ID

**Tickloop ID**:
The identity of an object Tickloop generates itself (Chat, Loop, Run, input, Tick Instance, subscription, Shell task, Scheduled Task, Shared Task, etc.): always a UUID v7, written as 22-character base62 for the model and the user; external IDs from Herdr, Providers, or the Artifacts service do not count. The rules are in `tick/ids`.
_Avoid_: short ID, serial number

### Host Interfaces

This section covers only concepts encountered in the open-source part; internal components of the hosted service are not described in this repository.

**Tickloop Code**:
The terminal AI agent for code and office work that the `tickloop` command starts; this repository builds it. Always written in full: a bare "Code" is not the product name.
_Avoid_: local tickloop, CLI (as a product name)

**Tickloop Chat**:
The product through which users use Chat and Work via Web/App, run by Tickloop Cloud. Always written in full: a bare "Chat" is a conversation, never the product.
_Avoid_: Chat (as a product name), cloud agent

**Tickloop Cloud**:
Tickloop's hosted service that runs Tickloop Chat; Tickloop Artifacts and the website are also built and deployed by it, and its server is not in this repository. It shares the open-source Harness and Tickhost with Tickloop Code.
_Avoid_: cloud service, server

**Harness**:
The open-source library that advances turns per Chat: it runs the Loop, tools and permission approval, Subagents, and model calls, and reaches the outside only through Seams (Transcript Store, Execution Environment, Model Endpoint, Credential Source, Settings Store).
_Avoid_: Agent Runtime, engine

**Stateless Host**:
A Harness host that keeps no Chat state resident in the process (such as Tickloop Cloud): each time it acquires a Chat Lease it advances one turn with a fresh Harness, reading and writing all state through the Transcript Store; the reference implementation and contract tests are in `harness/test-support`.

**Citation Number**:
A global number that a WebSearch or WebFetch source receives in the family root Chat's source table, never reordered or reused afterward; a host may use it instead of Markdown hyperlinks for citations.

**Chat Lease**:
In a Stateless Host, a lease allowing only one holder at a time to advance a given Chat family, with an increasing epoch; a holder that loses the lease can no longer write the Transcript or publish the Event Stream.
_Avoid_: lock, session lock

**Event Stream**:
Real-time events (streamed output, tool progress, and approval requests) the Harness's host publishes per Chat, each carrying a per-Chat increasing seq for resumption after disconnect and replay; the Transcript is the persistent record.

**Display Projection**:
Chat content derived from the Transcript for display in the UI, rebuildable from the Transcript at any time; content meant only for the model does not enter the Display Projection.

**Execution Environment**:
Where the Agent's Bash, file tools, and Ticks actually run, connected to the Harness as tools; the tools the model sees do not change with the execution location. Locally it is the Local Environment (the local Folder and the daemon's Tickhost). A Chat without an Execution Environment has no Folder and does not touch local files or processes.
_Avoid_: hands (colloquial only), executor

**Folder**:
The directory bound to a Chat in which the Agent reads and writes files and runs commands, located in that Chat's Execution Environment, which decides which of these it supports (a read-only connected directory supports reading files only); a local Chat uses its startup working directory as its Folder.
_Avoid_: Workspace (that means the Herdr Workspace), Project

**Work**:
A Chat with a Folder bound; it is a state of a Chat, not a separate kind of conversation entity, and the UI shows it alongside Chat as Chat/Work. Ticks (except Artifact Ticks) are available only in Work whose Execution Environment supports them.
_Avoid_: Cowork, agent mode, work mode

**Tickloop Accounts**:
Tickloop's single account and login service, a standard OIDC provider; Tickloop Cloud, Tickloop Artifacts, and Tickloop Code all log in with it, and terminals log in with a device code. Short name: Accounts.
_Avoid_: Account Service, IdP, authentication service, login service, SSO service

**Tickloop Account**:
A person's single identity in Tickloop, unique by verified email, which can have several upstream login identities attached.
_Avoid_: user table, account mapping, linked account

### Artifacts

**Tickloop Artifacts**:
The standalone service and web product that stores, presents, and manages Artifacts for users; login is through Tickloop Accounts, and the terminal command is `/artifacts`. Short name: Artifacts. Lists, the content library, and collections of several items are also called Artifacts (e.g. "all Artifacts").
_Avoid_: Artifact Service, Tickloop Docs, "document" (when referring to Artifacts generally); using Artifacts for a single item

**Artifact**:
A single piece of content, owned by a user and created by a person or an Agent, that can be accessed on its own; its type is only Doc, Page, or Slides and does not change after creation. A single item and its attributes, operations, and relationships all use the singular (e.g. "open Artifact", "Artifact Comment").
_Avoid_: using Artifact for a collection; calling ordinary web pages, project docs, or build output Artifacts

**Doc**:
An Artifact (Markdown) primarily for writing, reading, and co-editing, suited to reports, proposals, specs, and notes.
_Avoid_: adding a Chinese translation or a parenthetical explanation to the type name (likewise for Doc, Page, Slides, Slide)

**Page**:
An Artifact (HTML) primarily for visual presentation and interaction, suited to dashboards, charts, explainer pages, small tools, and UI prototypes.
_Avoid_: Design, Prototype (as Artifact type names)

**Slides**:
An Artifact primarily for presenting, made of ordered Slides that can be paged through one by one, overviewed by section, and played full screen.
_Avoid_: Deck, presentation, slideshow, PPT (as Artifact type names)

**Slide**:
One sheet in Slides: a fixed 1920×1080 canvas with a stable ID that can carry speaker notes.
_Avoid_: Page (when referring to a single Slide)

**Artifact Image**:
An image file uploaded to an Artifact, belonging only to it and deleted with it; currently only Slides can upload images, referenced as `asset:<id>`.
_Avoid_: attachment, asset (when referring to an Artifact Image)

**Artifact Live Content**:
The single body currently saved for an Artifact; each save is immediately visible to everyone who can access it.
_Avoid_: draft, publish, working copy

**Artifact Comment**:
A discussion thread started on an Artifact by a logged-in user who can access it; everyone with access sees the same discussion. A comment is not a completed edit to the content.
_Avoid_: private comment, annotation mode

**Artifact Version**:
A snapshot of the Live Content that the service keeps automatically when an editing session pauses, used for viewing and restoring; it does not control what others see.

**Artifact Feedback**:
A persistent message a logged-in user writes to `@tickloop` in a comment, saved even without an Agent; it has no pending, in-progress, completed, or failed task state, and is delivered to every Agent of that user associated with this Artifact.
_Avoid_: feedback request, feedback task (feedback has no task state; claim and acknowledgement at the transport layer only ensure reliable delivery)

**Artifact Association**:
A receiving relationship a user establishes between their own Agent's Chat and an Artifact; it is one kind of Trigger for that Chat's Loop and delivers Artifact Feedback item by item. The same user can associate several Agents. It grants no permissions, does not change ownership, and is not a Tick Subscription; while the Loop has paused Receiving, feedback is kept and not delivered.
_Avoid_: comment binding, subscribing to an Artifact

**Artifact Access Audience**:
Who may access an Artifact: anyone holding the share link, or specified people; expressed separately from the two permission levels, view and edit.

**Artifact Role**:
An account's access level on an Artifact (owner, editor, or viewer), reported by the Artifacts service; the Agent always operates on an Artifact with the role of the account that launched it.
_Avoid_: trusted source, organization trust

**Artifact Link Access**:
Access obtained through a currently valid share link, which ends when the link is disabled or permissions are tightened; it is not the same as being invited as a member.

### Herdr and Layout

**Workspace**:
The working environment in Herdr that organizes a set of Tabs; a user can have several. Tickloop defines no additional container layer.
_Avoid_: Space (former Tickloop container name)

**Layout**:
The organization and arrangement of Workspace → Tab → Pane in Herdr, including splits, sizes, positions, and focus; managed separately from a Tick's business content.

**Layout Host**:
The external environment that hosts the Workspace layout and Agent Views, currently Herdr; Ticks neither run nor are shown in it.

**Tab**:
A page in a Herdr Workspace that organizes a set of Panes; display name and identity are handled separately.

**Pane**:
A layout container in Herdr that hosts a terminal session; it can be split, resized, and moved, and can present an Agent View, never a Tick. Pane identity and Tick Instance identity are handled separately.
_Avoid_: Panel (former Tickloop container name)

### Appearance

**Theme**:
A set of configuration mapping the UI's semantic roles to colors and text decorations; it does not change behavior, layout, glyphs, or copy. The UI has only one glyph set, made of ordinary Unicode characters.
_Avoid_: Glyph Set, Icon Theme, Nerd Font

### Data Sources

**Source**:
An external source that provides data to Tickloop through a Tick Package. Operations that change external state are a separate write capability; read authorization does not extend to writes.

### Versions and Releases

**Product Version**:
The version of an independent release unit: Tickloop (including `tickhost`) shares one version, and each Tick Package has its own; communication compatibility is decided by the respective protocols and does not require equal versions.

**Release Commit**:
The commit in a release that records only changes to product version files, tagged `vX.Y.Z`.
