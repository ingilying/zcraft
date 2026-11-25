# Official Minecraft 1.21.1 data provenance

Dynamic registry data is generated locally at build time from the official
Minecraft Java 1.21.1 server bundler JAR. This directory no longer contains a
third-party registry snapshot.

- Official metadata:
  https://piston-meta.mojang.com/v1/packages/ff7ef54488da4a711f3c8a781a9f79105bbe801a/1.21.1.json
- Official download:
  https://piston-data.mojang.com/v1/objects/59353fb40c36d304f2035d51e7d6e6baa98dc05c/server.jar
- Mojang SHA-1: `59353fb40c36d304f2035d51e7d6e6baa98dc05c`
- Bundler size: `51627615` bytes.
- Inner JAR: `META-INF/versions/1.21.1/server-1.21.1.jar`.
- Selected input files: the eleven synchronized registry directories under
  `data/minecraft/` and 514 tag files under `data/minecraft/tags/` in the inner JAR.

The build does not start Minecraft, require Java, change EULA acceptance, or
copy executables into the distributed source. It verifies the pinned JAR before
selectively reading registry JSON; generated Rust/NBT resides in the build cache
and Cargo's `OUT_DIR`.

The Minecraft-derived data remains subject to Mojang/Microsoft's applicable
terms and original rights. ZCraft's GPL-3.0-only license covers its own code,
including its extraction and generation tools; it does not relicense Minecraft
data. Automatic extraction is not a grant of redistribution rights. Review the
[Minecraft EULA](https://www.minecraft.net/eula) and applicable usage guidelines
before redistributing binaries or derived data.

Transformations: filter biome records to protocol 767's network fields, encode
JSON as anonymous network NBT, sort registry entries deterministically, and
place `minecraft:plains` / `minecraft:overworld` at ID zero. Update Tags contains
all 514 vanilla tags in the 13 network-safe registries with tag data. Nested tag
references are resolved to the exact IDs advertised by Registry Data for dynamic
registries, and to official built-in registry IDs for static registries (not
block-state IDs). The 16 tags in server-only `worldgen/structure`,
`worldgen/world_preset`, and `worldgen/flat_level_generator_preset` registries are
excluded, as vanilla does not synchronize those registries to clients.

`static_registry_ids.json` is a narrow snapshot of eight static registry ID maps
from the **official** `generated/reports/registries.json`, not a third-party
snapshot. It is generated with the same SHA-1-verified JAR using only its data
report entrypoint:

```sh
java -DbundlerMainClass=net.minecraft.data.Main -jar /path/to/server.jar \\
  --reports --output generated
```

To reproduce the checked-in snapshot (Java 21+ and Python 3 required only for
this maintenance command):

```sh
python3 build_support/generate_static_registry_ids.py /path/to/server.jar
```

The helper verifies the official SHA-1 before execution and runs the report in
a temporary directory. Normal builds read this snapshot plus verified JAR JSON;
they still never execute Java, start a game server, or accept an EULA.

`game_data.json` is a compact gameplay metadata snapshot (1,060 blocks, 26,684
block states, 1,333 items, and resolved block tags) generated from the same
SHA-1-verified official 1.21.1 server bundler via Mojang's data generator and
the read-only `tools/GameDataOracle.java` reflection oracle:

```sh
python3 tools/generate-game-data.py /path/to/server.jar
```

Normal builds embed `game_data.json` directly and never invoke Java.

See [protocol-data.md](../../../docs/protocol-data.md) for the build/cache
contract and current limitations.
