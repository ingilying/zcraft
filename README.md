# ZCraft

Rust + Tokio 的 Minecraft Java 1.21.1（协议 767）服务端基础架构。

## 运行

```sh
cargo run
# 自定义监听地址、MOTD 和日志
ZCRAFT_BIND=127.0.0.1:25565 ZCRAFT_MOTD="Hello ZCraft" RUST_LOG=debug cargo run
```

默认监听 `0.0.0.0:25565`（所有网卡）。客户端添加 `localhost:25565`，
可查询服务器描述、版本、人数和延迟。Ctrl-C 停止接收并取消所有连接任务。

**目前不能进入游戏。** 登录/转移请求返回 Login Disconnect；尚未实现认证、
加密、压缩、Configuration、Play、世界或玩家逻辑。在线人数固定为 0。
状态查询允许其他协议版本，以便客户端展示版本不兼容信息。

## 分层

- `src/network.rs`：TCP 接入、连接生命周期、并发限制、完整读写超时。
- `src/protocol/codec.rs`：基础字段编解码、`Decode` / `Encode` trait。
- `src/protocol/frame.rs`：TCP 帧长度前缀与完整帧读写。
- `src/protocol/packets/`：按阶段组织的独立包结构，只包含字段和编解码。
- `src/protocol/registry.rs`：协议 767 的收发包注册表；宏生成枚举、解析、分发、编码和 `From` 转换。
- `src/handler/`：会话、上下文和按阶段组织的类型化处理器，不直接操作 socket。
- `src/main.rs`：配置、日志及退出信号。
- `tests/network.rs`：编解码、异常输入、TCP 拆包/粘包、状态/Ping、登录拒绝和超时测试。

接收流程：
`TCP → 长度前缀/完整帧 → 状态与 ID 解析 → Dispatcher → Clientbound → TCP`。

## 新增数据包

1. 在 `src/protocol/packets/` 对应阶段定义结构体，入站包实现 `Decode`，
   出站包实现 `Encode`。只编解码字段，不包含包 ID 或帧长度。
2. 入站包在 `src/handler/` 定义函数：
   `fn handle_x(ctx: &mut Context<'_>, packet: X) -> io::Result<Outcome>`。
3. 在 `src/protocol/registry.rs` 引入类型，并在对应状态下注册一行：

```rust,ignore
// serverbound_packets! 的 Status 块内
0x01 => Ping(Ping) => crate::handler::status::handle_ping,

// clientbound_packets! 的 Status 块内
0x01 => Pong(Pong),
```

以上是现有 Ping/Pong 的注册示例，不需要再手写枚举或修改分发 match。
处理器通过 `Some(Pong { payload }.into())` 构造回复。
新建阶段文件时还需要在父模块声明 `mod`。

宏生成静态匹配，不使用 `Any`、运行时向下转型或每包 trait object 分配。
同一状态、同一方向内的重复 ID 在编译期报错；不同状态/方向可复用 ID。
统一解析入口校验尾随字节后才分发；分发入口验证入站状态，返回后校验出站状态。
业务顺序约束（如重复 Status Request）仍由处理器负责。

包 ID 仅定义在当前版本注册表，不写入业务处理器。尚未实现多版本映射。
原有根模块编解码函数继续重导出，但 `Serverbound` / `Clientbound` 的变体
改为持有独立结构体，例如 `Ping { payload: 42 }.into()`。

默认最多 1024 个连接，每次完整帧读取/回复写入限时 10 秒；
帧长前缀最多 3 字节，帧体最多 2,097,151 字节，分配前校验长度。
`Config` 可在代码中调整连接限制和超时。每个连接串行处理并直接写回，
不使用无界消息队列。此限制不是完整的公网抗攻击方案。

## 验证

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

协议参考：[Minecraft Wiki Server List Ping](https://minecraft.wiki/w/Java_Edition_protocol/Server_List_Ping)。
