# outpost

A terminal UI for multiplexing network listeners, modeled on
[herdr](https://github.com/herdrdev/herdr), a multiplexer for coding agents.
herdr's layout, look, and interaction patterns are the design reference: the
goal is a TUI that feels almost identical to herdr.

## Vocabulary

- **Workspace**: one listener on `IP:PORT`, TCP or UDP (herdr: workspace).
- **Session**: one connection to a workspace. For UDP, each distinct peer
  address is a session. Each session is shown as a **tab** (herdr: agent / tab).
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
  into one redraw.
- `src/net.rs`: listener and connection tasks (tokio). Talks to the app only
  through `NetEvent`s. A session's write side is an `Outlet`; dropping it
  closes a TCP connection.
- `src/app.rs`: all state and input handling. Everything enters through
  `App::handle(Event)`. Modes (`Mode`) are the overlays plus normal/prefix.
  `Target` names what a menu, rename, or close acts on (workspace or tab).
- `src/ui.rs`: rendering. Records clickable regions in `Hits`, which the next
  mouse event resolves against.
- `src/text_input.rs`: single-line text with a movable cursor, used by every
  editable field.

Note: `ui::render` takes `&mut App`: it clamps transcript scroll and stores
`Hits`. herdr keeps render pure (geometry/mutation in a separate pass). If
render grows more side effects, split it the same way rather than adding more.

## UI conventions

outpost is mouse-first, like herdr. Every new feature should work with both
mouse and keyboard, and reuse the existing patterns instead of inventing new
ones:

- **Forms** (new workspace, settings, rename): rounded accent border with a
  title. Clicking a field focuses it; clicking inside text places the cursor.
- **Right-click menus**: square accent border, opened at the click and kept on
  screen, hover highlights, ↑/↓/Enter/Esc. Any click outside dismisses without
  passing through. Workspaces and tabs have menus; the sessions list does not.
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
  `add_test_workspace()`, `add_test_session()` (no real sockets; the returned
  receiver shows what the app sent to the peer). Drive it with the `key`,
  `type_str`, `prefix`, `click`, `right_click`, and `data` helpers.
- **Rendering** (`src/ui.rs` tests): `render_rows(app, 120, 32)` draws to
  ratatui's `TestBackend` and returns rows as strings. It also fills `app.hits`,
  so render first, then click a recorded rect.
- **Networking** (`src/net.rs` tests): real sockets on `127.0.0.1:0`, with
  timeouts on every wait.
- **End-to-end**, when a change depends on how a real terminal delivers input
  (mouse buttons, hover, escape sequences): drive `target/debug/outpost` with
  a small Python script using `pty.fork()`, set the size with `TIOCSWINSZ`, and
  write raw bytes (SGR mouse is `\x1b[<button;col;rowM`, 1-based). Check the
  result from the network side (what a client receives, whether a port is
  open). tmux `send-keys` cannot deliver mouse events. Keep these scripts in
  the session scratchpad, not the repo, and run them with `python3 -I`.

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
- `#[allow(...)]` only with a comment or an obvious reason.
- Don't add dependencies without a reason; check tokio/ratatui first.
- Keep changes surgical; match the surrounding style.

## Commits

The user makes commits. Don't commit or push unless asked. Do provide a
conventional commit message for each change that the user can use in their
commit.
