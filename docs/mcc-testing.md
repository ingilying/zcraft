# Minecraft Console Client smoke test

Build the server, then explicitly opt in with the path to your MCC executable:

```sh
cargo build --locked
python3 tools/check-mcc.py --mcc ~/mcc/MinecraftClient
```

The Python 3 script has no third-party dependencies. `--server PATH` selects a
prebuilt server; `--hold-seconds N` extends the heartbeat observation period
(default/minimum 35 seconds). MCC must support Java 1.21.1 / protocol 767 and
accept an INI path as its first argument.

## Isolation

The test launches an offline server bound only to `127.0.0.1`, with two player
slots and a temporary EULA acceptance file. MCC uses new configurations, dummy
offline names, `Password = "-"`, temporary working directories and HOME, and
explicit protocol selection. Session/profile caches, Sentry, translations,
SRV lookups and player-head fetching are disabled. Existing client/server
configurations and account credentials are not used or modified. This is
configuration isolation, not an OS network sandbox for the MCC executable.

All spawned processes are stopped in `finally`, including on assertion failure
or Ctrl-C. The printed temporary directory retains configs and diagnostic logs.
Delete it manually when no longer needed. The temporary port is selected with a
bind/rebind, so another process could claim it before server startup; in that
case the test reports failure rather than selecting an unrelated server.

## Assertions

- First MCC joins with terrain processing enabled; status reports `online=1`.
- A second connection with the same offline identity is rejected; the original
  remains connected.
- A distinct player joins; status reports `online=2` and `max=2`.
- A third identity is rejected because the server is full.
- Both admitted clients remain online for at least 35 seconds, exercising
  ongoing protocol liveness beyond the login deadline.
- `/quit` releases the slot and UUID, allowing the same identity to reconnect.
- Process termination also releases slots; final status reports `online=0`.

Verified locally with MCC v26.2, GitHub build 519 (commit `4fbe90c`), selecting
Minecraft 1.21.1 / protocol 767. All assertions passed. The script checks English
MCC success messages and current server rejection messages; localization or
future wording changes may require adapting these assertions.

This test does **not** validate graphical rendering, online Microsoft/Mojang
login, resource-pack download/reload, reconfiguration, multiplayer entity
synchronization, or bounded broadcast queue overflow. Those require separate
manual or automated tests. Successful MCC terrain processing is not proof of
complete vanilla client compatibility.
