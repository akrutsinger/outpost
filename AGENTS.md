# outpost

A terminal UI for multiplexing network listeners, modeled on
[herdr](https://github.com/herdrdev/herdr), a multiplexer for coding agents.
herdr's layout, look, and interaction patterns are the design reference: the
goal is a TUI that feels almost identical to herdr.

## Vocabulary

- **Endpoint**: a sidebar row that owns tabs: a listener, or the **local** endpoint (this machine),
  listed first once it has a shell.
- **Listener**: an endpoint with one socket bound to `IP:PORT`: TCP, UDP, TLS, or QUIC.
- **Terminal tab**: the user's login shell in a PTY, drawn by an emulator (vt100). Every key but the
  prefix goes to the shell. Shells are not sessions: the sidebar's sessions list shows connections.
- **Session**: one connection to a listener. For UDP, each distinct peer address is a session. Each
  session is shown as a **tab** (herdr: agent / tab).
- **Stream**: QUIC only. One session carries many; its transcript tags lines `[sN]` by stream ID
  once there are two, and input goes to one stream at a time.
- **Overlay**: any modal box (settings form, rename, menu, confirmation, help).

## herdr as the reference

When adding UI, look at how herdr does it first and match it. Useful places in
its source:

- `src/client/shell/sidebar.rs`, `agent_sidebar.rs`: sidebar rows and sections
- `src/client/shell/tabs.rs`: tab bar
- `src/client/shell/overlays.rs`: menus, rename, confirm-close, buttons
- `src/client/shell/render.rs`: sidebar separator, mode bar
- `src/app/state.rs` (`Palette::catppuccin`): colors; ours are in `src/theme.rs`
- `src/config/model.rs` (`KeysConfig::default`): default keybindings

Clone it into the session scratchpad when needed; never into this repo.
Adapt, don't copy wholesale: outpost is a much smaller program.

## Architecture

- `src/main.rs`: terminal setup, one event channel, the draw/handle loop.
  Input comes from a blocking crossterm reader thread; bursts are coalesced
  into one redraw. Before each draw, `App::fit_terminals` sizes every shell
  to `ui::terminal_area`, the same geometry render uses.
- `src/net.rs`: listener and connection tasks (tokio). Talks to the app only
  through `NetEvent`s. A session's write side is an `Outlet`; dropping it
  closes a TCP connection.
- `src/net/tls.rs`: TLS settings (`TlsConfig`), building the rustls server
  config from a self-signed or PEM-file certificate, and the handshake
  summary line. The app builds it when a form is applied, so certificate
  errors show in the form.
- `src/net/quic.rs`: QUIC listener and connections (quinn), reusing the TLS
  config. The app predicts the IDs of streams it opens (`server_bidi`), so
  `QuicOut::Write` to a new ID opens that stream.
- `src/pty.rs`: a program in a PTY (portable-pty). A reader thread per PTY
  sends `PtyEvent`s (output, then the exit status). Dropping a `Pty` hangs
  up on the program.
- `src/app.rs`: `App` and input handling. Everything enters through
  `App::handle(Event)`. Modes (`Mode`) are the overlays plus normal/prefix.
  `Target` names what a menu, rename, or close acts on (endpoint or tab).
  - `app/endpoint.rs`: `Endpoint` (a sidebar row and its `Tab`s), and its
    `Kind`, which holds what only that kind has: a `Listener` (one bound
    socket, its listening task, and the sessions it accepted;
    `Endpoint::listener()` reaches it) or `Local`. A `Tab` is a transcript
    `Session` or a `Terminal`.
  - `app/session.rs`: `Session` (transcript, input line, byte counts) and a
    QUIC session's `Streams`.
  - `app/terminal.rs`: `Terminal` (a shell's PTY and screen) and
    `encode_key`, which turns keys into the bytes an xterm sends.
  - `app/settings.rs`: `SettingsForm`: building it, editing rules per field,
    and `check`, which validates it into an `Applied` config.
- `src/ui.rs`: rendering entry point and shared drawing helpers. Records
  clickable regions in `Hits`, which the next mouse event resolves against.
  Split like herdr's shell:
  - `ui/sidebar.rs`: endpoints over sessions, and the dividers.
  - `ui/tabs.rs`: the tab bar (with herdr's `+`, which opens a shell) and the
    focused tab: a transcript and input, or a terminal's screen.
  - `ui/overlays.rs`: settings form, menu, rename, confirmation, help
    (`render_help`).
- `src/text_input.rs`: single-line text with a movable cursor, used by every
  editable field.

Note: `ui::render` takes `&mut App`: it clamps transcript scroll and stores
`Hits`. herdr keeps render pure (geometry/mutation in a separate pass). If
render grows more side effects, split it the same way rather than adding more.

## UI conventions

outpost is mouse-first, like herdr. Every new feature should work with both
mouse and keyboard, and reuse the existing patterns instead of inventing new
ones:

- **Forms** (new listener, settings, rename): rounded accent border with a
  title. Clicking a field focuses it; clicking inside text places the cursor.
- **Right-click menus**: square accent border, opened at the click and kept on
  screen, hover highlights, ↑/↓/Enter/Esc. Any click outside dismisses without
  passing through. Listeners and tabs have menus; the sessions list does not.
- **Confirmations**: herdr's red square box, title + one detail line, centered
  `↵ confirm` / `esc cancel` buttons. Other keys and stray clicks are ignored.
  Ask before any action that drops live connections; skip asking otherwise.
- The row or tab an overlay acts on is highlighted while it is open.
- Keys use herdr's `ctrl+b` prefix and herdr's default letters where an
  equivalent exists. Keep the `?` overlay (`render_help`) in sync.
- Text must fit at 120 columns, and stay readable for full IPv4 peers
  (`255.255.255.255:65535`) and long custom names. Add a render test when
  space is tight.

## Testing

Run before handing work back:

```bash
cargo fmt
cargo test
cargo clippy --all-targets   # must be warning-free
```

Follow herdr's approach: tests live next to the code, and behavior is tested
in-process without spawning the binary.

- **Behavior** (`src/app/tests.rs`): build state with `App::test_new()`,
  `add_test_listener()`, `add_test_session()` (no real sockets; the returned
  receiver shows what the app sent to the peer). Drive it with the `key`,
  `type_str`, `prefix`, `click`, `right_click`, and `data` helpers.
- **Rendering** (`src/ui/tests.rs`): `render_rows(app, 120, 32)` draws to
  ratatui's `TestBackend` and returns rows as strings. It also fills `app.hits`,
  so render first, then click a recorded rect.
- **Networking** (`src/net.rs` tests): real sockets on `127.0.0.1:0`, with
  timeouts on every wait.
- **End-to-end** (`tests/e2e/`), when a change depends on how a real terminal
  delivers input (mouse buttons, drags, escape sequences) or on real
  listeners: add a case to `tests/e2e/main.rs`. `Outpost::start()` runs the
  binary in a 120×32 pty and reads its screen through `vt100`; drive it with
  `keys`, `prefix`, `click`, `right_click`, `drag`, and `new_listener`. Check
  the network side too (what a client receives, whether a port is bound);
  `clients.rs` has TLS and QUIC clients. Before each step, wait for what it
  acts on (`wait_for`, `wait_until`, `eventually`), never sleep: keys sent
  before a tab or overlay exists go nowhere. tmux `send-keys` cannot deliver
  mouse events.

When a behavior changes on purpose, update the tests that encoded the old
behavior and say so; don't loosen assertions to make them pass.

## Safety

- Only stop processes, tmux sessions, and listeners you started. Never use
  `tmux kill-server`, `pkill`, or broad process matching: the user runs their
  own tmux sessions on this machine.
- Use the session scratchpad for temporary files, not `/tmp` or the repo.

## Code conventions

- No `unwrap()`/`expect()` in production code except where the invariant is
  local and obvious (with a message saying why). Tests may unwrap.
- Silence a lint with `#[expect(...)]` (not `#[allow]`, so it flags itself once
  the lint stops firing), and only with a comment or an obvious reason.
- Don't add dependencies without a reason; check tokio/ratatui first.
- Keep changes surgical; match the surrounding style.

## Commits

The user makes commits. Don't commit or push unless asked. Do provide a
conventional commit message for each change that the user can use in their
commit.
