# Daemon Mode

Spacebot runs as a background daemon by default. The CLI provides commands to start, stop, restart, and check the status of the running process.

## Commands

```bash
spacebot                      # start as background daemon (default)
spacebot start                # same as above
spacebot start --foreground   # run in the foreground (no daemonization)
spacebot stop                 # graceful shutdown
spacebot restart              # stop + start
spacebot status               # show pid and uptime
```

Global flags work on all commands:

```bash
spacebot start --debug              # verbose logging
spacebot start --config ./my.toml   # custom config path
spacebot start -f -d                # foreground + debug (useful during development)
```

## How It Works

### Starting

When you run `spacebot` or `spacebot start`:

1. Check if a daemon is already running (PID file + socket probe). If so, exit with a message.
2. Run onboarding if this is the first launch (interactive, stays in the foreground).
3. Validate the config file loads successfully.
4. Fork the process via `daemonize`. The parent prints a message and exits. The child continues.
5. Redirect logs to `~/.spacebot/logs/` with daily rotation.
6. Write PID to `~/.spacebot/spacebot.pid`.
7. Start the IPC server on `~/.spacebot/spacebot.sock`.
8. Initialize agents, messaging adapters, and the main event loop.

With `--foreground`, steps 4-5 are skipped. Logs go to stdout, the process stays attached to the terminal, and ctrl-c triggers shutdown.

### Stopping

`spacebot stop` connects to the Unix socket, sends a shutdown command, and waits for the process to exit (up to 10 seconds). The daemon performs graceful shutdown: draining active channels, stopping heartbeat schedulers, closing database connections, and cleaning up runtime files.

### Restarting

`spacebot restart` sends a stop (if running), waits for exit, then starts a new daemon. Accepts `--foreground` to restart in foreground mode.

### Status

`spacebot status` queries the running daemon for its PID and uptime.

```
$ spacebot status
spacebot is running
  pid:    12345
  uptime: 2h 15m 30s
```

## Runtime Files

All daemon runtime files live in the instance directory (`~/.spacebot/` or `$SPACEBOT_DIR`):

```
~/.spacebot/
├── spacebot.pid          # PID of the running daemon
├── spacebot.sock         # Unix domain socket for IPC
└── logs/
    ├── spacebot.log      # current log file (daily rotation)
    ├── spacebot.log.2026-02-12   # previous day's logs
    ├── spacebot.out      # stdout from the daemonized process
    └── spacebot.err      # stderr from the daemonized process
```

These files are cleaned up on graceful shutdown. If the process crashes, stale PID and socket files are detected and removed on the next `start`.

## IPC Protocol

The daemon listens on a Unix domain socket for control commands. The protocol is JSON-lines — one JSON object per line, one command per connection.

```
client connects to ~/.spacebot/spacebot.sock
client sends:    {"command":"status"}\n
server responds: {"result":"status","pid":12345,"uptime_seconds":8130}\n
connection closes
```

Two commands are supported:

| Command | Response |
|---------|----------|
| `shutdown` | `{"result":"ok"}` — daemon begins graceful shutdown |
| `status` | `{"result":"status","pid":12345,"uptime_seconds":8130}` |

The socket is also the mechanism for detecting whether a daemon is already running. If the socket exists but nothing is listening, it's treated as stale.

## Logging

In foreground mode, logs go to stdout with ANSI colors (the default `tracing_subscriber` behavior).

In daemon mode, logs are written to `~/.spacebot/logs/spacebot.log` using `tracing-appender` with daily rotation. Old log files are kept with date suffixes. ANSI codes are disabled for file output.

The `--debug` flag sets the log level to `debug` regardless of mode.

## Stale File Recovery

If Spacebot crashes without cleaning up, the next `start` handles it:

1. Read the PID from `spacebot.pid`
2. Check if that process is still alive (`kill -0`)
3. If dead, remove the stale PID file and socket file
4. Proceed with normal startup

If the PID is alive but the socket doesn't respond, the PID file is also treated as stale (the process may have crashed during startup before the socket was bound).

## Module Layout

All daemon infrastructure lives in `src/daemon.rs`:

- `DaemonPaths` — path derivation for PID file, socket, and log directory
- `is_running()` — PID + socket liveness check
- `daemonize()` — fork via the `daemonize` crate
- `start_ipc_server()` — Unix socket listener, returns a shutdown signal
- `send_command()` — client-side socket communication
- `cleanup()` — remove PID and socket files
- `IpcCommand` / `IpcResponse` — serde types for the wire protocol

The CLI subcommand routing lives in `src/main.rs`. Each subcommand (`start`, `stop`, `restart`, `status`) is a standalone function that handles its own runtime setup.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `daemonize` | Unix double-fork, PID file, stdio redirect |
| `tracing-appender` | File-based log rotation |
| `libc` | Process liveness check (`kill(pid, 0)`) |
