<p align="center">
  <img src="assets/zcraft-icon.svg" alt="ZCraft 图标" width="128" height="128">
</p>

<h1 align="center">ZCraft</h1>

<p align="center">
  使用 Rust 构建的 Minecraft Java 服务端，始于清晰、可扩展的协议与网络架构。
</p>

<p align="center">
  <strong>Rust + Tokio</strong> · Java 1.21.1 · 协议 767 · 早期开发中 ·
  <a href="LICENSE">GPL-3.0-only</a>
</p>

<p align="center">
  <a href="#功能状态">功能状态</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#配置">配置</a> ·
  <a href="#开发">开发</a> ·
  <a href="#参与贡献">参与贡献</a>
</p>

---

ZCraft 探索以 Rust 实现 Minecraft 服务端，将网络传输、协议编解码和业务处理分离，
通过强类型数据包与声明式注册表扩展协议功能。

> [!IMPORTANT]
> 项目处于早期开发阶段。已实现状态查询、离线/正版登录、Configuration 同步，以及
> **静态平坦世界的最小观战 Play 流程**，不再在登录后立即断开。
> 目前仅完成独立协议客户端联调，原版 Java 1.21.1 图形客户端的注册表加载与渲染仍需验证。
> 尚无完整玩法、保存或多人实体同步，暂不适合生产部署。

## 功能状态

- [x] 异步 TCP 接入、连接上限、读写超时与退出清理
- [x] VarInt 编解码、完整帧读写、拆包与粘包处理
- [x] 按协议状态和包 ID 解析，强类型处理器分发
- [x] 握手、服务器状态查询与 Ping/Pong
- [x] 服务器图标校验、Base64 编码与响应缓存
- [x] TOML 配置加载、默认文件生成与 EULA 启动检查
- [x] 离线登录与 Mojang 正版会话认证
- [x] RSA 密钥交换与 AES-128-CFB8 加密传输
- [ ] 数据包压缩
- [x] Configuration 状态机、11 个动态注册表、功能标志与完成确认
- [x] Configuration Keep Alive、可选/强制资源包协商与 Play 重新配置 API
- [x] 最小观战 Play 初始化、25 个静态区块、光照与玩家位置确认
- [x] Play Keep Alive、传送确认与取消安全的帧读取
- [ ] 自定义数据包、资源包动态增删及完整玩法语义
- [ ] 方块交互、玩家实体同步与世界持久化

状态响应的在线人数目前固定为 `0`，尚未实现多版本协议映射。

## 快速开始

### 环境要求

- 支持 Rust 2024 edition 的近期稳定版 Rust 与 Cargo。
- Minecraft Java 1.21.1 客户端，用于验证服务器列表及登录流程。
- C 编译器、Perl 和 Make 等原生构建工具，用于编译随依赖提供的 OpenSSL；首次构建较慢。
- 首次编译需下载约 51.6 MB 的官方 1.21.1 服务端 JAR，以生成注册表；可使用本地 JAR 或预热缓存离线构建，不需要安装 Java。
- 正版模式需能直接通过 HTTPS 访问 `sessionserver.mojang.com`。

在项目根目录运行：

```sh
cargo run --release
```

首次运行会自动生成 `zcraft.toml` 和 `eula.txt`，然后因未同意 EULA 而退出。

