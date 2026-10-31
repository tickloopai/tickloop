# Tick Protocol Specification

| Item | Value |
| --- | --- |
| Protocol version | 3 |
| Status | Draft; describes the Tickloop 0.2 implementation in this repository |
| Date | 2026-10-28 |
| Reference implementation | Host types and validation in `tick/protocol`; Tick-side SDK in `tick/sdk`; example in `tick/examples/counter.py` |

## Agent tools and package guidance

The protocol has three request methods (`tick/describe`, `tick/read`, `tick/call`) and one notification (`tick/update`). Tickloop exposes them through TickDescribe (capabilities), TickRead (current state), TickCall (declared operations) and TickUpdate (delivery event). TickRead does not repeat the descriptor or tool catalog. TickManage handles lifecycle, views, terminal input and Loop subscriptions in the Agent/host; it is not an additional Tick program RPC. Package Skills document launch commands and login prerequisites. A running instance's descriptor is the authoritative capability catalog.

<a id="s1"></a>

## 1. Overview

<a id="s1-1"></a>

### 1.1 Purpose

The Tick protocol defines how a terminal program exposes its state, callable operations, and business changes to a host. An AI Agent can read the program, call its tools, and resume work when new information arrives. A program implementing this protocol is a **extension Tick**.

An extension Tick serves two participants: a person using its terminal interface, and an Agent using the protocol through the host. Both interact with the same process and the same state.

<a id="s1-2"></a>

### 1.2 Roles

- **Tick**: the program implementing this protocol. It reads requests from standard input, writes protocol messages to standard output, and draws its interface on its controlling terminal.
- **Host**: the party that starts and communicates with the Tick. In Tickloop, this is the launcher process started for each Tick by Tickhost, together with Tickhost itself.
- **Client**: a party that uses a Tick through the host, usually an Agent's Loop. The interface between client and host is outside the scope of this specification.
- **View**: an interface displaying the Tick's controlling terminal, such as a card in a client's Monitor. Multiple views can attach to one Tick. Opening or closing a view does not affect the protocol session.

<a id="s1-3"></a>

### 1.3 Design principles

1. **The program is the tool.** A Tick declares its capabilities in a tool catalog. The host does not need custom code for each kind of Tick.
2. **State is a complete, current snapshot.** Each update carries the full current state, rather than a delta. The host keeps only the latest snapshot, so missing intermediate updates does not leave it with an inconsistent state.
3. **Changes can be approved.** Tools declare whether they mutate external state. A call is bound to the catalog revision used for approval; changing the catalog invalidates that approval.
4. **An unknown outcome is not retried.** Once a mutating call has been dispatched, a timeout or disconnection does not trigger an automatic resend.
5. **Implementation stays small.** A language's standard library is enough to implement a compliant Tick.

<a id="s1-4"></a>

### 1.4 Relationship to LSP and MCP

