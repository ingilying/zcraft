# Minecraft 命令执行运行时（第二阶段）

在 Brigadier 解析器之上提供 `SourceStack` 和有界工作队列。玩家聊天入口与
`ServerContext::execute_command` 已接入该运行时；不是仅供测试的旁路接口。

- `src/command/source.rs`：可信来源、上下文变换和输出策略。
- `src/command/runtime.rs`：执行预算、任务队列、帧、取消及结果回调。
- `tests/command_runtime.rs`：变换、队列顺序、预算、回调、取消测试。

结构参考 Java 1.21.1 的
[CommandSourceStack](https://mappings.dev/1.21.1/net/minecraft/commands/CommandSourceStack.html)、
[ExecutionContext](https://mappings.dev/1.21.1/net/minecraft/commands/execution/ExecutionContext.html)
和 [Frame](https://mappings.dev/1.21.1/net/minecraft/commands/execution/Frame.html)。
此层是 Rust 服务端适配，不是 Minecraft 专有源码的移植，也尚不等于完整原版执行环境。

## 两层接口

- `registry.dispatch(...).execute(host)`：Brigadier 核心语义，用于官方差分测试。
- `registry.run(input, stack, host, limits)`：Minecraft 运行时，返回 `ExecutionReport`。
- `server.run_command(stack, input)`：使用默认预算与绑定来源会话的服务器 Host。
- `server.execute_command(source, input)`：兼容便捷接口，将普通 `CommandSource` 包装为
  `SourceStack`，返回报告中的 `CommandResult`。

运行时处理适配器中的前导 `/`、输入长度和字符策略。核心解析器仍不修改原始语法。
调用方需要保留完整 `abort` 信息时使用 `run_command`，不要只取 `result`。

## 来源上下文

`SourceStack` 通过消费/克隆自身的方法生成新上下文，不修改父来源：

- `authenticated_player()` / `source().kind`：原始调用身份，不能被执行实体替换。
- `with_entity`：仅改变执行实体及眼高，不伪造发送者，不自动传送坐标。
- `positioned`、`with_position`、`with_rotation`：位置及 `[yaw, pitch]`（度）。
- `with_anchor(Feet | Eyes)`、`anchored_position()`：眼锚点按实体眼高偏移。
- `in_dimension`：调用方必须提供真实 `Dimension { id, coordinate_scale }`；X/Z
  乘旧维度比例除新维度比例，Y 不变。未知比例、非有限坐标等返回 `SourceError`。
- `with_permission` 与 `with_maximum_permission`：调整权限，但不能超过可信入口赋予的上限。
- `silent`、`with_feedback_destination(Caller | Discard)`：只控制反馈，不撤销游戏副作用或结果回调。
- `with_callback`：按添加顺序组合 `Arc<dyn Fn(bool, i32) + Send + Sync>` 回调。

网络来源由认证 UUID、当前服务端位置/旋转及眼高构造，权限固定为 0。
来源修改器只能改变执行上下文：运行时保留父调用身份，且权限不得超过父级当前权限。
这是本服务端的安全策略，比直接允许可信 Java 代码随意提权更保守。

维度元数据由调用方提供，目前不会自动创建或查询尚未实现的维度。
相对/局部坐标参数、实体选择器和世界查找属于第三阶段。

## 队列与帧

Executor 通过 `host.execution()` 获取窄接口：

- `enqueue(input)`：同一帧排入后续命令。
- `enqueue_frame(input, source, callback)`：创建子帧，继承原调用身份与权限/静默约束。
- `return_from_frame(success, value)`：返回本帧结果并丢弃其剩余任务及后代。
- `cancel()`：终止整条链；不会回滚此前发生的副作用。

命令函数返回后才执行新任务。同批入队保持调用顺序；当前任务新排入的工作先于
之前已排队的工作。例如 `start → [branch, last]`，`branch → [leaf]` 的实际顺序是
`start, branch, leaf, last`，不使用递归 Rust 调用执行命令链。

同一 Host 内禁止另起 `registry.run` 来重置预算。嵌套命令必须入队。
可信扩展也不得自行创建新 Host 递归调用、阻塞线程或在回调中做无界工作；
同步预算不能抢占这样的扩展代码。

默认每条链的策略：

| 预算 | 默认值 |
|---|---:|
| 命令 dispatch、重定向应用、executor 调用的合计成本 | 128 |
| 重定向累计派生来源数 | 64 |
| 等待及本轮新排入任务的合计数量 | 64 |
| 子帧最大深度 | 16 |
| 累计反馈 | 64 条、16 KiB UTF-8 |

预算由整条链共享，子帧、重定向或分叉不重置它。Brigadier 本身的解析深度与分支预算也仍然生效。
这些是本服务端策略，并非声称与原版 gamerule 默认值一致。

## 结果与取消

`ExecutionReport` 包含 `result`、`abort`、已消耗成本 `steps`。
普通语法/命令失败与预算、取消等运行时中止区分处理。底层 Host 的 I/O 错误在运行时报告为 `Host` 中止。

- `result` 是根帧最后执行动作的结果，或该帧显式返回值。
- 每次实际执行调用其来源结果回调；分叉分别报告各分支结果，不把聚合成功次数当成每个分支的值。
- 根命令分叉的返回数值仍是成功分支数量。
- 显式帧返回会作为该次来源回调的结果。解析失败或执行前中止也有失败回调。
- 帧完成回调独立于来源回调；每个已创建的帧至多完成一次。丢弃/中止的帧报告失败。
- 未开始执行且被丢弃的排队条目不会逐条触发来源执行回调。
- 已发生的成功回调或副作用不会因为后续取消而撤回。

服务器 Host 捕获玩家当前就绪 Play 生命周期的令牌。每个步骤和最终结果都检查来源活跃性；
断线、慢客户端淘汰、重新配置都会使旧令牌失效，即使同 UUID 重连或重新回到 Play 也不能复活旧链。
取消时丢弃待执行任务与累计反馈，不向新会话发送旧结果。外部自定义 Host 通过
`command_active` 实现自己的生命周期/取消检查；直接用其他 Host 调用核心 API 不自动获得服务器会话绑定。

## 无玩家连接的使用示例

```rust
use zcraft::command::{
    Anchor, CommandSource, Dimension, SourceKind, SourceStack,
};

let source = SourceStack::new(CommandSource {
    kind: SourceKind::CommandBlock,
    permission: 2,
    origin: None,
})
.positioned(
    Dimension { id: "minecraft:overworld".into(), coordinate_scale: 1.0 },
    [8.0, 64.0, 8.0],
).expect("valid position")
.with_anchor(Anchor::Feet);
// let report = server.run_command(&source, "list");
// 保存 report.result.success / value / feedback 到未来命令方块组件。
```

注册一个排队包装器（测试扩展示例，不是新增内置命令）：

```rust
use zcraft::command::{Command, CommandResult};

let wrapper = Command::new("queued_list", "schedule list", |_, host| {
    let Some(control) = host.execution() else {
        return Ok(CommandResult::failure("Runtime required."));
    };
    if let Err(error) = control.enqueue("list") {
        return Ok(CommandResult::failure(error.to_string()));
    }
    Ok(CommandResult::success(0))
});
// builder.register(wrapper);
```

## 尚未实现

- 跨 tick 挂起/恢复、命令方块组件、条件链和循环触发。
- 数据包函数、宏、函数返回语义和函数加载生命周期。
- 实体选择器、48 种 Minecraft 参数及世界相关求值。
- OP 管理、管理员反馈广播、`sendCommandFeedback` 等游戏规则及原版消息本地化。
- 原版来源栈的全部方法与所有极端行为。

当前反馈仅返回调用方或丢弃；玩家适配器发送系统消息，控制台/命令方块调用方负责保存或显示结果。
这不是后台任务调度器；每条链在调用内同步、有界地完成。