1. 阅读 [Minecraft EULA](https://www.minecraft.net/eula)。
2. **仅在同意后**，手动将 `eula.txt` 中的 `eula=false` 改为 `eula=true`。
3. 再次运行 `cargo run --release`。
4. 在客户端的服务器列表中添加 `localhost:25565`。

按 `Ctrl-C` 停止服务并清理连接。单独部署二进制时，请同时复制 `server-icon.png`，
或在配置中指定自己的图标。

> [!NOTE]
> 默认监听 `0.0.0.0:25565`，即所有 IPv4 网卡。仅本机测试时建议改为 `127.0.0.1:25565`。
> 连接上限和超时保护并不等同于完整的公网抗攻击方案。

## 配置

默认读取**当前工作目录**的 `zcraft.toml`，不存在时安全生成：

```toml
bind = "0.0.0.0:25565"
motd = "ZCraft — Rust Minecraft Server"
server_icon = "server-icon.png"
max_players = 20
offline_mode = false
max_connections = 1024
io_timeout_seconds = 10
```

| 配置项 | 说明 |
| --- | --- |
| `bind` | 监听 IP 和端口；IPv6 示例：`[::]:25565` |
| `motd` | 服务器列表描述 |
| `server_icon` | PNG 图标路径；空字符串表示禁用 |
| `max_players` | 服务器列表展示的容量，尚不是实际玩家接入限制 |
| `offline_mode` | 是否启用离线登录，默认 `false`，即要求正版会话认证 |
| `max_connections` | 最大并发连接数，必须大于 0 且不超过 Tokio 信号量上限 |
| `io_timeout_seconds` | 完整响应批次写入及部分帧读取的时限，整数秒，范围 1–86400 |

缺少字段使用默认值；未知字段、错误类型和无效值会阻止启动。
已有配置及注释不会被重写。修改后需重启，暂不支持热加载。

### 登录模式

默认启用正版认证：

```toml
offline_mode = false
```

客户端收到 RSA 公钥与随机验证令牌后完成密钥交换，后续整个 TCP 协议流（含长度前缀）
采用 AES-128-CFB8 加密。服务器向 Mojang `hasJoined` 接口验证会话，使用验证返回的
UUID、用户名和属性，不信任客户端自报 UUID。认证失败、超时或接口异常时拒绝登录，
**不会回退到离线模式**。认证请求固定使用官方 HTTPS 地址，不跟随重定向，也不使用代理环境变量。

仅在本地测试或可信网络中，可手动开启离线登录：

```toml
offline_mode = true
```

旧配置不会自动重写，需要自行添加此字段；省略时仍为 `false`。修改后重启生效。
离线模式不访问 Mojang，也不进行登录加密，使用 Java 兼容的
`OfflinePlayer:<用户名>` 算法生成离线 UUID。

> [!WARNING]
> 离线模式下任何人都可以冒用其他用户名。建议同时设置 `bind = "127.0.0.1:25565"`，
> 不要直接暴露到不可信网络。切换登录模式会改变同一用户名对应的 UUID。

当前支持 `Login Start → Encryption Request/Response（正版模式）→ Login Success → Login Acknowledged`。
收到确认后进入 Configuration，完成注册表同步后进入只读观战世界。
未实现压缩，不会发送 Set Compression。错误版本、重复或乱序登录包会被拒绝。
登录/状态的入站处理总期限为 30 秒，Mojang 认证上限为 10 秒，
RSA 解密最多并发 16 个任务。Configuration 每个协商步骤限时 30 秒，资源包步骤限时 120 秒，
从对应请求写入完成后计时；心跳、客户端设置及资源包进度不会延长步骤期限。
Configuration 和 Play 每 10 秒发起一次 Keep Alive，响应时限 30 秒；Play 位置确认时限 15 秒。
这些响应时限均从对应请求写入完成后计时，慢连接发送注册表不会消耗下一次确认的等待窗口。
每个完整响应批次的写入及部分帧接收仍受 `io_timeout_seconds` 限制。
Configuration 解码、状态校验或读取超时错误会尽力发送明确的 Disconnect；断开通知的写入同样有界。

### Configuration 与最小世界

服务器不声明可省略的已知数据包；收到客户端的空 known-pack 响应后，发送完整的
11 个动态注册表及 Update Tags；如配置资源包则等待协商完成，再发送 Finish Configuration，
收到确认才切换为 Play。同时接收客户端语言、视距、皮肤部件设置和可选插件消息。

默认不请求资源包。可在 `zcraft.toml` 末尾添加以下配置（替换 URL、UUID 及 SHA-1）：

```toml
[resource_pack]
id = "00000000-0000-0000-0000-000000000001"
url = "https://example.org/pack.zip"
sha1 = "0123456789012345678901234567890123456789"
required = false
prompt = "是否加载服务器资源包？"
```

URL 必须为 HTTP(S)，SHA-1 必须为 40 位十六进制字符串。支持接受、下载、加载及各类失败状态，
校验 UUID、状态顺序和重复响应；非强制包失败/拒绝后继续，强制包失败/拒绝则断开。
包下载由客户端执行，服务端不托管或预下载 ZIP；目前每次配置仅支持一个包。

嵌入式服务端可调用 `handler::configuration::request_reconfiguration(&mut Context)`，
将返回的 `Outcome` 通过正常写入器发送，并调用 `configuration::packet_sent` 报告写入完成。
此时仍按 Play 解码，收到 `Configuration Acknowledged` 后重新执行配置及观战世界初始化；
重复请求和未经请求的确认会被拒绝，等待确认也有独立的 30 秒期限。
同一连接已加载的相同 UUID/SHA-1 资源包会保留，不重复请求加载。
暂未提供管理命令或配置热加载入口。

- 主世界高度为 `-64..319`，plains 生物群系，固定白天。
- 加载中心 `(0, 0)` 周围半径 2 的 25 个区块；出生点为 `(8.5, 65, 8.5)`。
- 地面为基岩、两层泥土、草方块（顶层 y=63），包含高度图和天空光照。
- 使用观战模式，限制在已加载范围附近；越界会被传送回出生点。
- 登录身份及皮肤属性通过 Player Info Update 发送，但尚不广播其他玩家实体。
- 不保存世界，不处理方块修改、聊天广播、背包、配方或实体 AI。
  一部分明确注册的非交互包仅有界消费并忽略，不代表对应玩法已实现。
- 标签由构建期数据预编码；自定义数据包及涉及标签的完整玩法暂不支持。
  服务列表在线人数仍固定为 0。

注册表在编译期从官方服务端 JAR 的 JSON 生成 Rust 常量；有效缓存存在时不重复下载或提取。
运行时直接使用预编码数据，不依赖旧的第三方 JSON 快照。可用 `ZCRAFT_SERVER_JAR` 指定本地官方 JAR，
`ZCRAFT_DATA_CACHE` 指定缓存根目录，`ZCRAFT_DATA_OFFLINE=1` 禁止构建脚本联网。
来源、许可、离线构建及编码约束见 [协议数据说明](docs/protocol-data.md)。
独立协议客户端验证了配置包、地形和光照解码，以及超过 30 秒的持续连接；
这不能替代原版图形客户端的实际兼容性验证。

### 环境变量

| 变量 | 用途 |
| --- | --- |
| `ZCRAFT_CONFIG` | 指定配置文件路径，父目录需已存在 |
| `ZCRAFT_BIND` | 覆盖监听地址 |
| `ZCRAFT_MOTD` | 覆盖服务器描述 |
| `RUST_LOG` | 日志过滤级别，例如 `info` 或 `debug` |

```sh
ZCRAFT_CONFIG=./zcraft.toml \
ZCRAFT_BIND=127.0.0.1:25565 \
RUST_LOG=debug \
cargo run --release
```

优先级为 **环境变量覆盖 > 文件值 > 默认值**。文件本身先通过校验，再应用覆盖；
环境变量不会写回文件。日志输出至标准错误，启动失败记录为 `ERROR` 并以非零状态退出。

### EULA

每次启动都会在绑定网络端口前检查当前工作目录的 `eula.txt`。
只有明确的 `eula=true` 才允许启动；未同意、重复项、格式错误或读取失败均阻止启动。
程序不会自动同意，也不提供环境变量绕过。

`ZCRAFT_CONFIG` 不改变 EULA 文件的位置。生成的配置和 EULA 文件已加入 Git 忽略规则。

### 服务器图标

默认图标使用深绿底和薄荷绿像素字母 **Z**，提供以下资源：

| 文件 | 用途 |
| --- | --- |
| [`server-icon.png`](server-icon.png) | 64×64 PNG，可直接用于服务器列表 |
| [`assets/zcraft-icon.svg`](assets/zcraft-icon.svg) | README 展示及可编辑矢量源文件 |
| [`tools/generate_icon.py`](tools/generate_icon.py) | 使用 Python 标准库重绘 PNG |

```sh
python3 tools/generate_icon.py
```

自定义图标必须是 **64×64 的静态 PNG，大小不超过 20 KiB**。路径相对于工作目录，
而非配置文件目录。文件缺失时省略图标；错误尺寸、损坏图片或读取错误会阻止服务启动。
图标在启动时加载并缓存，通过状态响应的 `favicon` 字段返回，替换后需重启。
服务器列表需要 PNG，不能直接使用 SVG。

## 开发

### 架构

```text
TCP → 完整帧 → 状态与包 ID 解析 → Dispatcher → 类型化处理器
                                                   ↓
TCP ← 帧编码 ← Clientbound ←─────────────────────── 回复
```

```text
src/
├── main.rs               # 启动流程、日志与退出信号
├── config.rs             # 配置加载、默认生成及校验
├── eula.rs               # EULA 启动检查
├── network.rs            # TCP 与连接生命周期
├── transport.rs          # 可切换的明文 / AES-CFB8 传输
├── identity.rs           # 跨模块共享的玩家身份与属性
├── auth/
│   ├── mod.rs            # AuthService 认证入口与服务编排
│   ├── offline.rs        # 离线 UUID 与用户名校验
│   ├── mojang.rs         # 会话验证接口、HTTP 查询与响应校验
│   └── crypto.rs         # RSA、挑战值与 Minecraft 登录哈希
├── context.rs            # 会话及共享服务上下文
├── registry_data.rs      # 编译期生成的 Configuration 注册表数据入口
├── world.rs              # 静态地形、区块、高度图及光照
├── protocol/
│   ├── codec.rs          # 基础类型编解码
│   ├── frame.rs          # 帧长度与完整帧读写
│   ├── registry.rs       # 声明式收发包注册表
│   └── packets/          # 按协议阶段组织的数据包
└── handler/              # 无状态分发器与各阶段状态机，包括 configuration/play
```

构建脚本 `build.rs` 和 `build_support/` 负责官方 JAR 获取、校验、选择性提取及 JSON → NBT → Rust 代码生成，生成物保存在构建缓存与 Cargo `OUT_DIR`。

`Dispatcher` 只负责路由和协议状态校验，不构建具体业务响应。
状态响应及图标缓存由 status 模块负责，处理器通过上下文访问服务，不直接操作 socket。
`identity` 只定义共享身份模型；认证由 `auth` 内部模块协作完成，登录状态机留在
`handler/login.rs`。RSA 登录密钥交换与 `transport.rs` 的 AES 流加密分离。

### 扩展数据包

1. 在 `src/protocol/packets/` 定义包结构，入站实现 `Decode`，出站实现 `Encode`。
2. 在 `src/handler/` 编写异步类型化处理器：`async fn handle_x(ctx: &mut Context<'_>, packet: X) -> io::Result<Outcome>`。
3. 在 `src/protocol/registry.rs` 对应阶段注册类型与处理器。

以下为已有 Ping/Pong 的注册方式：

```rust
// serverbound_packets! 的 Status 块
0x01 => Ping(Ping) => crate::handler::status::handle_ping,

// clientbound_packets! 的 Status 块
0x01 => Pong(Pong),
```

处理器返回的 `Outcome.replies` 支持有序多包回复；关闭连接和启用加密仍通过 Outcome 通知网络层执行。
注册宏生成枚举、解析、分发、编码及 `From` 转换，无需重复维护多个 `match`。
同一状态和方向内的重复 ID 会在编译期报错；包 ID 不进入业务处理器。
字段编解码不包含帧长度和包 ID，新建模块需在父模块声明。

### 检查与测试

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

测试覆盖字段与帧编解码、协议分发、TCP 生命周期、配置生成、EULA 启动门禁、图标响应，
以及离线 UUID、RSA/SPKI、Minecraft 有符号 SHA-1、在线身份校验、双向加密登录、
Configuration 顺序、资源包成功/拒绝/失败与状态校验、重新配置、断开提示、
区块/光照、心跳和慢连接下的配置/传送确认期限。
在线认证集成测试使用注入的会话验证器，不依赖真实账号；尚未完成真实 Microsoft/Mojang 账号联调。

可选的独立客户端测试需要 Node.js 和两个 npm 包；依赖安装在临时目录，不修改项目：

```sh
npm install --prefix /tmp/zcraft-client-check minecraft-protocol prismarine-chunk
NODE_PATH=/tmp/zcraft-client-check/node_modules \\
  cargo test --test client_compat -- --ignored --nocapture
```

该测试通过测试服务入口启动本地离线服务，不使用真实账号或用户配置。
它使用独立实现解析所有注册表和 25 个区块的地面及光照，并维持连接 34 秒，
验证 Keep Alive 和 Play 阶段不再受原有 30 秒登录期限影响。默认 `cargo test` 跳过此项。

## 参与贡献

欢迎通过 Issue 反馈问题或讨论设计，通过 Pull Request 提交改进。

- 问题报告请附上复现步骤、客户端版本和相关日志，避免包含账号或凭证。
- 较大功能或架构改动建议先讨论范围。
- 为行为变更补充测试，并在提交前运行上述检查。
- 保持传输、协议和业务处理的职责边界，避免将具体包逻辑放入分发器。

## 许可与声明

ZCraft 采用 **GNU General Public License v3.0（仅第 3 版）**，
SPDX 标识为 `GPL-3.0-only`。完整条款见 [LICENSE](LICENSE)。
第三方依赖及资源仍适用各自的许可证；本项目许可证不替代 Minecraft EULA。

ZCraft 是独立的社区项目，与 Mojang 或 Microsoft 无隶属关系，亦未获其认可。
Minecraft 相关名称及商标归各自权利人所有。

协议参考：[Minecraft Wiki · Server List Ping](https://minecraft.wiki/w/Java_Edition_protocol/Server_List_Ping)。
