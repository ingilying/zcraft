# Minecraft 命令参数

本模块以 Java 1.21.1（协议 767）为基线。参数解析与世界求值分离：解析器只生成结构化值；需要来源位置、执行实体或世界注册表的数据时，再显式调用 `resolve`。缺失能力返回错误，不使用猜测值。

## 已实现的纯语法参数

`MinecraftArgument` 支持以下协议类型及对应的协议 767 parser ID：

| 类型 | ID | 结果 |
|---|---:|---|
| `block_pos` | 8 | `BlockPos` |
| `column_pos` | 9 | `ColumnPos` |
| `vec3` | 10 | `Vec3` |
| `vec2` | 11 | `Vec2` |
| `angle` | 27 | `Angle` |
| `rotation` | 28 | `Rotation` |
| `swizzle` | 31 | `Axes` |
| `resource_location` | 35 | `ResourceLocation` |
| `entity_anchor` | 37 | `Anchor` |
| `int_range` | 38 | `IntRange` |
| `float_range` | 39 | `FloatRange` |
| `uuid` | 53 | `Uuid` |

世界坐标支持绝对值和 `~` 相对值；三维浮点位置及方块位置支持 `^` 局部坐标。单个三维参数不能混用 `^` 与世界坐标。绝对整数方块坐标按原版接受整数；浮点位置的未显式小数绝对 X/Z 可选择方块中心修正。局部坐标使用来源 yaw、pitch 和脚/眼锚点。需要相对位置、局部坐标、旋转或锚点而来源未提供对应能力时，求值失败。

`tests/fixtures/minecraft-arguments-1.21.1.tsv` 是从 SHA-1 验证过的官方 1.21.1 server bundler 中直接调用参数解析器生成的黑盒语法/UTF-16 游标夹具。可选重建命令：

```sh
ZCRAFT_TEST_SERVER_JAR=target/zcraft-data/1.21.1/server.jar \
  cargo test --locked --test minecraft_arguments \
  regenerate_against_official_minecraft_argument_parsers -- --ignored
```

## 实体参数

`EntityArgument { single, players_only }` 使用协议 ID 6 和原版 flags。支持：

- 玩家名称和 UUID；
- `@p`、`@a`、`@r`、`@s`、`@e`；
- `x/y/z`、`dx/dy/dz`、`distance`、`x_rotation/y_rotation`；
- `limit`、`sort`；
- 可重复且带反选规则的 `name`、`type`、`gamemode`。

选择器需要权限等级 2；直接名称和 UUID 不需要选择器权限。`@s` 使用 `SourceStack` 的执行实体 UUID，而不是认证玩家 UUID。距离、盒选、最近/最远排序限定来源维度。随机排序必须注入 RNG，便于服务端使用真实随机源和测试使用固定种子。

当前世界只存在已进入并仍可接收 Play 数据的玩家。`ServerContext` 在持锁期间复制其 UUID、名称、位置、精确旋转和游戏模式，然后释放锁再求值；不会跨等待点或 executor 调用持有玩家表锁。当前实体种类仅可信支持 `minecraft:player`。

尚不支持 `scores`、`advancements`、`nbt`、`predicate`、`team`、`tag` 等依赖未实现系统的过滤条件，解析时明确报错。

## 方块和物品参数

`BlockArgument`（协议 ID 12）解析资源 ID 与方块属性；`ItemArgument`（协议 ID 14）解析物品资源 ID。求值只接受服务器现有的可信子集：

- 方块状态：air、stone、bedrock、dirt、grass_block（`snowy=false`）、glass；
- 创造模式快捷栏物品：stone、dirt、glass。

返回值分别是实际协议 block-state ID 和 item registry ID。未知方块、物品、属性或状态不会映射为占位值。方块实体 NBT、物品组件、组件移除、旧式物品 NBT 和标签引用尚未实现，解析时明确报错。

这些参数仅提供解析、补全及显式求值，不新增任何内置命令。
