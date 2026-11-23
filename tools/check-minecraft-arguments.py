#!/usr/bin/env python3
"""Render black-box argument fixtures from the verified official 1.21.1 JAR.
No download, server startup, EULA modification, or user account access.
Usage: python3 tools/check-minecraft-arguments.py path/to/server.jar
"""
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

root = Path(__file__).resolve().parent.parent
jar = Path(sys.argv[1])
if hashlib.sha1(jar.read_bytes()).hexdigest() != "59353fb40c36d304f2035d51e7d6e6baa98dc05c":
    raise SystemExit("Expected the official Minecraft Java 1.21.1 server bundler")
with tempfile.TemporaryDirectory(prefix="zcraft-argument-reference-") as tmp:
    directory = Path(tmp)
    with zipfile.ZipFile(jar) as bundle:
        for category in ("libraries", "versions"):
            for line in bundle.read(f"META-INF/{category}.list").decode().splitlines():
                digest, _, path = line.split("\t")
                data = bundle.read(f"META-INF/{category}/" + path)
                if hashlib.sha256(data).hexdigest() != digest:
                    raise SystemExit(f"Nested artifact checksum mismatch: {path}")
                target = directory / Path(path).name
                if target.exists():
                    raise SystemExit(f"Artifact name collision: {path}")
                target.write_bytes(data)
    result = subprocess.run(
        ["java", "-Dfile.encoding=UTF-8", "-cp", str(directory / "*"),
         str(root / "tools/MinecraftArgumentOracle.java")],
        cwd=directory, capture_output=True, text=True, timeout=60, check=True,
    )
    # Incidental dependency initialization logs are not fixture rows.
    rows = [line for line in result.stdout.splitlines()
            if len(line.split("\t")) == 4 and line.split("\t")[-1] in ("ok", "error")]
    if len(rows) != 94:
        raise SystemExit("Incomplete oracle output: " + result.stdout + result.stderr)
    print("# Official Java 1.21.1 argument syntax: kind, UTF-8 hex (- means empty), UTF-16 cursor, status")
    print("\n".join(rows))
