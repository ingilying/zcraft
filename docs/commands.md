# 命令系统

命令入口与聊天、网络协议分离：

- `src/command/mod.rs`：注册定义、权限/来源、执行接口与结果。
- `src/command/parse.rs`：参数类型、引号与错误位置。
- `src/command/builtins.rs`：`help`、`list`、`me`、`msg`（别名 `tell`）。
- `src/protocol/packets/play_chat.rs`：把同一注册表转换为协议 767 命令树。
- `src/handler/play/chat.rs`：玩家就绪检查、限频及调用，不再解析具体命令。

## 注册新命令

注册表构建后不可修改；在启动前注入 `PlayService`，无需修改聊天处理器或协议编解码：

```rust
use zcraft::{
    command::{ArgumentKind, Command, CommandRegistryBuilder, CommandResult},
    handler::play::PlayService,
};

fn custom_play_service() -> Result<PlayService, Box<dyn std::error::Error>> {
let mut builder = CommandRegistryBuilder::with_builtins();
builder.register(
    Command::new("count", "return a numeric result", |call, _host| {
        Ok(CommandResult::success(
            call.arguments.integer("value").expect("declared argument"),
        ))
    })
    .alias("result")
    .permission(2)
    .argument("value", ArgumentKind::Integer { min: 0, max: 100 }),
);
let registry = builder.build().expect("valid command definitions");
Ok(PlayService::new()?.with_commands(registry))
}
// 把返回的 PlayService 放入 ServerContext，然后调用 serve_with_context。
```

`Command::new` 接收不捕获环境的执行函数/闭包。函数取得 `Invocation`（来源、类型化参数、注册表）及 `CommandHost`。Host 目前提供列举玩家、玩家动作消息和私聊三项能力；新增有副作用的游戏命令时，应扩展受控 Host 能力及其服务端实现，而非从命令解析层访问 Session 或 socket。纯计算命令不需要修改 Host。

执行函数是同步的，不应执行阻塞 I/O 或长时间计算。返回反馈应保持简短；反馈以系统消息发送时仍受 NBT 字符串/协议长度限制。

## 一份定义，多处使用

每项定义包含名称、别名、说明、最低权限、是否仅限玩家、参数和执行函数：

- `parse` 根据定义读取并校验参数。
- `execute` 在执行函数前检查来源和权限。
- `help` 自动生成当前来源可用的帮助与用法。
- `DeclareCommands::new` 生成相同来源可用的客户端命令树。
- 别名复用参数路径和执行函数，不能绕过权限。

构建阶段拒绝重名/冲突别名、非法名称、重复参数名、倒置整数边界和不处于末尾的贪婪参数。最多 1024 个协议节点，每个命令最多 16 个参数。

## 参数与解析

当前每条命令是一条有序的**必填参数路径**，支持：

| 类型 | 语义 | 协议解析器 |
|---|---|---|
| `Word` | 单词，字符集为 ASCII 字母数字及 `_-.+` | `brigadier:string` / SINGLE_WORD |
| `String` | 单词或双引号字符串，支持 `\"`、`\\` 转义及引号内 Unicode | `brigadier:string` / QUOTABLE_PHRASE |
| `Greedy` | 剩余文本，保留内部空格，必须位于末尾 | `brigadier:string` / GREEDY_PHRASE |
| `Integer { min, max }` | 带上下界的 i32 | `brigadier:integer` |
| `Bool` | `true` 或 `false` | `brigadier:bool` |

支持可选前导 `/`，参数间使用 ASCII 空格。拒绝尾随多余参数、换行、格式控制符和方向覆盖字符。`CommandError` 包含错误说明、原始输入的 UTF-8 **字节**范围及用法。面向网络玩家的错误是私有反馈，不会广播。

核心输入最多 4096 字节；玩家聊天包仍执行独立的 256 UTF-16 单元限制。框架不是完整 Brigadier：暂不支持可选参数、分支子命令、选择器、相对坐标或动态补全建议。

## 来源、权限与反馈

`CommandSource` 是**可信服务端输入**：

- `Player(uuid)`：玩家身份；网络入口仅从认证会话取 UUID，权限固定为 0，位置来自服务端 Play 状态。
- `Console`：程序化服务端调用，可明确赋予权限。
- `CommandBlock`：预留的非玩家来源，通过 `Origin { world, position }` 提供上下文。

没有 OP 系统或权限热更新。不得把客户端传来的 UUID、权限等级或命令方块位置直接填入来源。隐藏客户端命令树不能代替服务端权限检查，二者都执行检查。

调用不需要 Session 或网络连接：

```rust
use zcraft::command::{CommandSource, Origin, SourceKind};

let source = CommandSource {
    kind: SourceKind::CommandBlock,
    permission: 2,
    origin: Some(Origin {
        world: "minecraft:overworld".into(),
        position: [8.0, 64.0, 8.0],
    }),
};
// let result = server_context.execute_command(&source, "list")?;
```

也可直接用 `CommandRegistry::execute` 和自己的 `CommandHost`，用于无服务器测试或嵌入。

`CommandResult` 包含 `success`、数值 `value` 及私有 `feedback`；语法、权限、普通业务失败返回失败结果，底层 I/O 错误保留为 `io::Error`。`list` 的数值结果为可参与聊天的在线玩家数。`help`/`list` 支持所有来源，`me`/`msg`/`tell` 仅支持玩家；目标不可用会返回失败结果，私聊不会泄露给第三方。

玩家入口保留原有聊天限频和设置。核心不依赖玩家限频；未来命令方块的 tick 调度、执行总预算、条件链、输出保存及反馈去向必须由调用方实现，本次没有实现这些机制，也没有提供控制台读取循环。

## 验证

- `src/command/tests.rs`：类型、引号、Unicode、错误范围、注册约束、权限、别名、协议树。
- `tests/commands.rs`：注入自定义命令，在真实 `ServerContext` 中用控制台/模拟命令方块调用，无需连接。
- `tools/check-chat.cjs`：独立客户端解码命令树并验证原有聊天与私聊行为。
