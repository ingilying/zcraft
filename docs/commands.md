# 命令系统

目标基线是 Java 1.21.1（协议 767）随附的 **Brigadier 1.3.10**。已实现第一阶段的命令树核心，以及第二阶段的来源上下文和有界执行队列；仍不是完整的 Minecraft 命令运行环境。

- `src/command/mod.rs`：注册、命令来源、Host 与玩家/控制台适配层。
- `src/command/tree.rs`：不可变节点树、重定向图、解析结果、执行与补全。
- `src/command/parse.rs`：UTF-16 reader、内建参数、自定义解析器接口。
- `src/command/source.rs`、`runtime.rs`：来源变换、反馈回调、有界任务队列与执行帧。
- `src/command/builtins.rs`：保留 `/help`、`/list`、`/me`、`/msg`、`/tell`，未新增内置命令。
- `src/protocol/packets/play_chat.rs`：从同一注册表生成客户端命令树。

核心语义参考官方 [Brigadier](https://github.com/Mojang/brigadier) 1.3.10 源码。相关 MIT 许可见 [第三方许可](third-party/brigadier-LICENSE)。

## 注册

简单命令使用 `Command::new(...).argument(...).alias(...)` 后交给 `CommandRegistryBuilder::register`；复杂命令通过节点 API 注册。节点的 executor 不依赖叶节点，因而可表达默认行为和可选参数。

```rust
use zcraft::{
    command::{ArgumentKind, CommandNode, CommandRegistryBuilder, CommandResult, NodeId},
    handler::play::PlayService,
};

fn custom_play_service() -> Result<PlayService, Box<dyn std::error::Error>> {
    let mut builder = CommandRegistryBuilder::with_builtins();
    let demo = builder.add_node(
        NodeId::ROOT,
        CommandNode::literal("demo").executes(|_, _| Ok(CommandResult::success(1))),
    )?;
    let count = builder.add_node(demo, CommandNode::literal("count"))?;
    builder.add_node(
        count,
        CommandNode::argument("value", ArgumentKind::Integer { min: 0, max: 100 })
            .executes(|call, _| {
                Ok(CommandResult::success(call.arguments.integer("value").unwrap()))
            }),
    )?;
    Ok(PlayService::new()?.with_commands(builder.build()?))
}
```

将返回的服务放入 `ServerContext`，再调用 `serve_with_context`。树冻结后不可修改。同一父节点下重名时复用节点，保留最初类型、要求及重定向；新 executor 可覆盖旧 executor。调用方可以在返回的节点上继续挂载新的分支。`find_node`/`path` 查找路径，`find_ambiguities` 通过类型示例提示相邻分支的重叠。

## 解析与执行

- `registry.dispatch(input, source)` 返回可复用的 `ParseResults`，即使输入不完整也能检查游标、已解析路径、参数范围、候选错误和补全。
- `dispatch_at` 从指定 UTF-16 游标开始。核心接收原样输入，不移除 `/` 或修剪空格。`ServerContext::execute_command` 是服务端便利适配器，处理前导 `/`、首尾空白和 4096 字节上限；网络聊天入口另行检查字符与 256 UTF-16 码元限制。
- 支持六种内建参数解析器：布尔、i32、i64、f32、f64、字符串（单词、单/双引号或剩余文本）。`ArgumentParser` 可返回任意 `Send + Sync` 类型，使用 `Arguments::custom` 取值。无协议描述的自定义类型只在服务端出现。
- `ParseResults::execute` 接受 `CommandHost`，无需 Session 或 socket。`CommandResult` 包含成功状态、整数值与反馈。执行函数不应阻塞 I/O。回调返回的 `io::Error` 不参与分叉的普通命令失败处理。
- `requires` 给节点设置来源谓词；`redirects` 建立上下文链，`forks` 为子链提供多个来源。普通重定向汇总值，分叉汇总成功次数。`result_consumer` 观察单次执行结果。
- 重定向进入目标**子树**，不重新解析目标节点自身。命令注册者必须为别名/入口设适当的前置要求；便捷别名继承原命令权限。来源修改器是可信服务端代码，不能直接使用客户端提供的权限值。
- `all_usage`、`smart_usage` 提供树用法。`ParseResults::suggestions(cursor)` 执行同步 provider；`suggestions_async(cursor)` 同时轮询独立异步 provider，同一节点没有异步 provider 时回退到同步版本。两者都按节点权限过滤，使用 UTF-16 替换范围，拒绝越界/切开代理对的范围，合并后排序去重。异步 provider 出错则跳过，取消会丢弃待完成的 future。

服务端策略限制最多 1024 个节点，默认最多 8192 次分支访问、128 层解析和 256 个派生来源。预算不足显式失败，不冒充语法错误。服务端入口在其上使用独立的来源栈和帧/队列运行时；同步执行，不做跨 tick 调度。

## 来源与执行队列

`ServerContext::run_command` 接受 `SourceStack`，支持位置、旋转、维度比例、执行实体及脚/眼锚点变换；
执行实体与原认证身份分离，提供静默反馈和组合结果回调。Executor 可通过 `host.execution()`
排入同帧或子帧任务、返回帧结果或取消整条链，预算在所有任务间共享。
默认成本 128、派生来源 64、待执行任务 64、帧深度 16；玩家命令绑定当前 Play 生命周期，
重新配置或重连不会复活旧链。完整语义、调用示例及与原版的差异见 [执行运行时](command-runtime.md)。

玩家，由认证会话提供 UUID、位置和固定权限 0；控制台及未来命令方块可由服务端构造 `CommandSource`，携带权限及 `Origin { world, position }`。`/help` 和 `/list` 可程序化调用；`/me`、`/msg`、`/tell` 仅供玩家。命令方块本身、OP 与权限热更新均未实现。

在线补全：协议 767 的 `0x0b` 请求与 `0x10` 响应保留事务 ID 和 UTF-16 替换范围，支持可选 NBT 提示。带 `.suggests(...)` 或 `.suggests_async(...)` 的可序列化参数节点声明 `minecraft:ask_server`；其他节点不声明。`suggests_async` 接收返回 `SuggestionFuture` 的函数（例如 `|ctx| Box::pin(async move { Ok(vec![ctx.suggest("example")]) })`）。同步和异步 provider 都应为可信、非阻塞代码，不应自行启动无法取消的工作。

每个就绪 Play 连接每 200 ms 最多处理一次补全（超频直接丢弃），与聊天限频独立；核心输入上限为 4096 字节，返回最多 128 项，单项文本和提示各最多 1024 字节。连接内等待上限为 100 ms，超时取消本次补全并返回空结果；不启动后台任务，重新配置期间不处理补全。

尚未完成：Minecraft 来源栈全部方法、跨 tick/命令方块与函数调度、48 种 Minecraft 专用参数及依赖世界的求值。Rust 字符串不保留未配对代理项；极端浮点错误的 Java 数字显示细节未完整移植。当前差分覆盖不是对所有输入完全兼容的证明。

## 验证

默认 Rust 测试读取 `tests/fixtures/brigadier-1.3.10.tsv`，无需 Java。参考工具从官方 1.21.1 服务端 JAR 抽取准确版本，并对照 `META-INF/libraries.list` 验证嵌套 JAR 的 SHA-256。可选测试重新运行官方 Java dispatcher，确保 fixture 未过期：

```sh
cargo test --locked --test brigadier
ZCRAFT_BRIGADIER_SERVER_JAR=target/zcraft-data/1.21.1/server.jar \
  cargo test --locked --test brigadier regenerate_against_reference_jar -- --ignored
```

覆盖 106 组输入/权限组合：解析游标、错误文本及位置、执行结果、建议文本及替换范围；另有 51 条普通与智能用法。独立 Minecraft 协议客户端测试验证 `tell` 的真实 redirect 节点及原有聊天行为。
