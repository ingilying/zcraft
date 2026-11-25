#!/usr/bin/env python3
"""Generate compact gameplay metadata from the official Java 1.21.1 server.

This runs only Mojang's data generator and a read-only reflection oracle; it does
not start a game server, modify eula.txt, or access a user account.
Usage: python3 tools/generate-game-data.py path/to/server.jar [output.json]
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

SERVER_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "assets/protocol/767/game_data.json"


def check_server(path: Path) -> None:
    if hashlib.sha1(path.read_bytes()).hexdigest() != SERVER_SHA1:
        raise SystemExit("Expected the official Minecraft Java 1.21.1 server bundler")


def nested_artifacts(server: Path, root: Path) -> tuple[Path, list[Path]]:
    jars = []
    version = None
    with zipfile.ZipFile(server) as bundle:
        for category in ("libraries", "versions"):
            for line in bundle.read(f"META-INF/{category}.list").decode().splitlines():
                digest, _, relative = line.split("\t")
                data = bundle.read(f"META-INF/{category}/" + relative)
                if hashlib.sha256(data).hexdigest() != digest:
                    raise SystemExit(f"Nested artifact checksum mismatch: {relative}")
                target = root / category / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                jars.append(target)
                if category == "versions":
                    if version is not None:
                        raise SystemExit("Expected one bundled server version")
                    version = target
    if version is None:
        raise SystemExit("Bundled server version is missing")
    return version, jars


def block_tags(server_jar: Path) -> dict[str, list[str]]:
    raw = {}
    prefix = "data/minecraft/tags/block/"
    with zipfile.ZipFile(server_jar) as jar:
        for name in jar.namelist():
            if name.startswith(prefix) and name.endswith(".json"):
                tag = "minecraft:" + name[len(prefix):-5]
                values = json.loads(jar.read(name))["values"]
                raw[tag] = [value if isinstance(value, str) else value["id"] for value in values]
    resolved = {}
    visiting = set()
    def visit(tag: str) -> list[str]:
        if tag in resolved:
            return resolved[tag]
        if tag in visiting:
            raise SystemExit(f"Cyclic block tag: {tag}")
        visiting.add(tag)
        values = []
        for value in raw.get(tag, []):
            if value.startswith("#"):
                values.extend(visit(value[1:]))
            else:
                values.append(value)
        visiting.remove(tag)
        resolved[tag] = sorted(set(values))
        return resolved[tag]
    for tag in raw:
        visit(tag)
    return dict(sorted(resolved.items()))


def main() -> None:
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    server = Path(sys.argv[1]).resolve()
    output = Path(sys.argv[2]).resolve() if len(sys.argv) == 3 else OUTPUT
    check_server(server)
    with tempfile.TemporaryDirectory(prefix="zcraft-game-data-") as temporary:
        root = Path(temporary)
        version, jars = nested_artifacts(server, root)
        subprocess.run([
            "java", "-Dfile.encoding=UTF-8", "-DbundlerMainClass=net.minecraft.data.Main",
            "-jar", str(server), "--reports",
        ], cwd=root, check=True, stdout=subprocess.DEVNULL, timeout=120)
        reports = root / "generated/reports"
        blocks_report = json.loads((reports / "blocks.json").read_text())
        items_report = json.loads((reports / "items.json").read_text())
        registries = json.loads((reports / "registries.json").read_text())
        classpath = os.pathsep.join(str(path) for path in jars)
        oracle = subprocess.run([
            "java", "-Dfile.encoding=UTF-8", "-cp", classpath,
            str(ROOT / "tools/GameDataOracle.java"),
        ], cwd=root, check=True, capture_output=True, text=True, timeout=120)
        runtime = {}
        for line in oracle.stdout.splitlines():
            fields = line.split("\t")
            if len(fields) != 13:
                continue
            state, block, hardness, correct, air, liquid, occludes, replaceable, collision, skylight, emission, light_block, item = fields
            runtime[int(state)] = {
                "block": block, "hardness": float(hardness), "correct_tool": correct == "true",
                "air": air == "true", "liquid": liquid == "true", "occludes": occludes == "true",
                "replaceable": replaceable == "true", "collision": collision == "true",
                "skylight": skylight == "true", "light_emission": int(emission),
                "light_block": int(light_block), "item": item,
            }
        state_count = sum(len(block["states"]) for block in blocks_report.values())
        if len(runtime) != state_count:
            raise SystemExit(f"Oracle returned {len(runtime)} of {state_count} block states")

        item_ids = registries["minecraft:item"]["entries"]
        blocks = []
        states = []
        block_index = {}
        for name, report in blocks_report.items():
            index = len(blocks)
            block_index[name] = index
            first = len(states)
            default = None
            item_name = None
            for state in report["states"]:
                state_id = state["id"]
                facts = runtime.pop(state_id)
                if facts.pop("block") != name:
                    raise SystemExit(f"State {state_id} belongs to the wrong block")
                candidate = facts.pop("item")
                item_name = candidate if item_name is None else item_name
                if candidate != item_name:
                    raise SystemExit(f"Block item differs by state: {name}")
                if state.get("default"):
                    default = state_id
                states.append({"id": state_id, "properties": state.get("properties", {}), **facts})
            if default is None:
                raise SystemExit(f"Block has no default state: {name}")
            blocks.append({
                "name": name, "default_state": default, "first_state": first,
                "state_count": len(report["states"]), "properties": report.get("properties", {}),
                "item": None if item_name == "minecraft:air" else item_name,
            })
        if runtime:
            raise SystemExit("Oracle returned unknown block states")

        tags = {}
        for name, members in block_tags(version).items():
            tags[name] = [block_index[member] for member in members if member in block_index]
        items = []
        for name, report in items_report.items():
            components = report["components"]
            tool = components.get("minecraft:tool")
            rules = []
            if tool:
                for rule in tool["rules"]:
                    values = rule["blocks"] if isinstance(rule["blocks"], list) else [rule["blocks"]]
                    rules.append({
                        "blocks": values,
                        **({"speed": rule["speed"]} if "speed" in rule else {}),
                        **({"correct_for_drops": rule["correct_for_drops"]} if "correct_for_drops" in rule else {}),
                    })
            items.append({
                "name": name, "id": item_ids[name]["protocol_id"],
                "max_stack": components["minecraft:max_stack_size"],
                "max_damage": components.get("minecraft:max_damage", 0),
                "tool_rules": rules,
                "block": block_index.get(name),
            })
        items.sort(key=lambda item: item["id"])
        data = {
            "format": 1, "minecraft": "1.21.1", "protocol": 767,
            "server_sha1": SERVER_SHA1, "blocks": blocks, "states": states,
            "block_tags": tags, "items": items,
        }
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(data, separators=(",", ":"), sort_keys=True) + "\n")
        print(f"wrote {output}: {len(blocks)} blocks, {len(states)} states, {len(items)} items, {len(tags)} block tags")


if __name__ == "__main__":
    main()
