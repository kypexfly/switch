# AGENTS.md

Switch manages multiple accounts for subscription AI coding tools (Claude Code today) and swaps the active login. Usage-only providers (Antigravity) are shown without switching. Read `docs/architecture.md` before changing behavior.

## Workspace layout

- `crates/protocol`: API types (`utoipa::ToSchema`), events, and server registration. Changing it changes the contract for every client.
- `crates/client`: Blocking HTTP client (`Client`) and server discovery/auto-start (`Service`).
- `crates/core`: `Engine`, `Provider` trait, Claude, Antigravity, and demo providers, browser sign-in state, poll scheduler, account `Store`, and credential `Vault`. All account logic lives here; no HTTP server code.
- `crates/server`: Axum HTTP + SSE server over `Engine`, background loops, and `service.json` registration.
- `crates/cli`: The `swx` binary (`tui`, `serve`, `stop`, `status`, `add`, `login`, `switch`, `use`, `refresh`).
- `crates/tui`: Interactive terminal dashboard on OpenTUI.
- `crates/desktop`: macOS-only menu bar app on Rune and AppKit (`objc2`).

Clients hold no account logic and never hardcode provider IDs, provider names, or usage window labels; they use the snapshot's `switchable` and `sign_in` flags. The only provider-specific client assets are the tray SVG marks in `crates/desktop`.

## Setup, checks, and packaging

```sh
scripts/fetch-opentui.sh                              # native OpenTUI library in vendor/opentui-native (needed by tui and cli)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt -p <crate>                                  # format only the crate you changed, not the whole workspace
scripts/bundle-macos.sh                               # builds and ad-hoc signs dist/Switch.app (bundles switch-desktop + swx)
scripts/install-macos.sh                              # bundles, installs /Applications/Switch.app and links ~/.local/bin/swx; rerun to update
scripts/install-linux.sh                              # builds and installs ~/.local/bin/swx, restarting the systemd user service if active; rerun to update
```

Dependencies and workspace config to keep in mind:

- Rust edition 2024; formatting rules live in `rustfmt.toml`.
- OpenTUI is pinned (`=0.9.9` in `Cargo.toml` and `scripts/fetch-opentui.sh`). `.cargo/config.toml` sets `OPENTUI_LIB_DIR` to `vendor/opentui-native`.
- Rune is pinned to a git revision in `Cargo.toml`; keep all `[patch.crates-io]` entries at the same revision.
- `serde_json` enables `preserve_order` and `float_roundtrip` workspace-wide so editing `~/.claude.json` keeps key order and numeric formatting intact.

## Trying changes safely

Use a scratch `SWITCH_HOME` and `--demo` so nothing touches real logins:

```sh
export SWITCH_HOME=$(mktemp -d)
cargo run -p switch-cli -- serve --demo               # terminal 1
cargo run -p switch-cli -- tui                        # terminal 2
SWITCH_CLI=target/debug/swx cargo run -p switch-desktop
```

Use the same `SWITCH_HOME` in every terminal. Without a running demo server, any client auto-starts the real server, which reads the real Claude login.

To inspect desktop UI changes without running the app, render light/dark panel PNGs and tray previews from fixtures:

```sh
SWITCH_DESKTOP_SNAPSHOTS=$(mktemp -d) cargo test -p switch-desktop render_snapshots -- --ignored
```

## Environment variables

| Variable | Purpose |
| --- | --- |
| `SWITCH_HOME` | Data directory for `service.json`, `accounts.json`, `server.log`, `desktop.lock`, `desktop.json`, and `logins/` (default `~/.local/share/switch`). |
| `SWITCH_CLI` | Override path to the `swx` binary spawned by `switch-desktop` (defaults to sibling `swx`, then `PATH`). |
| `SWITCH_DESKTOP_SNAPSHOTS` | Output directory for the ignored `render_snapshots` test in `switch-desktop`. |
| `NO_COLOR` | Disables ANSI color output in `swx`. |
| `OPENTUI_LIB_DIR` | Native OpenTUI library directory (configured in `.cargo/config.toml`). |

## Safety rules

- Never read or write `~/.claude`, `~/.claude.json`, `~/.claude/.config.json`, the Antigravity app's data, the macOS Keychain, or real credentials in tests or experiments, and never run a real Google sign-in or call Google from tests. Use `Claude::at(<temp dir>)`, `Demo`, `Store::memory()`, and `Vault::memory()` or `Vault::files(<temp dir>)`.
- Never renew a provider's live login; its refresh token belongs to the provider's own tool. Only renew saved logins (including logins from Switch's own sign-in), and always persist the rotated secret to the vault before using it.
- Before replacing a live login, save the outgoing login only into the account it provably belongs to. When ownership cannot be confirmed or conflicts with local config, refuse the switch and leave the live login in place.
- Take the provider's locks in its required order before replacing the live login, write files atomically, never overwrite an unparseable config file, and undo partial writes on failure.
- Never include tokens, secrets, or raw credential JSON in errors, logs, or `Debug` output.

## Style and commits

- Smallest change that solves the task; no speculative abstractions.
- No comments unless the code cannot say it; prefer clear names.
- UI copy: no em dashes (use a hyphen), no all-caps titles, no letter-spacing, no decorative animation.
- Use Conventional Commits (`feat: ...`, `fix: ...`, `docs: ...`, optional crate scope). Add new commits; do not amend.
