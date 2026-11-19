#!/usr/bin/env python3
"""Opt-in offline MCC compatibility test. Only connects to a temporary loopback server."""
import argparse
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def recv_exact(sock, size):
    result = bytearray()
    while len(result) < size:
        chunk = sock.recv(size - len(result))
        if not chunk:
            raise EOFError("status connection closed")
        result.extend(chunk)
    return bytes(result)


def recv_varint(sock):
    value = 0
    for shift in range(0, 35, 7):
        byte = recv_exact(sock, 1)[0]
        value |= (byte & 127) << shift
        if not byte & 128:
            return value
    raise ValueError("oversized VarInt")


def status(port):
    with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
        host = b"127.0.0.1"
        handshake = b"\0" + varint(767) + varint(len(host)) + host
        handshake += struct.pack(">H", port) + b"\x01"
        sock.sendall(varint(len(handshake)) + handshake + b"\x01\x00")
        length = recv_varint(sock)
        if not 0 < length <= 1024 * 1024:
            raise ValueError("invalid status frame length")
        if recv_varint(sock) != 0:
            raise ValueError("unexpected status packet")
        size = recv_varint(sock)
        if size > length:
            raise ValueError("invalid status JSON length")
        return json.loads(recv_exact(sock, size))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mcc", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/zcraft")
    parser.add_argument("--hold-seconds", type=float, default=35)
    parser.add_argument("--compression-threshold", type=int, default=256)
    args = parser.parse_args()
    if args.hold_seconds < 35:
        parser.error("--hold-seconds must be at least 35 (exercise heartbeats)")
    if not -1 <= args.compression_threshold <= 2097151:
        parser.error("--compression-threshold must be -1..=2097151")
    mcc, server_bin = args.mcc.expanduser().resolve(), args.server.expanduser().resolve()
    for executable in (mcc, server_bin):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error(f"not executable: {executable}")
    root = Path(tempfile.mkdtemp(prefix="zcraft-mcc-"))
    print(f"Logs and isolated configuration: {root}", flush=True)
    processes, files = [], []

    def launch(command, cwd, label):
        output = (root / f"{label}.log").open("w")
        files.append(output)
        env = os.environ.copy()
        for key in list(env):
            if key.startswith("ZCRAFT_"):
                del env[key]
        env.update(HOME=str(cwd), DOTNET_CLI_TELEMETRY_OPTOUT="1", RUST_LOG="debug")
        process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                   stdout=output, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    def log(label):
        return (root / f"{label}.log").read_text(errors="replace")

    def wait_for(predicate, message, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.1)
        raise AssertionError(message)

    def stop(process):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        if process.stdin:
            process.stdin.close()

    try:
        # Brief bind/rebind race is possible; startup failure is reported, not hidden.
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        (root / "eula.txt").write_text("eula=true\n")
        (root / "zcraft.toml").write_text(
            f'bind="127.0.0.1:{port}"\noffline_mode=true\nmax_players=2\nserver_icon=""\n'
            f'compression_threshold={args.compression_threshold}\n')
        server = launch([str(server_bin)], root, "server")

        def online():
            if server.poll() is not None:
                raise AssertionError("server exited: " + log("server"))
            return status(port)["players"]["online"]

        def ready():
            try:
                return online() == 0
            except OSError:
                return False

        wait_for(ready, "server did not start")
        assert status(port)["players"]["max"] == 2

        def client(name, label):
            cwd = root / label
            cwd.mkdir()
            config = cwd / "MinecraftClient.ini"
            config.write_text(f'''[Main.General]
Account = {{ Login = "{name}", Password = "-" }}
Server = {{ Host = "127.0.0.1", Port = {port} }}
AccountType = "mojang"
[Main.Advanced]
EnableSentry = false
MinecraftVersion = "1.21.1"
LoadMccTranslation = false
LoadResourcePackTranslations = false
LoadForgeModTranslations = false
AutoDiscoverForgeModTranslationSources = false
ShowGithubStarReminder = false
SessionCache = "none"
ProfileKeyCache = "none"
ResolveSrvRecords = "no"
PlayerHeadAsIcon = false
ExitOnFailure = true
TerrainAndMovements = true
InventoryHandling = false
EntityHandling = false
[Console.General]
ConsoleMode = "classic"
''')
            return launch([str(mcc), str(config)], cwd, label)

        def joined(label, count):
            wait_for(lambda: "Server was successfully joined" in log(label) and online() == count,
                     f"{label} failed to join with online={count}")
            print(f"PASS {label} joined, online={count}", flush=True)

        alice = client("MccAlice", "alice")
        joined("alice", 1)
        duplicate = client("MccAlice", "duplicate")
        wait_for(lambda: "this player is already connected" in log("duplicate"),
                 "duplicate UUID was not rejected")
        assert alice.poll() is None and online() == 1
        stop(duplicate)
        print("PASS duplicate rejected; original remains online", flush=True)

        bob = client("MccBob", "bob")
        joined("bob", 2)
        extra = client("MccExtra", "full")
        wait_for(lambda: "the server is full" in log("full"), "capacity was not enforced")
        assert online() == 2
        stop(extra)
        print("PASS full server rejected third player, online=2", flush=True)

        deadline = time.monotonic() + args.hold_seconds
        while time.monotonic() < deadline:
            assert alice.poll() is None and bob.poll() is None, "MCC exited during heartbeat test"
            assert online() == 2, "player lost during heartbeat test"
            time.sleep(0.5)
        print(f"PASS both clients remained online for {args.hold_seconds:g}s", flush=True)

        alice.stdin.write(b"/quit\n")
        alice.stdin.flush()
        wait_for(lambda: online() == 1, "normal client quit did not release slot")
        stop(alice)
        reconnect = client("MccAlice", "reconnect")
        joined("reconnect", 2)
        stop(bob)
        wait_for(lambda: online() == 1, "terminated client did not release slot")
        stop(reconnect)
        wait_for(lambda: online() == 0, "last disconnect did not release slot")
        print("PASS normal quit, reconnect and process termination cleanup; online=0", flush=True)
        print(log("alice").splitlines()[0], flush=True)
    except BaseException:
        for path in sorted(root.glob("*.log")):
            print(f"--- {path.name} ---\n{path.read_text(errors='replace')[-12000:]}", flush=True)
        raise
    finally:
        for process in reversed(processes):
            stop(process)
        for output in files:
            output.close()


if __name__ == "__main__":
    main()
