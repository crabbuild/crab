# Terminal recovery verification

Uses the production TerminalView, TerminalKeepAlive, preload, IPC registration,
router, local bridges and daemon. The SSH fixture uses the production terminal
manager and handlers with an authenticated loopback SSH server and real tmux.
Other desktop services are fixture plumbing; no Rust sidecar, user repository,
real agent account, or real agent chat is needed.

Build Electron and start a Vite server on port 5178:

```sh
npm run build:electron
npx vite --host 127.0.0.1 --port 5178
```

Create a disposable directory, then launch the fixture with that directory as
`--fixture-root`. The profile, daemon socket, layouts and proof files stay there.
The daemon survives closing the window; terminate that fixture daemon after QA.

```sh
FIXTURE_ROOT=$(mktemp -d /tmp/crab-terminal-XXXXXX)
env -u ELECTRON_RUN_AS_NODE node_modules/.bin/electron \
  tests/fixtures/terminal-recovery/main.cjs --fixture-root "$FIXTURE_ROOT"
```

The fixture header switches repositories, reopens workspaces with new IDs, and
changes the renderer cap. Split controls are the real terminal controls. The
`agent.cjs` fixture is an interactive alternate-screen program: it records its
PID, input, cwd and resize events to a supplied disposable JSONL proof file.
Type `stop` to end that process.

For SSH QA, install tmux in the test environment and start the loopback server
in a separate process before launching the GUI. It verifies its generated host
fingerprint, listens only on localhost, and writes connection metadata to the
fixture root. Its tmux socket is isolated by TMUX_TMPDIR. Stop the SSH fixture
with SIGTERM to close its clients and clean its remote tmux server.

```sh
ELECTRON_RUN_AS_NODE=1 node_modules/.bin/electron \
  node_modules/vite-node/vite-node.mjs \
  tests/fixtures/terminal-recovery/ssh-server.ts "$FIXTURE_ROOT"
```

Creating an `offline` file in the root makes the GUI fixture's SSH connector
fail without ending the remote process. Reopen the app, verify the saved tab and
retry message, remove that file, then press Enter to reattach the same PID.
`--daemon-entry` accepts a replacement entry path for the app-upgrade case;
`--renderer-url` lets CI use a Vite server on an ephemeral port.

The automated component smoke is `tests/e2e/terminal-recovery.spec.ts`. It uses
real components and PTYs instead of a duplicate LRU model. Native transport
regressions live in terminal-session-lifecycle, terminal-snapshot-replay,
daemon-native-terminal and ssh-terminal-live specs. SSH native tests require
tmux; Windows clients use the same SSH transport, while the local loopback tmux
server test runs on Unix hosts.
