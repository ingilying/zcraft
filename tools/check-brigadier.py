#!/usr/bin/env python3
"""Render the differential fixture using the verified bundled Brigadier JAR."""
import hashlib
import pathlib
import subprocess
import sys
import tempfile
import zipfile

root = pathlib.Path(__file__).resolve().parent.parent
artifact = 'com.mojang:brigadier:1.3.10'
with zipfile.ZipFile(sys.argv[1]) as server:
    digest, _, path = next(line.split('\t') for line in server.read('META-INF/libraries.list').decode().splitlines()
                           if line.split('\t')[1] == artifact)
    data = server.read('META-INF/libraries/' + path)
if hashlib.sha256(data).hexdigest() != digest:
    raise SystemExit('Brigadier SHA-256 mismatch')
with tempfile.TemporaryDirectory() as tmp:
    jar = pathlib.Path(tmp) / 'brigadier.jar'
    jar.write_bytes(data)
    subprocess.run(['java', '-Dfile.encoding=UTF-8', '-cp', str(jar), str(root / 'tools' / 'BrigadierOracle.java')], check=True)