Like LSP, this protocol uses JSON-RPC 2.0 over standard input and output. Unlike LSP, messages are delimited by newlines, with no `Content-Length` header ([Section 3.1](#s3-1)).

The tool catalog resembles MCP tools: names, descriptions, and inputs defined with JSON Schema. Three features distinguish the Tick protocol:

- A Tick also has readable, sequenced state and notifies the host when it changes.
- Tool calls are bound to a catalog `revision`.
- A Tick has a terminal interface for people, kept separate from protocol messages.

There is no capability negotiation in an initialization handshake. The host learns the Tick's capabilities from `tick/describe`.

<a id="s2"></a>

## 2. Conventions

<a id="s2-1"></a>

### 2.1 Normative language

The terms **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) and [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174), only when shown in bold.

Unbolded explanations, examples, and notes are informative.

<a id="s2-2"></a>

### 2.2 Type notation

Message structures use TypeScript interfaces, following the notation of the LSP specification:

- `?` marks a field that may be omitted.
- `integer` is an integer from -2^53+1 through 2^53-1; `uinteger` is an integer from 0 through 2^53-1.
- `object` means a JSON object.
- Timestamps are [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) strings.
- Paths within JSON values are [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) JSON Pointers.

```typescript
type integer = number;
type uinteger = number;
type JSONValue = string | number | boolean | null | JSONValue[] | { [key: string]: JSONValue };
/** A JSON Schema document whose top level is an object. */
type JSONSchema = { [key: string]: JSONValue };
```

<a id="s2-3"></a>

### 2.3 Bytes and sizes

All sizes are in UTF-8 bytes. Message body sizes are measured after the receiver re-encodes the parsed value ([Section 3.5](#s3-5)).

<a id="s3"></a>

## 3. Base protocol

<a id="s3-1"></a>

### 3.1 Transport

The host starts the Tick as a child process with three channels:

| Channel | Purpose |
| --- | --- |
| Standard input | Requests from the host to the Tick |
| Standard output | Responses and notifications from the Tick to the host; protocol messages only |
| Controlling terminal, also connected to standard error | The Tick's interface, displayed by views |

Message framing rules:

- Each message is a UTF-8 JSON value followed by a newline `\n` (U+000A).
- A message **MUST NOT** contain unescaped newlines.
- A Tick **MUST NOT** write non-protocol content to standard output, including debug output, progress bars, or third-party library output.
- A Tick **MUST** terminate the final message with a newline. Output that ends without one is a violation ([Section 3.6](#s3-6)).
- A Tick **MUST** write each message as a complete line. When multiple threads write to standard output, bytes from different messages **MUST NOT** interleave.
- Content written to standard error appears in the view. A Tick **SHOULD NOT** write diagnostic logs to standard error; it **SHOULD** use its own log file instead.

> Note: This protocol does not use LSP's `Content-Length` header. Newline framing lets even a minimal implementation parse messages with a line reader.

<a id="s3-2"></a>

### 3.2 Messages

All messages follow [JSON-RPC 2.0](https://www.jsonrpc.org/specification) and carry `"jsonrpc": "2.0"`. Batch messages are not supported.

```typescript
interface Message {
  jsonrpc: "2.0";
}

interface RequestMessage extends Message {
  /** Assigned by the host; unique among the requests in flight. */
  id: uinteger;
  method: string;
  params?: object;
}

interface ResponseMessage extends Message {
  /** The id of the request this answers. */
  id: uinteger;
  /** Present on success, absent on error. */
  result?: JSONValue;
  /** Present on error, absent on success. */
  error?: ResponseError;
}

interface ResponseError {
  code: integer;
  message: string;
  data?: JSONValue;
}

interface NotificationMessage extends Message {
  method: string;
  params?: object;
}
```

Message direction:

- Requests travel only from host to Tick. A Tick **MUST NOT** send requests to the host.
- The only notification from Tick to host is `tick/update`. The host **MUST** ignore other notifications and continue the session. It **MAY** log or count them.
- The host does not send notifications to the Tick.

Request IDs:

- The host assigns unsigned integer IDs.
- A Tick **MUST** respond to each request exactly once, using the same `id`.
- A response **MUST** contain exactly one of `result` and `error`.
- A Tick **MUST NOT** send a response with `id: null`. If a request cannot be parsed or has no usable `id`, the Tick **MUST** discard the line without replying. It **MAY** record the event in its diagnostic log. The host does not send such requests.

> Note: JSON-RPC 2.0 permits errors with `id: null` for unparseable requests. This host only sends valid requests, so the Tick protocol prohibits such responses and treats them as explicit violations.

Method names beginning with `tick/` are reserved for this protocol.

<a id="s3-3"></a>

### 3.3 Concurrency and ordering

- The host **MAY** send a new request before receiving a response to an earlier one.
- At most 8 requests may be in flight, including `tick/describe` and `tick/read` ([Section 3.5](#s3-5)).
- A Tick **MAY** process requests concurrently and **MAY** respond in any order. The host matches responses by `id`.
- `tick/describe` and `tick/read` **SHOULD** respond promptly, without waiting for running tool calls.

A Tick may emit `tick/update` at any time: before initialization finishes, or before or after a response to an in-flight request. The host checks update sequence numbers in arrival order ([Section 6.3](#s6-3)).

<a id="s3-4"></a>

### 3.4 Error codes

Ticks use the following error codes. The host uses the code to determine whether a call may have had an effect.

| Code | Name | Meaning | Host interpretation |
| --- | --- | --- | --- |
| -32600 | InvalidRequest | Not a valid JSON-RPC request | Not executed |
| -32601 | MethodNotFound | Unknown method | Not executed |
| -32602 | InvalidParams | Invalid parameters or unknown tool | Not executed |
| -32001 | UnsupportedJsonRpc | `jsonrpc` is not `"2.0"` | Not executed |
| -32003 | FrameTooLarge | Request exceeds the Tick's accepted size | Not executed |
| -32004 | RevisionMismatch | Call `revision` does not match the current catalog | Not executed; catalog changed |
| -32000 | ToolFailed | Tool ran but failed | Executed |
| Other | — | Defined by the Tick | Executed |

- A call answered with a “not executed” error **MUST** have had no effect.
- If a tool fails after starting and may have had partial effects, the Tick **MUST** use -32000 or another “executed” error code.
- `message` **SHOULD** be a human-readable explanation. `data` **MAY** carry structured details.

<a id="s3-5"></a>

### 3.5 Limits

| Item | Limit | If exceeded |
| --- | --- | --- |
| Request | 512 KiB | The host does not send it; the Tick never receives it |
| `tick/describe` result | 512 KiB after encoding | Initialization fails, or the request fails during normal operation |
| `tick/update` notification | 512 KiB after encoding | Protocol violation; session ends |
| Response line | 8 MiB | Protocol violation; session ends |
| Response `result` | 8 MiB after encoding | Outcome reported as unknown; session continues. A Tick that detects its own oversized result returns -32000 |
| In-flight requests, including `tick/describe` and `tick/read` | 8 | The host does not send another request and reports busy to the client |

- Encoded size is measured after the host parses and re-encodes the value. Number representations can grow, for example from `1e15` to `1000000000000000.0`. A Tick **SHOULD** leave headroom.
- The host **MAY** impose stricter aggregate limits, such as 64 MiB of response buffers across all in-flight calls.

<a id="s3-6"></a>

### 3.6 Violations

The following are protocol violations. The host **MUST** end the session, treat all in-flight requests as having unknown outcomes, and terminate the Tick process:

- A line is not valid JSON, or `jsonrpc` is not `"2.0"`.
- A line has neither an integer `id` nor a `method`, including responses with a null or string `id`.
- Output ends without a terminating newline.
- A line exceeds 8 MiB.
- A `tick/update` violates its requirements ([Section 8.2](#s8-2)).

The following fail only the affected request; the session continues:

- A response contains both `result` and `error`, or neither.
- A `tick/read` result violates the sequence rules ([Section 6.3](#s6-3)).
- An encoded `result` exceeds 8 MiB.

The host **MUST** discard a response whose `id` does not match an in-flight request.

<a id="s4"></a>

## 4. Lifecycle

<a id="s4-1"></a>

### 4.1 Startup

The host starts the Tick using the command supplied by the user:

- `argv` matches the command, with `argv[0]` resolved to an absolute path.
- The working directory is the one specified when opening the Tick.
- The environment includes:

| Variable | Value |
| --- | --- |
| `TICKLOOP_INSTANCE_ID` | This run's instance identity, in the form `hosted:<id>` |
| `TERM` | `xterm-256color` |

- `TICKLOOP_INSTANCE_ID` is for logging or distinguishing concurrent runs. It is absent from protocol messages and is not a credential.
- A Tick **MAY** use its presence to distinguish host startup from a person launching it directly in a terminal.
- Other variables beginning with `TICKLOOP_` are internal to the host. A Tick **MUST NOT** depend on them.

The Tick runs in the foreground process group of a session whose controlling terminal is the view terminal:

- Initial size is 120 columns by 30 rows, kept until a view attaches.
- The terminal takes the size of the view that typed last, or else of the view that attached last; when that view changes size, the host resizes the terminal and the kernel sends `SIGWINCH` to the Tick. When the last view detaches, the terminal keeps its last size.
- Resizing does not use the protocol.

<a id="s4-2"></a>

### 4.2 Initialization

After startup, the host sends, in order:

1. `tick/describe`, then validates its result ([Section 5.3](#s5-3)).
2. `tick/read`, to obtain initial state.

Rules:

- Each request **MUST** receive a response within 10 seconds.
- The total time from startup through initialization is at most 10 seconds.
- Any failure, timeout, or invalid result fails initialization. The host terminates the Tick process; the instance does not appear in the client. The failure reason and the first 256 characters of the view screen are reported to the client.
- A Tick **MAY** emit `tick/update` before initialization completes. The host receives these updates normally.

<a id="s4-3"></a>

### 4.3 Versioning

The `protocol` field in `tick/describe` declares the version implemented by the Tick. Host requests do not carry a version.

- The host accepts only the version it implements. A mismatch fails initialization, with an error naming both versions, for example: `unsupported Tick protocol version 2; this host speaks Tick protocol 3`.
- The Tick does not receive this error. It appears in the view and is reported to the client.
- Extensions that preserve existing field meanings, such as new optional fields, do not change the protocol version.
- The host **MUST** ignore unknown fields in descriptors and state. A Tick **SHOULD** ignore unknown request parameters.

<a id="s4-4"></a>

### 4.4 Operation

After initialization, the host sends requests as needed:

| Event | Request |
| --- | --- |
| Client queries the Tick's capabilities | `tick/describe` |
| Client reads the Tick | `tick/read` |
| Client prepares a tool call | `tick/describe`, to obtain the catalog and `revision` |
| Client calls a tool after approval | `tick/call` |
| Subscription observation stream is established or restored | `tick/describe` |
| No usable cached state is available at delivery time | `tick/read` |

- The host **MAY** send `tick/describe` and `tick/read` at any time, without a limit on the total number of requests.
- Both methods **MUST** be free of side effects and **SHOULD** respond within a few milliseconds.

<a id="s4-5"></a>

### 4.5 Shutdown

To terminate a Tick, the host sends `SIGTERM` to its process group. It **MAY** then send `SIGKILL` without a grace period. The host does not guarantee that it will close standard input first.

- A Tick **MUST NOT** rely on shutdown cleanup to save important data.
- On standard-input EOF, a Tick **SHOULD** wait for in-flight calls to finish, then exit.
- If a Tick exits on its own, the session ends. The host treats in-flight requests as having unknown outcomes, and the client sees that the instance has ended. The exit code is not part of the protocol.

<a id="s5"></a>

## 5. tick/describe

<a id="s5-1"></a>

### 5.1 Request

```typescript
// method: "tick/describe"
interface DescribeParams {}
```

The host sends `params: {}`.

<a id="s5-2"></a>

### 5.2 Result

```typescript
interface TickDescriptor {
  /** The protocol version this Tick implements. */
  protocol: 3;
  /** What the program is, such as "example.counter". */
  kind: string;
  /** A short name shown to people and models. */
  title: string;
  /** One or two sentences on what the Tick shows and does. */
  description: string;
  /** Names this tool catalog. Changes whenever any tool changes. */
  revision: string;
  tools: TickTool[];
  /** A query tool that rebuilds what latest-only updates dropped. */
  recover_with?: string;
  /** What the Tick declares about the data of its state. */
  state?: StateDeclaration;
  /** What the Tick declares about the terminal it draws on. */
  screen?: ScreenDeclaration;
}

interface TickTool {
  /** Unique within the catalog. */
  name: string;
  /** What the tool does, written for a model deciding whether to call it. */
  description: string;
  /** JSON Schema of `args`; its top level is an object. */
  input_schema: JSONSchema;
  /** Whether a call can change anything outside the call's own result. */
  mutates: boolean;
}

interface StateDeclaration {
  /** Arrays in `data` ordered in time. */
  series?: StateSeries[];
}

interface StateSeries {
  /** JSON Pointer to an array within `data`; "" is `data` itself. */
  path: string;
  /** The end of the array holding its newest element. */
  newest: "first" | "last";
}

interface ScreenDeclaration {
  /** The fewest terminal rows the Tick stays usable at: an integer from 6 to 14; 6 when absent. */
  min_rows?: number;
  /** The most terminal rows the Tick can use: an integer no less than `min_rows`. When absent,
   *  a card shows the Tick at `min_rows`. */
  max_rows?: number;
}
```

<a id="s5-3"></a>

### 5.3 Validation

The host **MUST** reject a result unless all of the following hold:

- `protocol` equals the version implemented by the host.
- `kind` and `title` are nonempty after trimming whitespace.
- `revision` is not an empty string.
- Each tool's `name` is nonempty after trimming whitespace and unique within the catalog.
- Each `input_schema` is an object and a valid JSON Schema.
- If present, `recover_with` names a catalog tool whose `mutates` is `false`.
- Each `state.series` path is empty or starts with `/`, and paths are unique.
- If present, `screen.min_rows` is an integer from 6 to 14.
- If present, `screen.max_rows` is an integer no less than `screen.min_rows` (6 when absent) and no greater than 65535.
- The encoded result is no larger than 512 KiB.

<a id="s5-4"></a>

### 5.4 `kind`

`kind` is the program's self-declared type. It is unrelated to the instance identity prefix (`hosted:`, `shell:`, or `artifact:`). Multiple instances may have the same `kind`.

A `kind` **SHOULD** be a lowercase, dot-separated name whose first component identifies the publisher, such as `example.counter` or `acme.orderbook`.

<a id="s5-5"></a>

### 5.5 `revision`

`revision` identifies the current tool catalog:

- A Tick **MUST** assign a new `revision` whenever a tool's name, description, input schema, or `mutates` changes.
- A Tick whose catalog does not change during its lifetime **MAY** use a constant revision.
- `revision` is an opaque string. The host compares it only for equality.

<a id="s5-6"></a>

### 5.6 `mutates`

- If a call can change anything beyond its own result, the Tick **MUST** declare the tool as `mutates: true`. This includes its own state, business content in its interface, files, and remote services.
- Query tools that only read information declare `mutates: false`.

The host uses `mutates` to decide whether approval is needed before a call and whether an unknown outcome may be retried. Declaring a mutating tool as `false` bypasses approval ([Section 10](#s10)).

<a id="s5-7"></a>

### 5.7 `screen`

`screen.min_rows` and `screen.max_rows` count the rows of the terminal the Tick draws on, its own status line included. They exclude anything the client draws around that terminal, such as a card's title line. The host reads the declaration once, when the Tick initializes; later changes are ignored.

A client may show a Tick's terminal in a card beside its conversation, as the Monitor does. Such a card is exactly `screen.min_rows` rows tall when it has room, and 40 columns wide unless the user narrows it. A Tick that declares `screen.max_rows` may get more rows, up to `screen.max_rows`, when the client has height to spare; a client never takes rows from another card's `min_rows` to grow one. For Ticks shown this way:

- A Tick **SHOULD NOT** use Esc: its back key is Backspace instead of Esc. A Tick that uses Esc anyway **MUST NOT** require a double Esc: while a card has the keyboard, a second Esc within 300 ms of the first leaves the card and is not delivered to the Tick. The first Esc is delivered at once.
- A Tick **SHOULD** declare key bubbling and bubble the keys listed in [Section 5.8](#s5-8) that it does not use, so the person can move to other Ticks with them.
- A Tick **MUST** stay usable at 30 columns.
- A Tick **MUST** redraw its whole screen after `SIGWINCH`, and **SHOULD NOT** repaint when nothing changed.

<a id="s5-8"></a>

### 5.8 Key bubbling

A Tick shown in a card can hand keys it does not use back to the client, which then acts on them, for example by selecting the next Tick. Key bubbling travels as private OSC sequences in the Tick's terminal output, not as protocol messages, so it works for any program running in a Tick's terminal, extension or not.

Each sequence is `ESC ] 7777 ; tickloop ; <field>` followed by BEL (`0x07`) or ST (`ESC \`), with no spaces:

| Sequence | Meaning |
| --- | --- |
| `\x1b]7777;tickloop;bubble=on\x07` | The Tick bubbles the listed keys it does not use. |
| `\x1b]7777;tickloop;bubble=off\x07` | The Tick bubbles no keys. |
| `\x1b]7777;tickloop;key=<name>\x07` | The Tick did not use the key `<name>` it just read. |

Key names are `esc`, `up`, `down`, `left`, `right`, `tab` and `backtab` (Shift+Tab).

Rules:

- A Tick declares `bubble=on` once it is ready for input, typically at startup. The declaration holds for the rest of the instance, across views and resizes, until the Tick writes `bubble=off`. A Tick starts with bubbling off.
- A Tick **SHOULD** write `key=<name>` right after reading one of the listed keys that it does not use in its current state, and **MUST NOT** write it for a key it acted on.
- The host ignores `key=` while bubbling is off, and unknown fields and key names. It removes every such sequence from the screen it keeps, so views never show them.
- A bubbled key goes to the view that typed last, or else the view that attached last. What the client does with it is the client's choice: the Monitor leaves the card on `esc` for its list, on `up`, `down`, `tab` and `backtab` for its list with another Tick selected (entering no other card), and on `left` for its input.
- Other keys, including letters such as `h`, `j`, `k` and `l`, are never bubbled.
- A Tick that never declares bubbling works unchanged; the person leaves its card with a double Esc.

The host ignores any OSC longer than 1024 bytes. In Rust, `tick_protocol::KeySignal::sequence` writes these sequences. A Neovim configuration, for example, can bubble Esc in Normal mode:

```lua
vim.api.nvim_create_autocmd('VimEnter', {
  callback = function() io.stdout:write('\27]7777;tickloop;bubble=on\7') end,
})
vim.keymap.set('n', '<Esc>', function() io.stdout:write('\27]7777;tickloop;key=esc\7') end)
```

<a id="s5-9"></a>

### 5.9 Focus reports

A Tick shown in a card can tell whether the person is typing into it, for example to show its key hints only then, with standard xterm focus reporting:

- A Tick enables it by writing `CSI ? 1004 h` (`\x1b[?1004h`) to its terminal, alone or in a combined list such as `CSI ? 1004 ; 2004 h`, and disables it with `CSI ? 1004 l`.
- While it is enabled, the host writes `ESC [ I` to the Tick's terminal when one of its views gains the keyboard, as a card does when the person enters it, and `ESC [ O` when none of its views has the keyboard any more. Right after the Tick enables it, the host writes the current state once.
- With several views, the Tick has the focus while any of them has the keyboard.
- The host never writes either sequence to a Tick that has not enabled focus reporting, and it never passes on the focus reports of the client's own terminal.

<a id="s6"></a>

## 6. tick/read

<a id="s6-1"></a>

### 6.1 Request

```typescript
// method: "tick/read"
interface ReadParams {}
```

<a id="s6-2"></a>

### 6.2 Result

```typescript
interface TickState {
  /** Grows with every update; see 6.3. */
  seq: uinteger;
  /** The complete current state, or null while the Tick has no content yet. */
  data: JSONValue | null;
  /** When the state was observed; the host uses its receipt time when absent. */
  at?: string;
}
```

- `data` is the Tick's complete current business state, not a delta from the previous state.
- Null `data` means no content is available yet, for example while data is loading.
- If present, `at` **MUST** be a valid RFC 3339 timestamp.

<a id="s6-3"></a>

### 6.3 Sequence rules

The Tick maintains `seq` as the sequence number of its state:

- When sending `tick/read`, the host records a read floor: the greater of the previous read result's `seq` and the last update's `seq` received so far. The read result's `seq` **MUST** be at least this floor.
- The read result is not compared with updates arriving around it. A Tick may send an update immediately before or after a read response, carrying the same or an earlier state.

<a id="s6-4"></a>

### 6.4 Designing state

State is a summary that helps the Agent decide whether to act, not a copy of all available data. The following guidance is informative:

- Typical state **SHOULD** fit within roughly 4 KiB. Subscription delivery trims to a budget of 16 KiB in total, with 4 KiB per item when multiple items are delivered.
- Expose large content through query tools and name those tools in the state. For example, a `candles` tool can retrieve chart details for a time range.
- Declare chronological arrays in `state.series` so trimming preserves the newest end. Undeclared arrays retain their beginning and end.
- Cursor position, scrolling, selection, colors, and other presentation details are not business state.

<a id="s7"></a>

## 7. tick/call

<a id="s7-1"></a>

### 7.1 Request

```typescript
// method: "tick/call"
interface CallParams {
  /** A tool name from the catalog. */
  tool: string;
  /** Arguments matching the tool's input_schema; {} when it takes none. */
  args: object;
  /** The revision of the catalog the call was prepared and approved against. */
  revision: string;
}
```

<a id="s7-2"></a>

### 7.2 Handling a call

On receiving `tick/call`, the Tick:

1. **MUST** return -32004 without executing if `revision` differs from its current catalog.
2. **MUST** return -32602 without executing if the tool is unknown or `args` is invalid. The host validates arguments against `input_schema` before sending, but the Tick **SHOULD** validate them independently.
3. Executes the tool and returns its output as `result`, or an error as described in [Section 3.4](#s3-4).

The result may be any JSON value. It goes only to the calling client and does not change the state sequence number. If the tool changes state, the Tick emits a separate `tick/update` ([Section 8](#s8)).

<a id="s7-3"></a>

### 7.3 Timeouts and unknown outcomes

- `tick/call` has a 60-second timeout. After it expires, the host stops waiting and reports an unknown outcome to the client.
- The protocol has no cancellation message. A Tick may finish after timeout. A late response may be received or discarded, but it does not change the outcome already reported.
- The host **MUST NOT** automatically resend a call to a `mutates: true` tool. When timeout, session termination, or a violation leaves the outcome unknown, the client or user decides what to do next.
- Operations that may take more than 60 seconds **SHOULD** return immediately, continue in the background, and report progress and results through `tick/update`.

<a id="s7-4"></a>

### 7.4 Idempotency

Because the host does not automatically retry mutating calls, the Tick does not need retry deduplication. Tools that change a remote service **SHOULD** include enough information in the result or state to check whether the change took effect, such as an order ID or the new value.

<a id="s8"></a>

## 8. tick/update

<a id="s8-1"></a>

### 8.1 Notification

```typescript
// method: "tick/update"
// params: TickState
```

A Tick emits this notification when its business state changes. Parameters contain the complete state after the change.

<a id="s8-2"></a>

### 8.2 Requirements

Every `tick/update` **MUST** meet all of the following requirements. Otherwise the host ends the session as a protocol violation ([Section 3.6](#s3-6)):

- `data` is not null.
- `seq` is strictly greater than the `seq` of every previous `tick/update`.
- If present, `at` is a valid RFC 3339 timestamp.
- Encoded size does not exceed 512 KiB.

<a id="s8-3"></a>

### 8.3 What counts as an update

An update represents a business change, such as a new candle, a news item, or a value changed by a tool call.

Cursor movement, scrolling, repaints, resizing, and color changes **MUST NOT** emit updates.

The host retains only the latest state. At the subscription interval, it delivers that state to the client and reports how many updates were merged. Therefore:

- When output is blocked, a Tick **MAY** drop older updates that have not yet been written and send only the newest one.
- Update frequency **SHOULD NOT** exceed what the interface needs for refreshes.

<a id="s9"></a>

## 9. Recovery

Subscriptions deliver only the latest state, so intermediate changes may be merged away. Clients needing history can read it through the query tool named by `recover_with`:

- This tool **MUST** declare `mutates: false`.
- The host does not call it automatically. It includes the tool name with delivery, and the client decides whether to call it.
- A Tick without `recover_with` does not provide a way to recover missed changes through this mechanism.

<a id="s10"></a>

## 10. Security considerations

- **Permissions.** A Tick runs with the permissions of the user who started it and can access what that user can access. Users **SHOULD** run only Ticks they trust.
- **Approval relies on declarations.** The host uses `mutates` to determine whether approval is needed. Tick authors **MUST** declare it truthfully. Hosts and clients **SHOULD** show the tool name, arguments, and description so users can assess the call.
- **Catalog changes invalidate approval.** Approval is bound to `revision`. A call prepared against an old catalog is rejected with -32004, preventing a tool from changing meaning after approval.
- **Output is untrusted data.** State, tool results, and error messages enter the model context. Hosts and clients **MUST** treat them as data rather than instructions, and must not expand authorization based on their contents. Ticks **SHOULD NOT** put credentials, tokens, or sensitive personal information in state.
- **Standard-output integrity.** Non-protocol output ends the session. A Tick **SHOULD** redirect libraries that may print to standard output as early as startup.
- **Instance identity is not a credential.** `TICKLOOP_INSTANCE_ID` is only for logging and distinguishing runs, not authentication.

## Appendix A Example session

`→` marks a line written by the host to the Tick's standard input. `←` marks a line written by the Tick to standard output. Some lines are wrapped for readability; on the wire, each message occupies one line.

```text
→ {"jsonrpc":"2.0","id":1,"method":"tick/describe","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{"protocol":3,"kind":"example.counter","title":"Counter",
     "description":"Keyboard and tool calls share one value.","revision":"1",
     "tools":[{"name":"increment","description":"Add an integer to the counter.",
               "input_schema":{"type":"object","properties":{"amount":{"type":"integer"}},
                               "required":["amount"],"additionalProperties":false},
               "mutates":true},
              {"name":"history","description":"Read the last 256 changes.",
               "input_schema":{"type":"object","additionalProperties":false},"mutates":false}],
     "recover_with":"history","state":{"series":[{"path":"/recent","newest":"last"}]}}}
→ {"jsonrpc":"2.0","id":2,"method":"tick/read","params":{}}
← {"jsonrpc":"2.0","id":2,"result":{"seq":0,"data":{"value":0,"recent":[]}}}

  (the user approves increment with amount 2; the call carries revision "1")
→ {"jsonrpc":"2.0","id":3,"method":"tick/call","params":{"tool":"increment","args":{"amount":2},"revision":"1"}}
← {"jsonrpc":"2.0","method":"tick/update","params":{"seq":1,"data":{"value":2,"recent":[{"seq":1,"value":2}]}}}
← {"jsonrpc":"2.0","id":3,"result":{"value":2}}

  (a person types + in the Tick's view)
← {"jsonrpc":"2.0","method":"tick/update","params":{"seq":2,"data":{"value":3,"recent":[{"seq":1,"value":2},{"seq":2,"value":3}]}}}

  (a call prepared against an older catalog)
→ {"jsonrpc":"2.0","id":4,"method":"tick/call","params":{"tool":"increment","args":{"amount":1},"revision":"0"}}
← {"jsonrpc":"2.0","id":4,"error":{"code":-32004,"message":"the Tick Tool catalog changed; describe the Tick and prepare the call again"}}
```

## Appendix B Minimal implementation

This Tick uses only the Python standard library. It stores a single note and exposes one mutating tool. It satisfies the Tick-side MUST requirements of this specification, but does not process requests concurrently or draw an interface.

```python
#!/usr/bin/env python3
import json, sys

REVISION = "1"
TOOLS = [{"name": "set_note", "description": "Replace the note.", "mutates": True,
          "input_schema": {"type": "object", "properties": {"text": {"type": "string"}},
                           "required": ["text"], "additionalProperties": False}}]
seq, note = 0, None

class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

def send(frame):
    print(json.dumps({"jsonrpc": "2.0", **frame}), flush=True)

def handle(method, params):
    global seq, note
    if method == "tick/describe":
        return {"protocol": 3, "kind": "example.note", "title": "Note",
                "description": "One line of text.", "revision": REVISION, "tools": TOOLS}
    if method == "tick/read":
        return {"seq": seq, "data": None if note is None else {"note": note}}
    if method == "tick/call":
        if params.get("revision") != REVISION:
            raise RpcError(-32004, "the tool catalog changed")
        text = params.get("args", {}).get("text")
        if params.get("tool") != "set_note" or not isinstance(text, str):
            raise RpcError(-32602, "set_note takes a text string")
        seq, note = seq + 1, text
        send({"method": "tick/update", "params": {"seq": seq, "data": {"note": note}}})
        return {"saved": True}
    raise RpcError(-32601, f"unknown method {method}")

for line in sys.stdin:
    request = json.loads(line)
    try:
        send({"id": request["id"], "result": handle(request["method"], request.get("params", {}))})
    except RpcError as error:
        send({"id": request["id"], "error": {"code": error.code, "message": str(error)}})
```

## Appendix C Implementation checklist

- [ ] Write only protocol messages to standard output, one complete, newline-terminated line at a time, atomically across threads.
- [ ] Respond exactly once to each request with the matching `id`; never respond with `id: null`.
- [ ] Make `tick/describe` and `tick/read` side-effect-free and prompt.
- [ ] Change `revision` whenever the catalog changes. Reject mismatched revisions with -32004 without executing.
- [ ] Declare every mutating tool as `mutates: true`.
- [ ] Use “not executed” error codes only when the call has had no effect.
- [ ] Send complete, non-null state in every `tick/update`, with strictly increasing `seq`.
- [ ] Keep read sequence numbers from moving backwards or falling below the last emitted update.
- [ ] Do not emit updates for presentation changes such as cursor movement, scrolling, or repaints.
- [ ] Keep state within a few KiB, expose large content through query tools, and declare time series in `state.series`.
- [ ] Stay usable at 30 columns and `screen.min_rows` rows, declare `screen.max_rows` if more rows help, and redraw the whole screen after `SIGWINCH`.
- [ ] Use Backspace instead of Esc as the back key, and never require a double Esc; declare `bubble=on` and bubble `esc`, `up`, `down`, `left`, `right`, `tab` and `backtab` whenever the Tick does not use them.
- [ ] Run operations longer than 60 seconds in the background and report results through updates.
- [ ] Exit promptly on `SIGTERM` or standard-input EOF, without relying on shutdown cleanup.
