#!/usr/bin/env python3
"""Refresh protocol-767 tag registry IDs from Mojang's SHA-1-pinned server report.

Requires Java 21+. This runs only Minecraft's data report generator (not a server).
Normal Cargo builds use the checked-in snapshot and do not require Java.
Usage: python3 build_support/generate_static_registry_ids.py /path/to/server.jar
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

SERVER_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
REGISTRIES = (
    "block", "cat_variant", "entity_type", "fluid", "game_event",
    "instrument", "item", "point_of_interest_type",
)


def snapshot(report):
    result = {}
    for name in REGISTRIES:
        entries = report[f"minecraft:{name}"]["entries"]
        ids = {key: value["protocol_id"] for key, value in sorted(entries.items())}
        if sorted(ids.values()) != list(range(len(ids))):
            raise ValueError(f"noncontiguous or duplicate IDs for {name}")
        result[f"minecraft:{name}"] = ids
    return {
        "minecraft_version": "1.21.1",
        "source_jar_sha1": SERVER_SHA1,
        "registries": result,
    }


def main():
    jar = Path(sys.argv[1]).resolve()
    if hashlib.sha1(jar.read_bytes()).hexdigest() != SERVER_SHA1:
        raise ValueError("expected official Minecraft 1.21.1 server.jar")
    with tempfile.TemporaryDirectory(prefix="zcraft-registry-report-") as directory:
        subprocess.run([
            "java", "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(jar),
            "--reports", "--output", "generated",
        ], cwd=directory, check=True)
        report = json.loads((Path(directory) / "generated/reports/registries.json").read_text())
    output = Path(__file__).resolve().parents[1] / "assets/protocol/767/static_registry_ids.json"
    output.write_text(json.dumps(snapshot(report), indent=2, sort_keys=True) + "\n")
    print(f"Wrote {output}")


if __name__ == "__main__":
    main()
