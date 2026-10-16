# Tick 协议规范

| 项 | 内容 |
| --- | --- |
| 协议版本 | 3 |
| 状态 | 草案；行为与本仓库 Tickloop 0.2 的实现一致 |
| 日期 | 2026-10-14 |
| 参考实现 | 宿主侧类型与校验 `tick/protocol`，Tick 侧 `tick/sdk`，示例 `tick/examples/counter.py` |

## 与 Agent 工具和包 Skill 的分工

协议保留三个请求方法 `tick/describe`、`tick/read`、`tick/call` 和一个通知 `tick/update`。Tickloop 分别通过 TickDescribe 查询能力、TickRead 读取当前状态、TickCall 调用声明的业务工具，TickUpdate 表示更新交付事件。TickRead 不重复返回 descriptor 或工具目录。TickManage 在 Agent／宿主侧管理生命周期、视图、终端输入和 Loop 的订阅关系，不新增 Tick 程序 RPC。包 Skill 只负责启动命令、参数和登录前提，运行实例的 descriptor 是当前能力目录的权威来源。

## 1. 概述

### 1.1 目的

Tick 协议规定一个终端程序如何把自己的状态、可调用的操作和业务上的变化提供给宿主，使 AI Agent 能够读取它、调用它，并在它发生变化时被唤醒继续工作。实现了本协议的程序称为**扩展 Tick**。

一个扩展 Tick 同时服务两方：人在它的终端界面里操作，Agent 经宿主通过本协议操作。两方面对的是同一个进程、同一份状态。

### 1.2 角色

- **Tick**：实现本协议的程序。它从标准输入读取请求，向标准输出写协议消息，在自己的控制终端上绘制界面。
- **宿主（Host）**：启动 Tick 并与之通信的一方。在 Tickloop 中，宿主是 Tickhost 为每个 Tick 启动的 launcher 进程及其背后的 Tickhost。
- **客户端**：通过宿主使用 Tick 的一方，通常是 Agent 的 Loop。客户端与宿主之间的接口不在本规范范围内。
- **视图**：显示 Tick 控制终端的界面，例如 Herdr Pane 或 Monitor 浮窗。一个 Tick 可以同时有多个视图，视图的打开与关闭不影响协议会话。

### 1.3 设计原则

1. **程序就是工具。** Tick 用一份工具目录声明自己能做什么；宿主不需要为每种 Tick 编写专门的代码。
2. **状态是最新的完整快照。** 每条更新携带完整的当前状态而不是增量，宿主只保留最新的一份。错过中间的更新不会让宿主得到错误的状态。
3. **修改可以审批。** 工具声明自己是否会修改外部状态；调用绑定在审批时看到的目录版本上，目录变化后旧的审批不再生效。
4. **结果未知时不重试。** 修改类调用一旦派发，超时或断线都不会被自动重发。
5. **实现门槛低。** 只用一门语言的标准库就能写出一个合规的 Tick。

### 1.4 与 LSP、MCP 的关系

