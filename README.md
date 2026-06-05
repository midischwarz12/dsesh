<!--
SPDX-FileCopyrightText: 2026 midischwarz12
SPDX-License-Identifier: AGPL-3.0-or-later
-->

# dsesh

`dsesh` is a small detachable terminal session runner. It is intentionally closer
to `dtach` or `abduco` than to `tmux`: one Unix socket owns one foreground
program, and attaching clients only connect to that program. The difference is
that `dsesh` keeps a retained vt100 screen model in the server. When you attach
again, the client receives a fresh rendering of the last known terminal screen
before live output resumes, so full-screen programs generally do not need
`Ctrl-L`, `SIGWINCH`, or a manual redraw after reattach.

## Status

This is an early implementation. It supports one PTY-backed command per socket,
reattachment with retained screen redraw, terminal resize propagation, and a
simple detach key. It does not implement panes, windows, tabs, layouts, status
bars, plugins, or persistent session registries.

## Usage

Start a new session:

```sh
dsesh new /tmp/editor.sock -- nvim
```

Detach with `Ctrl-\`. The child process keeps running.

Attach later:

```sh
dsesh attach /tmp/editor.sock
```

Use `run` for dtach-like "attach if it exists, otherwise create it":

```sh
dsesh run /tmp/shell.sock -- "$SHELL"
```

When the socket already exists, `run` does not need a command:

```sh
dsesh run /tmp/shell.sock
```

The Nix flake also provides `dr`, a convenience wrapper that creates
`/tmp/.dsesh`, chooses a UUID socket name, and runs the command through
`dsesh run`:

```sh
nix run .#dr -- "$SHELL"
nix run .#dr -- sh -c 'command1; command2 | command3'
```

`dsesh` prints `[detached - SOCKET]` after a client detaches and
`[EOF - ended session]` when the child process exits or is terminated.

## Model

`dsesh` has two moving pieces:

- A server process owns the PTY, child process, Unix socket, and retained
  terminal screen.
- A client process puts your terminal in raw mode, forwards input to the server,
  prints output from the server, and detaches on `Ctrl-\`.

The server parses PTY output into a `vt100` screen. Every new client receives a
clear-screen sequence plus the formatted retained screen contents. After that,
the client receives live PTY bytes.

The socket path is the session identity. Removing the socket while the server is
running makes the session unreachable, like other socket-file based tools.
Examples use `.sock` because the path is a Unix domain socket, not a saved
session data file.

## CLI

```text
Usage: dsesh [OPTIONS] <COMMAND>

Commands:
  new     Start a new session and attach to it
  attach  Attach to an existing session
  run     Attach to an existing socket, or start a new session first

Options:
      --cols <COLS>  Fallback terminal width [default: 80]
      --rows <ROWS>  Fallback terminal height [default: 24]
  -h, --help         Print help
  -V, --version      Print version
```

## Nix

Enter the development shell:

```sh
direnv allow
# or
nix develop
```

Build the package:

```sh
nix build .#dsesh
```

Run checks:

```sh
nix flake check
```

## Development

The project is a standard Rust crate.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace --all-targets
./tests/e2e-retained-screen.sh
```

The E2E test starts a detached session through the real binary and checks that a
second attach receives previously emitted screen content through the retained
screen snapshot. It also checks detach, EOF, `run SOCKET`, and Ctrl-C handling.

## License

`dsesh` is licensed under the GNU Affero General Public License v3.0 or later.
See `LICENSE` for the full license text.

## Design limits

`dsesh` deliberately avoids terminal multiplexing and rich session management.
There is no session list, authentication layer, scrollback browser, pane graph,
or command protocol for manipulating layouts. If you need those, `tmux` and
`zellij` are the right tools. If you want one process behind one socket with
screen retention across detach and attach, this is the intended scope.