本协议和 LSP 一样使用 JSON-RPC 2.0 并以标准输入输出为传输，区别是消息按行分隔，没有 `Content-Length` 报头（[第 3.1 节](#s3-1)）。

本协议的工具目录与 MCP 的 tools 相近：名称、描述和 JSON Schema 形式的输入。不同之处有三点：

- Tick 还有自己的状态：带序号，可以读取，变化时通知宿主。
- 工具调用绑定目录版本 `revision`。
- Tick 有一个人正在使用的终端界面，协议消息与界面严格分开。

本协议没有初始化握手中的能力协商：宿主通过 `tick/describe` 的结果了解 Tick 的全部能力。

## 2. 约定

### 2.1 规范性用语

本文中的“必须”“不得”“应当”“不应”“可以”分别对应 [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) 与 [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174) 中的 MUST、MUST NOT、SHOULD、SHOULD NOT 与 MAY，仅在加粗时具有规范含义。

没有加粗的说明、示例和“注”属于说明性内容。

### 2.2 类型表示

消息结构用 TypeScript 接口表示，写法与 LSP 规范相同：

- `?` 表示字段可以省略；
- `integer` 表示 -2^53+1 到 2^53-1 之间的整数，`uinteger` 表示 0 到 2^53-1 之间的整数；
- `object` 表示 JSON 对象；
- 时间是 [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339) 字符串；
- 指向 JSON 内部位置的路径是 [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) JSON Pointer。

```typescript
type integer = number;
type uinteger = number;
type JSONValue = string | number | boolean | null | JSONValue[] | { [key: string]: JSONValue };
/** A JSON Schema document whose top level is an object. */
type JSONSchema = { [key: string]: JSONValue };
```

### 2.3 字节与大小

本文所说的大小都是 UTF-8 编码后的字节数。消息体的大小按接收方重新编码后的结果计算（[第 3.5 节](#s3-5)）。

## 3. 基础协议

### 3.1 传输

宿主以子进程方式启动 Tick，并为它提供三个通道：

| 通道 | 用途 |
| --- | --- |
| 标准输入 | 宿主写给 Tick 的请求 |
| 标准输出 | Tick 写给宿主的响应与通知，只能用于协议消息 |
| 控制终端（标准错误也连到这里） | Tick 的界面，由视图显示 |

消息格式规则：

- 每条消息是一个 JSON 值，编码为 UTF-8，后接一个换行符 `\n`（U+000A）。
- 消息内部**不得**含有未转义的换行符。
- Tick **不得**向标准输出写任何非协议内容，包括调试输出、进度条和第三方库的打印。
- 最后一条消息之后，Tick **必须**写出结尾的换行符；没有换行就结束的输出视为违规（[第 3.6 节](#s3-6)）。
- Tick **必须**把每条消息整行写出：多个线程写标准输出时，一条消息的字节**不得**与另一条交错。
- 写到标准错误的内容会显示在视图里。Tick **不应**把诊断日志写到标准错误；需要日志时，**应当**写到 Tick 自己的日志文件。

> 注：本协议不使用 LSP 的 `Content-Length` 报头。按行分隔使最简单的实现也能用逐行读取完成解析。

### 3.2 消息

所有消息都遵循 [JSON-RPC 2.0](https://www.jsonrpc.org/specification)，并且必须带 `"jsonrpc": "2.0"`。不支持批量消息。

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

方向规则：

- 请求只从宿主发往 Tick。Tick **不得**向宿主发送请求。
- Tick 发往宿主的通知只有 `tick/update`。宿主收到其他通知时**必须**忽略它并继续会话，**可以**记录或计数。
- 宿主不向 Tick 发送通知。

关于 `id`：

- 宿主分配的 `id` 是无符号整数。
- Tick 对每个请求**必须**恰好回复一次，`id` 与请求相同。
- 响应**必须**含有 `result` 与 `error` 其中之一，且只能有一个。
- Tick **不得**发送 `id` 为 `null` 的响应。请求无法解析或取不到 `id` 时，Tick **必须**丢弃该行且不回复，**可以**记录到自己的诊断日志（宿主不会发送这样的请求）。

> 注：JSON-RPC 2.0 允许对无法解析的请求回复 `id: null` 的错误。本协议的宿主只发送合法请求，所以禁止这种响应，以便宿主把它当作明确的违规处理。

方法名以 `tick/` 开头的名字保留给本协议。

### 3.3 并发与顺序

- 宿主**可以**在上一个请求得到响应之前发送下一个请求。
- 宿主同时在途的请求（包括 `tick/describe` 与 `tick/read`）不超过 8 个（[第 3.5 节](#s3-5)）。
- Tick **可以**并发处理请求，并**可以**以任意顺序回复；宿主按 `id` 匹配响应。
- `tick/describe` 与 `tick/read` **应当**立即作答，不等待正在执行的工具调用。

`tick/update` 可以在任何时候发出，包括初始化完成之前、在途请求的响应之前或之后。宿主按到达顺序检查更新的序号（[第 6.3 节](#s6-3)）。

### 3.4 错误码

Tick 在响应中使用下列错误码。宿主根据错误码判断这次调用有没有产生效果。

| 代码 | 名称 | 含义 | 宿主的理解 |
| --- | --- | --- | --- |
| -32600 | InvalidRequest | 请求不是合法的 JSON-RPC 请求 | 未执行 |
| -32601 | MethodNotFound | 方法不存在 | 未执行 |
| -32602 | InvalidParams | 参数不对，或工具不存在 | 未执行 |
| -32001 | UnsupportedJsonRpc | 请求的 `jsonrpc` 不是 `"2.0"` | 未执行 |
| -32003 | FrameTooLarge | 请求超过 Tick 接受的大小 | 未执行 |
| -32004 | RevisionMismatch | `tick/call` 的 `revision` 与当前目录不符 | 未执行，目录已变化 |
| -32000 | ToolFailed | 工具已运行，但失败了 | 已执行 |
| 其他 | — | 由 Tick 定义 | 已执行 |

- Tick 用“未执行”类错误码回复的调用，**必须**没有产生任何效果。
- 工具运行到一半失败、可能已经产生部分效果时，Tick **必须**使用 -32000 或其他“已执行”类错误码。
- `message` **应当**是一句能直接给人看的说明。`data` **可以**携带结构化的细节。

### 3.5 限额

| 项目 | 上限 | 超出时 |
| --- | --- | --- |
| 单个请求 | 512 KiB | 宿主不发送，Tick 收不到 |
| `tick/describe` 的结果 | 512 KiB（编码后） | 初始化失败；运行中则这次请求失败 |
| 单条 `tick/update` | 512 KiB（编码后） | 违规，会话结束 |
| 单个响应行 | 8 MiB | 违规，会话结束 |
| 单个响应的 `result` | 8 MiB（编码后） | 这次调用的结果按“未知”处理，会话继续；Tick 自己发现结果超限时回 -32000 |
| 同时在途的请求（包括 `tick/describe` 与 `tick/read`） | 8 个 | 宿主不发送，向客户端报告忙 |

- 编码后大小指宿主解析后重新编码的字节数。数字的写法不同，重新编码后可能变长（例如 `1e15` 编码为 `1000000000000000.0`），Tick **应当**为此留出余量。
- 宿主**可以**设置更严格的总量限制，例如所有在途调用的响应缓冲合计 64 MiB。

### 3.6 违规

下列情况是协议违规。宿主**必须**结束会话：在途请求全部按结果未知处理，并结束 Tick 进程。

- 一行不是合法 JSON，或 `jsonrpc` 不是 `"2.0"`；
- 一行既没有整数 `id` 也没有 `method`（包括 `id` 为 `null` 或字符串的响应）；
- 输出在没有换行符的情况下结束；
- 单行超过 8 MiB；
- 不合规的 `tick/update`（[第 8.2 节](#s8-2)）。

下列情况只让对应的请求失败，会话继续：

- 响应同时含有或同时缺少 `result` 与 `error`；
- `tick/read` 的结果违反序号规则（[第 6.3 节](#s6-3)）；
- `result` 编码后超过 8 MiB。

宿主收到与在途请求都不匹配的 `id` 时，**必须**丢弃该响应。

## 4. 生命周期

### 4.1 启动

宿主以用户给出的命令启动 Tick：

- `argv` 与命令相同，`argv[0]` 为解析后的绝对路径；
- 工作目录为打开 Tick 时指定的目录；
- 环境中带有下列变量：

| 变量 | 内容 |
| --- | --- |
| `TICKLOOP_INSTANCE_ID` | 本次运行的实例身份，形如 `hosted:<id>` |
| `TERM` | `xterm-256color` |

- `TICKLOOP_INSTANCE_ID` 只供 Tick 写日志或区分自己的多个运行，不出现在任何协议消息中，也不是凭据。
- Tick **可以**根据它是否存在来判断自己是被宿主启动的，还是由人在终端里直接运行的。
- 其他以 `TICKLOOP_` 开头的变量属于宿主内部，Tick **不得**依赖它们。

Tick 运行在一个以视图终端为控制终端的会话中，处于前台进程组：

- 初始尺寸为 120 列 × 30 行；
- 视图尺寸变化时，宿主调整终端尺寸，Tick 由内核收到 `SIGWINCH`；
- 尺寸变化不经过协议。

### 4.2 初始化

启动后，宿主依次发送：

1. `tick/describe`，检查结果（[第 5.3 节](#s5-3)）；
2. `tick/read`，取得初始状态。

规则：

- 每个请求**必须**在 10 秒内得到响应。
- 从启动到初始化完成，总共不超过 10 秒。
- 任何一步失败、超时，或结果不合规，初始化失败：宿主结束 Tick 进程，实例不会出现在客户端中，失败原因与视图屏幕上的前 256 个字符会一起报告给客户端。
- Tick **可以**在初始化完成前发出 `tick/update`，宿主照常接收。

### 4.3 版本

`tick/describe` 的结果中 `protocol` 字段声明 Tick 实现的协议版本。宿主请求中不携带版本。

- 宿主只接受自己实现的版本。版本不符时，初始化失败，错误信息说明两边的版本，例如 `unsupported Tick protocol version 2; this host speaks Tick protocol 3`。
- Tick 收不到这条错误；它显示在视图里，并报告给客户端。
- 不改变已有字段含义的扩展（例如新增可选字段）不改变协议版本。
- 宿主**必须**忽略 `tick/describe` 结果和状态中不认识的字段；Tick **应当**忽略请求参数中不认识的字段。

### 4.4 运行

初始化完成后，宿主在需要时发送请求：

| 时机 | 请求 |
| --- | --- |
| 客户端读取 Tick | `tick/read`，随后 `tick/describe` |
| 客户端准备调用工具 | `tick/describe`，取得目录与 `revision` |
| 客户端调用工具（审批之后） | `tick/call` |
| 建立或恢复订阅的观察流 | `tick/describe` |
| 交付时缓存中没有可用的状态 | `tick/read` |

- 宿主**可以**在任何时候发送 `tick/describe` 与 `tick/read`，次数不限。
- 这两个方法**必须**没有副作用，并且**应当**在几毫秒内作答。

### 4.5 结束

宿主结束 Tick 时，向 Tick 所在的进程组发送 `SIGTERM`，之后**可以**不经宽限直接发送 `SIGKILL`。宿主不保证在结束前关闭标准输入。

- Tick **不得**依赖退出前的清理来保存重要数据。
- Tick 读到标准输入结束（EOF）时，**应当**等在途的调用完成后退出。
- Tick 自行退出时，会话随之结束：宿主把在途请求按结果未知处理，客户端看到实例已结束。退出码不属于协议。

## 5. tick/describe

### 5.1 请求

```typescript
// method: "tick/describe"
interface DescribeParams {}
```

宿主发送 `params: {}`。

### 5.2 结果

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
```

### 5.3 校验规则

宿主**必须**拒绝不满足下列条件的结果：

- `protocol` 等于宿主实现的版本；
- `kind` 与 `title` 去掉首尾空白后不为空；
- `revision` 不为空字符串；
- 每个工具的 `name` 去掉首尾空白后不为空，并且在目录内唯一；
- 每个工具的 `input_schema` 是对象，并且是合法的 JSON Schema；
- `recover_with` 如果存在，指向目录中一个 `mutates` 为 `false` 的工具；
- `state.series` 中每个 `path` 为空字符串或以 `/` 开头，并且互不相同；
- 结果编码后不超过 512 KiB。

### 5.4 `kind`

`kind` 是程序自报的类型。它与实例身份的前缀（`hosted:`、`shell:`、`artifact:`）无关，同一个 `kind` 可以同时有多个实例。

`kind` **应当**是以点分隔的小写名字，第一段标明发布者，例如 `example.counter`、`acme.orderbook`。

### 5.5 `revision`

`revision` 命名当前的工具目录：

- 目录中任何工具的名称、描述、输入 Schema 或 `mutates` 发生变化时，Tick **必须**换用一个新的 `revision`。
- 目录在运行期间不变的 Tick **可以**一直使用同一个值。
- `revision` 是不透明字符串，宿主只比较相等与否。

### 5.6 `mutates`

- 一次调用可能改变调用结果以外的任何东西时，Tick **必须**把该工具声明为 `mutates: true`。这些东西包括：Tick 自己的状态、界面上的业务内容、文件，以及远端服务。
- 只读取信息的查询工具声明为 `mutates: false`。

宿主依据 `mutates` 决定调用前是否需要审批，以及结果未知时能否重试。把修改类工具声明为 `false` 会绕过用户的审批（[第 10 节](#s10)）。

## 6. tick/read

### 6.1 请求

```typescript
// method: "tick/read"
interface ReadParams {}
```

### 6.2 结果

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

- `data` 是 Tick 当前的完整业务状态，不是相对于上一次的增量。
- `data` 为 `null` 表示还没有内容，例如数据仍在加载。
- 如果 `at` 存在，它**必须**是合法的 RFC 3339 时间。

### 6.3 序号规则

`seq` 是 Tick 为自己的状态维护的序号：

- 宿主在发送 `tick/read` 时记下“读取下限”：上一次 `tick/read` 结果的 `seq` 与此前已收到的最后一条 `tick/update` 的 `seq` 中较大的一个。`tick/read` 结果的 `seq` **必须**不小于读取下限。
- `tick/read` 结果不与它前后到达的 `tick/update` 比较：Tick 可以在回复读取之前或之后紧接着发出更新，携带相同或更早的状态。

### 6.4 状态的设计

状态是供 Agent 判断要不要行动的摘要，不是全部数据。下面是说明性的建议：

- 典型状态**应当**在 4 KiB 左右或更小。宿主交付订阅时会按预算裁剪：所有条目合计 16 KiB，多个条目时每条 4 KiB。
- 大块内容放在查询工具后面，状态中写明对应的工具名。例如 K 线明细用 `candles` 工具按区间读取。
- 状态中按时间排列的数组应当在 `state.series` 中声明，宿主裁剪时会保留最新的一端。没有声明的数组会保留首尾。
- 光标位置、滚动、选中项、配色等界面细节不属于状态。

## 7. tick/call

### 7.1 请求

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

### 7.2 处理

Tick 收到 `tick/call` 后：

1. `revision` 与当前目录不同时，**必须**以 -32004 回复且不执行。
2. 工具不存在，或 `args` 不合要求时，**必须**以 -32602 回复且不执行。宿主在发送前已按 `input_schema` 检查过 `args`，但 Tick **应当**自行检查，不依赖宿主。
3. 执行工具，以 `result` 回复其结果；失败时按[第 3.4 节](#s3-4)回复错误。

结果可以是任何 JSON 值。它只返回给发起调用的客户端，不改变状态序号；工具改变了状态时，Tick 另外发出 `tick/update`（[第 8 节](#s8)）。

### 7.3 超时与结果未知

- `tick/call` 的超时为 60 秒。超时后宿主不再等待，并向客户端报告“结果未知”。
- 本协议没有取消消息。超时之后 Tick 仍可能完成调用，迟到的响应会被宿主接收或丢弃，但不会改变已报告的结果。
- 宿主**不得**自动重发 `mutates: true` 工具的调用。超时、会话结束或违规导致结果未知时，由客户端或用户决定下一步。
- 运行时间可能超过 60 秒的操作，**应当**设计为立即返回、在后台继续，并通过 `tick/update` 报告进展与结果。

### 7.4 幂等

宿主不会自动重试修改类调用，所以 Tick 不需要为重试去重。会修改远端服务的工具，**应当**在结果或状态中给出足以核对是否已生效的信息，例如订单号或新值。

## 8. tick/update

### 8.1 通知

```typescript
// method: "tick/update"
// params: TickState
```

Tick 的业务状态发生变化时发出。参数是变化之后的完整状态。

### 8.2 规则

每条 `tick/update` **必须**满足下列条件，否则宿主按违规结束会话（[第 3.6 节](#s3-6)）：

- `data` 不为 `null`；
- `seq` 严格大于此前任何一条 `tick/update` 的 `seq`；
- `at` 如果存在，是合法的 RFC 3339 时间；
- 编码后不超过 512 KiB。

### 8.3 什么是一次更新

一次更新表示业务上的变化，例如新的一根 K 线、一条新资讯，或工具调用改变的值。

光标移动、滚动、重绘、尺寸变化、配色变化**不得**发出更新。

宿主只保留最新的一份状态，订阅到期时把最新状态交给客户端，并说明合并了几次更新。所以：

- Tick **可以**在输出受阻时丢弃尚未写出的旧更新，只写出最新的一条；
- 更新的频率**不应**高于界面刷新的需要。

## 9. 恢复

订阅只交付最新状态，中间的变化可能被合并掉。需要完整历史的客户端通过 `recover_with` 指定的查询工具读取：

- `recover_with` 指向的工具**必须**是 `mutates: false`。
- 宿主不会自动调用它，只把它的名字随交付告诉客户端，由客户端决定是否调用。
- 没有声明 `recover_with` 的 Tick 表示错过的变化无法补回。

## 10. 安全考虑

- **权限。** Tick 以启动它的用户的权限运行，可以访问该用户能访问的一切。用户**应当**只运行自己信任的 Tick。
- **审批依赖 Tick 的声明。** 宿主按 `mutates` 决定是否需要审批。Tick 作者**必须**如实声明；宿主和客户端**应当**向用户展示工具的名称、参数与描述，供用户判断。
- **目录变化使审批失效。** 审批绑定 `revision`，目录变化后旧的调用以 -32004 被拒绝。这防止工具在审批之后被替换成另一个含义。
- **输出是不可信的数据。** 状态、工具结果和错误消息会进入模型的上下文。宿主与客户端**必须**把它们当作数据而不是指令，不得因为其中的文字扩大授权。Tick **不应**在状态中放入凭据、令牌或个人敏感信息。
- **标准输出的完整性。** 任何写到标准输出的非协议内容都会使会话结束。Tick **应当**在启动时就把可能打印的库重定向到别处。
- **实例身份不是凭据。** `TICKLOOP_INSTANCE_ID` 只用于日志与区分运行，不能用来认证。

## 附录 A 示例会话

`→` 是宿主写到 Tick 标准输入的行，`←` 是 Tick 写到标准输出的行。为便于阅读，部分行折成多行，实际传输时每条消息占一行。

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

## 附录 B 最小实现

下面的 Tick 只用 Python 标准库，保存一行笔记，并提供一个修改类工具。它满足本规范对 Tick 的全部“必须”要求，但没有并发处理请求，也没有绘制界面。

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

## 附录 C Tick 实现检查清单

- [ ] 标准输出只写协议消息，每条一行，以换行结尾，多线程写入时整行原子。
- [ ] 每个请求恰好回复一次，`id` 与请求相同，从不回复 `id: null`。
- [ ] `tick/describe` 与 `tick/read` 没有副作用，立即作答。
- [ ] 目录的任何变化都换用新的 `revision`，`revision` 不符时以 -32004 拒绝且不执行。
- [ ] 修改类工具都声明 `mutates: true`。
- [ ] “未执行”类错误码只用于确实没有产生效果的调用。
- [ ] 每条 `tick/update` 携带完整状态，`data` 非空，`seq` 严格递增。
- [ ] `tick/read` 的 `seq` 不倒退，不小于已发出的最后一条更新。
- [ ] 界面变化（光标、滚动、重绘）不发更新。
- [ ] 状态保持在几 KiB 以内，大块内容放在查询工具后面，时间序列在 `state.series` 中声明。
- [ ] 超过 60 秒的操作改为后台进行，用更新报告结果。
- [ ] 收到 `SIGTERM` 或标准输入结束时尽快退出，不依赖退出前的清理。
