# Architecture

Switch manages multiple accounts for subscription AI coding tools and swaps the active login used by each tool. The real providers today are Claude Code, which can switch accounts, and Antigravity, which only reports usage. The provider abstraction is designed to accommodate additional tools such as Codex and OpenCode Go.

## System model

One background server owns all account state, credential storage, usage polling, and live-login mutations. Every user interface is a thin client communicating over local HTTP and a server-sent event (SSE) stream.

```
swx tui ────────────────┐
Switch.app (menu bar)  ┼── HTTP + SSE ──> swx serve ──> Engine ──> Provider (Claude, Antigravity, Demo, ...)
swx status / use / ... ┘   127.0.0.1, bearer token                Store (accounts.json)
                                                                  Vault (Keychain / logins/)
```

### Server discovery and lifecycle

- `swx serve` binds to `127.0.0.1` on a random port (or `--port <n>`), generates a random bearer password, and atomically writes `service.json` (`url`, `version`, `pid`, `password`, `managed`, mode `0600`) under `SWITCH_HOME` (default `~/.local/share/switch`). `managed` is true unless the server was spawned by a client (`--service`), for example when run by hand or by systemd.
- Starting a second server for the same `SWITCH_HOME` is serialized by an exclusive `server.lock` held until the server's runtime and blocking work finish. If a server is already registered, startup fails immediately; a stale `service.json` from a crashed server whose port was reused does not block startup. On shutdown (`POST /api/stop`, `SIGINT`, or `SIGTERM`) or an engine panic, the server removes `service.json` (only if the password still matches its own), exiting with an error on panic so clients or systemd restart it.
- `Service::connect()` in `switch-client` reads `service.json` and verifies the server via `GET /api/info`. If a running, unmanaged server is older than the client (comparing numeric version segments), the client stops it first; a managed server is used whatever its version, so a client never takes over a service's fixed port. If no server is running, the client spawns `swx serve --service` in its own process group with stdout/stderr appended to `server.log`.

### Snapshots and events

The `Snapshot` struct in `switch-protocol` is the single source of truth shared across all interfaces:

- `providers`: ordered list of `ProviderState` (`id`, `name`, `accounts`, optional `unsaved_login`, `switchable`, `sign_in`, and optional `last_auto_switch_at`). `switchable` is false for usage-only providers, whose accounts are never made the live login; clients hide switching and the save-login action for them. `sign_in` is true when accounts are added by signing in through Switch (`swx login <provider>`) rather than from the live login.
- `settings`: `auto_switch` (default `false`), `threshold` (default `90.0`, valid range `1.0..=100.0`), `cooldown_seconds` (default `300`, valid range `0..=86400`), and `strategy` (default `best`).
- `warnings`: human-readable operational notices (such as unreadable live logins or auto-switch failures).

Each `Account` carries its `identity` (`account_id`, `email`, optional `organization_id` and `organization`), optional `alias` and `plan`, `active`, `enabled`, `status` (`pending`, `ready`, `stale { message }`, or `needs_login`), optional `usage` (`fetched_at` and `windows`), and `resets` (`LimitReset` grants: `id`, `label`, `remaining`, optional `expires_at`, `usable`, optional `detail`).

Each `UsageWindow` carries `id`, `label`, optional `used_percent`, optional `resets_at`, `primary` (account-wide windows shown in compact views and used for switching decisions), and optional `period_seconds` (length of a rolling window, used by clients to draw the pace tick on usage bars).

`GET /api/event` immediately emits `server.connected` with the current snapshot, then emits `snapshot.updated` whenever the snapshot changes (including progressively after each account finishes during `poll` and `POST /api/refresh`). Clients render the latest snapshot rather than patching state locally, and never hardcode provider IDs, provider names, or usage window labels.

## Crates

| Crate | Role |
| --- | --- |
| `switch-protocol` | Shared API, snapshot, event, and registration types (`utoipa::ToSchema`). |
| `switch-client` | Blocking HTTP/SSE client (`Client`, 2s connect / 60s call / 180s refresh timeout) and server discovery (`Service`). |
| `switch-core` | `Engine`, `Provider` trait, Claude, Antigravity, and demo providers, browser sign-in state, poll scheduler, `Store`, and `Vault`. No HTTP server code. |
| `switch-server` | Axum router, bearer auth and cross-site middleware, SSE broadcast, background loops, and `service.json` registration. |
| `switch-cli` | The `swx` binary: subcommands, terminal status renderer, and entry point for `serve` and `tui`. |
| `switch-tui` | OpenTUI terminal dashboard with keyboard/mouse navigation and live event streaming. |
| `switch-desktop` | macOS menu bar app on Rune and AppKit (`objc2`): template tray icon, popover panel, and tray menu. |

## HTTP API

Every route requires `Authorization: Bearer <password>` (checked in constant time), unless the server runs with `--no-auth`, in which case `Host` (and `X-Forwarded-Host`, if set) must match `127.0.0.1`, `localhost`, `::1`, `*.ts.net`, or `allowed_hosts` in `server.json`. Requests a browser marks as coming from another site are always refused with `403`: `Sec-Fetch-Site` other than `same-origin` or `none`, or, without that header, an `Origin` whose host matches neither `Host` nor `X-Forwarded-Host`. This stops a web page from acting on the server through a visitor's browser. Error responses are JSON `{"error": "..."}` with status `401` (unauthorized), `403` (cross-site), `404` (unknown provider or account), `409` (refused with a user-facing message), or `500` (internal failure).

| Route | Request / behavior | Success response |
| --- | --- | --- |
| `GET /api/info` | Server version and PID | `ServerInfo` |
| `POST /api/stop` | Schedules graceful server shutdown | `204 No Content` |
| `GET /api/snapshot` | Current snapshot | `Snapshot` |
| `POST /api/refresh` | Fetches usage for accounts eligible on request, then runs auto-switch | `Snapshot` |
| `POST /api/provider/{provider}/account` | Verifies and saves the provider's current live login | `Snapshot` |
| `POST /api/provider/{provider}/switch` | `{"strategy": "next" \| "best"}` | `Snapshot` |
| `POST /api/provider/{provider}/sign-in` | `{"redirect_uri": "..."}`; starts a browser sign-in for a provider with `sign_in` | `SignIn` (`url`, `state`) |
| `POST /api/provider/{provider}/sign-in/finish` | `{"state": "...", "code": "..."}`; exchanges the code and saves the account | `Snapshot` |
| `POST /api/account/{id}/activate` | Switches live login to the specified account | `Snapshot` |
| `POST /api/account/{id}/reset` | `{"grant_id": "..."}`; redeems a banked limit reset and refreshes the account's usage | `Snapshot` |
| `PATCH /api/account/{id}` | Updates `alias` (empty string clears it) and/or `enabled` | `Snapshot` |
| `DELETE /api/account/{id}` | Removes the account and deletes its vault entries | `Snapshot` |
| `PATCH /api/order` | Reorders `providers` and/or `accounts` by id; listed items trade places among themselves, unknown ids are ignored | `Snapshot` |
| `PATCH /api/settings` | Updates `auto_switch` and/or `threshold` | `Settings` |
| `GET /api/event` | SSE stream of `server.connected` and `snapshot.updated` events | `text/event-stream` |

## Engine

`Engine` (`crates/core/src/engine.rs`) separates locking into two layers:

- `work`: per-provider locks that serialize all operations reading/writing logins, calling provider APIs, or switching accounts for a given provider while allowing different providers to sync and refresh in parallel. Refresh takes the lock per account and lets a waiting action go first, so a switch waits for one account rather than the whole refresh.
- `state`: short-lived lock guarding in-memory accounts, live identities, warnings, and poll schedules so `snapshot()` stays fast and non-blocking during I/O.

### Usage-only providers and browser sign-in

A provider reports `Provider::switchable() == false` when Switch can read its usage but not change its live login. Its `live()` is always empty and `lock`/`replace_live` fail. The engine refuses `activate`, `switch`, and `add_account` for it with a `409`, and auto-switch skips it. Its accounts are polled through the saved-login path below.

A provider that returns `Some` from `Provider::sign_in()` adds accounts through an OAuth authorization code sign-in with PKCE, split so the server can run elsewhere (for example a VPS) while the browser runs next to the client:

1. `Engine::begin_sign_in(provider, redirect_uri)` creates a random `state` and code verifier, keeps them in memory for 10 minutes (at most 16 pending sign-ins), and returns the provider's authorize URL with the S256 challenge.
2. The client opens the URL and receives the browser's redirect with `code` and `state`.
3. `Engine::finish_sign_in(provider, state, code)` takes the pending sign-in (each can be finished once), exchanges the code with the stored verifier and redirect URI, reads `Provider::profile`, saves the account (updating the account with the same identity if there is one), stores the secret in the vault, and fetches its usage once.

### Live login synchronization

`Engine::sync_live()` runs at startup and every 10 seconds in the background. It reads each provider's local live login without touching the network so Switch notices sign-ins, sign-outs, and switches performed outside Switch. A live login that does not match any saved account appears on the provider as `unsaved_login`.

### Usage polling and rate-limit policy

`crates/core/src/poll.rs` maintains an independent schedule per account to stay within strict provider rate limits (Claude's usage endpoint admits roughly 30 requests per account per trailing hour). The server invokes `Engine::poll()` every 15 seconds to check which accounts are due.

- **Freshness floor**: Usage younger than 3 minutes is never re-fetched, even on manual `POST /api/refresh`. On server restart, cached `fetched_at` timestamps seed initial schedules so restarts do not trigger bursts.
- **Active account**: Polls every 3 minutes while usage is changing (at least 1 percentage point delta), backs off by 1.5x up to 5 minutes while flat, and tightens to 1 minute when a primary window is rising within 15 percentage points of `settings.threshold` (unless recently rate-limited).
- **Idle accounts**: Start at 5 minutes and back off up to 10 minutes while unchanged.
- **Exhausted accounts**: When peak primary usage reaches 100%, interval is at least 10 minutes, pulled forward to 60 seconds past the latest exhausted window reset.
- **Window resets**: Next poll is never scheduled later than 60 seconds after the earliest future reset among primary windows and used non-primary windows.
- **Jitter**: Success intervals include +/-10% random jitter so separate processes drift apart.
- **Errors and rate limits**: Failures back off exponentially from 30 seconds up to 10 minutes. Rate limits (`429`) honor `Retry-After` (clamping `0s` edge responses to at least 5 minutes and adding a 15-minute margin to long blocks up to 75 minutes) and keep subsequent polls slowed between 6 and 30 minutes for one hour. Manual refresh also skips accounts currently backing off from failures.
- **Plan labels**: Account plan labels are re-checked from `Provider::profile` about once every 24 hours after a successful usage fetch, skipping accounts that were recently rate-limited. A profile without a plan keeps the known label.
- **Banked limit resets**: For providers with `Provider::supports_resets()` (Claude), reset grants (`GET /api/oauth/usage?cedar_ember=1&skip_spend=1`) are checked after a successful usage fetch at most once per hour on background polls and once per 15 minutes on manual refresh, skipping recently rate-limited accounts. Redeeming a grant (`POST /api/organizations/{org}/reset_rate_limits`) persists the in-flight `request_id` in `accounts.json` across restarts until the claim settles to avoid double-spending, never renews a live login, and immediately refreshes the account's usage on success.

### Token renewal

- **Live account**: Usage is queried with the live login and Switch never renews it, because the refresh token belongs to the provider's own tool. A `401` marks the account `stale` until the tool renews its own session. When `sync_live` observes a changed live token (or live-login recovery), non-rate-limit failure backoff is cleared so the account refreshes promptly without restarting the server.
- **Saved (non-live) accounts**: If a saved secret is expired (for Claude, within 5 minutes of `expiresAt`) or returns `401`, Switch renews it via `Provider::renew` and writes the rotated secret to the vault before using it. An `invalid_grant` rejection marks the account `needs_login`.
- **Signed-in accounts**: Logins from Switch's own browser sign-in (Antigravity) belong to Switch, not to the provider's tool, so renewing them is the normal saved-login path above.

### Switching strategies and auto-switch

- Candidates exclude the active account, disabled accounts (`enabled: false`), and accounts with status `needs_login`.
- `next` selects the next eligible non-exhausted account in list order, wrapping around, ignoring windows past their reset.
- `best` selects the eligible account with the lowest peak primary `used_percent` (accounts with no measured usage sort last).
- Auto-switch runs after every `poll` and `refresh` when `settings.auto_switch` is enabled. If any non-expired primary window of the active account is at or above `settings.threshold` and a candidate picked by `settings.strategy` has peak current primary usage at least 5 percentage points below `settings.threshold`, the engine activates that candidate, subject to `settings.cooldown_seconds` between automatic switches. Switching decisions ignore windows past their reset and require recent `Ready` usage (at most 15 minutes old) for both the active account and the candidate. Manually selecting an account (or switching outside Switch as observed by `sync_live`) while a primary window is at or above `threshold` pins that choice across restarts until one of those windows resets. Auto-switch runs each provider independently, falls back to the next candidate on target failures, and backs off failing targets or providers (60 seconds up to 30 minutes).

## Switching safely

Account switching is designed to never lose a rotated refresh token, cross-contaminate accounts, or corrupt a tool's configuration:

1. **Renew target first**: Verify the target account is not `needs_login` and exists in the vault. If its token is expired, renew it and store the rotated secret in the vault before acquiring provider locks.
2. **Acquire provider locks**: Take the provider's locks in its required order so the tool cannot concurrently refresh tokens or rewrite config.
3. **Preserve the outgoing live login**: Before overwriting the live login, save it because the tool may have rotated its refresh token since Switch last saw it:
   - If the live identity matches a saved account and its token `lineage` (hash of the refresh token, or access token if absent) matches the vault copy, update that account directly without network I/O.
   - Otherwise query `Provider::profile`. If the returned identity matches the local config's identity, save the login (updating the matching account or creating a new one if it was unsaved). If the profile belongs to a different identity, refuse the switch (`409`).
   - If `Provider::profile` fails (for example offline or expired access token), save into the matching local owner only if the live token's lineage does not match any other saved account or its `.previous` backup; otherwise refuse the switch and leave the live login untouched.
4. **Replace live login with rollback**: Write the target credentials and config atomically. If the second write fails, restore the previous credentials before returning an error.

### Claude Code live store (`crates/core/src/claude/`)

- **Paths**: `Claude::for_user()` always manages the default profile (`~/.claude` and `~/.claude.json`, or legacy `~/.claude/.config.json` if present and non-empty), ignoring any inherited `CLAUDE_CONFIG_DIR`. Tests use `Claude::at(<temp dir>)`, which never touches the Keychain.
- **Locks**: `DirLock` acquires Claude Code's three directory locks in order: `~/.claude/.oauth_refresh.lock` (60s stale timeout), `~/.claude.lock` (60s stale timeout), and `~/.claude.json.lock` (10s stale timeout). Held locks refresh their mtime every 3 seconds; dead locks are reclaimed through a `.takeover` guard directory so two waiters cannot delete each other's newly acquired lock.
- **Credentials and machine fields**: On macOS, live credentials are stored in Keychain item `Claude Code-credentials` (account `$USER`) with fallback to `~/.claude/.credentials.json` (if `.credentials.json` also exists on macOS, both are updated on replace). Elsewhere, `~/.claude/.credentials.json` is used. Machine-local keys (`mcpOAuth`, `mcpOAuthClientConfig`, `mcpXaaIdp`, `mcpXaaIdpConfig`, `pluginSecrets`) are stripped when saving to the vault and copied forward from the current live store when replacing.
- **Config safety**: Only `oauthAccount` in `~/.claude.json` is replaced; key order and float formatting are preserved via `serde_json` (`preserve_order`, `float_roundtrip`). A config file that fails to parse is never overwritten.

### Antigravity (`crates/core/src/antigravity/`)

Usage only; Switch never reads or changes the Antigravity app's own login or state.

- **Sign-in**: Google OAuth with the installed-app client that ships with Antigravity (its client secret is not confidential). Authorize at `https://accounts.google.com/o/oauth2/v2/auth` with scopes `cloud-platform` and `userinfo.email`, PKCE S256, `access_type=offline`, and `prompt=consent`; the redirect is `http://127.0.0.1:51121/oauth-callback`. The code and refresh token are exchanged at `https://oauth2.googleapis.com/token`; a renewal without a new refresh token keeps the old one, and `invalid_grant` marks the account `needs_login`. Identity comes from `https://www.googleapis.com/oauth2/v1/userinfo?alt=json` (`id`, `email`).
- **Sensitivity**: The `cloud-platform` scope lets the saved token act on the Google account's Cloud resources, so it is stored only in the vault like every other secret.
- **Plan and project**: `POST /v1internal:loadCodeAssist` on `cloudcode-pa.googleapis.com`, then `daily-cloudcode-pa.googleapis.com`, with Antigravity's client metadata and user agent. `paidTier.id` maps to Google AI Pro, Ultra, or Ultra Lite; `cloudaicompanionProject` is the quota project.
- **Usage**: `POST /v1internal:retrieveUserQuotaSummary` on the daily host first, then production unless the daily answer measures every shared bucket; the first measurement of a bucket wins. Buckets `gemini-5h` and `gemini-weekly` become the primary windows Gemini 5h and Gemini 7d; `3p-5h` and `3p-weekly` become Claude 5h and Claude 7d, and the single weekly bucket of the Gemini 4 Argon group becomes Argon 7d. Used percent is `(1 - remainingFraction) * 100`; unknown buckets are skipped.
- **Errors**: `401` is an expired access token (renewed and retried for saved accounts), `403` with reason `SUBSCRIPTION_REQUIRED` reports that the account has no Antigravity subscription, other `403` answers count as `401`, and `429` honors `Retry-After`.

## Storage and vault

All Switch state lives under `SWITCH_HOME` (default `~/.local/share/switch`):

- `service.json`: running server registration (`0600`).
- `server.json`: optional server configuration (`port`, `no_auth`, `allowed_hosts`).
- `server.lock`: exclusive advisory file lock held by `swx serve` until the server exits.
- `server.log`: log output for auto-started background servers.
- `accounts.json`: saved accounts in display order, provider order, cached usage, and settings (no secrets). Written via `write_atomic` (temp file in the same directory, `sync_all`, persists permissions and follows symlinks, `0600` for new files).
- `desktop.lock`: single-instance advisory file lock held by `switch-desktop`.
- `desktop.json`: what `switch-desktop` hides in its panel and menu bar, and the usage windows its menu bar shows. The server and other clients never read it.
- `logins/<id>.json`: private credential files (`0600`), used directly on non-macOS systems and as an automatic fallback on macOS.

### Vault (`crates/core/src/vault.rs`)

- Every `put` that changes a secret copies the replaced secret to `<id>.previous` before writing `<id>`. `remove` deletes both `<id>` and `<id>.previous`.
- Secrets are cached in memory before persistence so a rotated token remains usable for the lifetime of the server even if storage temporarily fails.
- On macOS, secrets are stored in Keychain service `dev.switch.accounts` via `/usr/bin/security` (15s timeout, read-back verification after every write, passing commands over stdin when within the 4000-byte interactive line limit). If a Keychain write fails (for example when locked), the vault falls back to `logins/<id>.json` so a rotated refresh token is never lost. Reads check the fallback file first, and the next successful Keychain write removes the fallback file.

## Clients

### CLI (`crates/cli`, binary `swx`)

Subcommands:

- `swx` / `swx tui`: open the terminal dashboard.
- `swx serve [--demo] [--port <n>] [--no-auth]`: run the server in the foreground (merging defaults from `server.json` if present). `--no-auth` accepts requests without the password, for a port reachable only from a private network.
- `swx stop`: stop the running server.
- `swx status`: print providers, accounts, and usage.
- `swx add [provider]`: verify and save the provider's current live login (defaults to `claude`).
- `swx login [provider]`: add an account by signing in through a browser, for providers with `sign_in` (defaults to the only such provider). It listens on `127.0.0.1:51121`, opens the sign-in page with `open` or `xdg-open` (not over SSH), always prints the URL, and waits for the browser's redirect. If the port is taken or the browser cannot reach the machine, paste the address the browser ends up on (or the code). The server may run on another machine; only the code is sent to it.
- `swx switch [next|best] [--provider <id>]`: switch using a strategy (defaults to `next`, `claude`).
- `swx use <account>`: switch by 1-based index from `swx status`, alias, email, or account ID.
- `swx refresh`: request a usage refresh and print the updated list.
- `swx reset <account> [grant] [--yes]`: redeem an available banked limit reset on an account after confirmation.
- `swx config [--auto-switch <bool>] [--threshold <pct>] [--cooldown <secs>] [--strategy <best|next>]`: inspect or update auto-switch settings.
- `swx alias <account> [name]`, `swx pause <account>`, `swx resume <account>`, `swx remove <account> [--yes]`: rename, pause/resume, or remove an account.

`status`, `add`, `switch`, `use`, and `refresh` share the same aligned output format (or raw JSON with `--json`): accounts numbered sequentially across all providers, status and plan details, and per-window bars (`━`/`─`) with local reset time, countdown, and fetch age. Bars and percentages turn yellow at 75% and red at 90% when writing to a TTY unless `NO_COLOR` is set. `switch` and `use` also print a confirmation line and a note to restart running tool sessions.

### TUI (`crates/tui`)

An OpenTUI terminal dashboard that connects to the server, streams `/api/event` (reconnecting every 2 seconds on drop), and triggers actions asynchronously without mutating local state. After switching accounts, it shows a hint to restart running tool sessions.

Controls:

- `up`/`down`, `j`/`k`, or mouse click: select an account or provider row.
- `enter`: switch to the selected account.
- `n` / `b`: switch to the next / best account of the selected provider.
- `a`: save the selected provider's current live login.
- `r`: refresh usage.
- `e`: pause or resume the selected account for switching.
- `m`: rename the selected account (empty input clears the alias).
- `u`: open or confirm a banked limit reset on the selected account.
- `x` (then `y`): remove the selected account.
- `J` / `K` (or `shift+down` / `shift+up`): move the selected account within its provider; `]` / `[`: move its provider.
- `s`: toggle auto-switch.
- `+` / `-`: adjust auto-switch threshold in 5% steps between 50% and 100%.
- `t`: toggle between reset countdown and local reset clock time.
- `?`: open/close help; `q` / `ctrl+c`: quit.

### macOS menu bar app (`crates/desktop`, `Switch.app`)

A macOS accessory app built with Rune and AppKit (`objc2`). It enforces a single instance via `desktop.lock`, connects to the server using `SWITCH_CLI` (falling back to the bundled sibling `swx` binary in `Switch.app/Contents/MacOS/swx`, then `PATH`), and follows `/api/event` with exponential reconnect backoff (2s to 30s).

- **Tray icon**: Rendered at 2x scale as an alpha-only template image so macOS adapts it to light and dark menu bars. Draws one mark per provider (from `crates/desktop/assets/`) followed by one column per account shown in the menu bar with its chosen usage windows. Columns keep snapshot order; the active account is rendered at full opacity and inactive accounts at 45%. The tooltip lists the same accounts and windows in the same order. Accounts of usage-only providers are always drawn at full opacity. The right-click menu lists accounts of switchable providers with checkmarks for quick switching, plus Refresh and Quit.
- **Panel**: Opens centered beneath the tray item on whichever screen contains it (accounting for multi-display coordinates and per-display backing scale), fits content height up to screen bounds with scrolling, and closes on focus loss or Escape. Each provider is a section, and each window is one row (label, bar, used percentage, reset) whose columns line up across accounts. Only the active account, or a live login that is not saved yet, is drawn as a card; other accounts are flat with a Switch button unless their provider is usage only or they are signed out. After the account name come its plan, with the organization when accounts share an email, and its status, else "Session limit reached" when a window is used up (active and usage-only accounts only). Signed-out accounts show only their status. When a window includes `period_seconds`, its bar shows a pace tick (elapsed fraction of the window). Bar color follows the used percentage alone: accent below 75%, warning from 75%, danger from 90%, matching the CLI and TUI. Bars of stale, paused, or offline accounts are dimmed. Accounts with banked limit resets show a `Limit resets` row whose pill expands an inline list of grants with `Use` -> `Reset` confirmation. Clicking a "Resets in" heading toggles every row between countdown (`1h 43m`) and local clock time (`15:45`, `Fri 15:45`, or `Dec 1`). A failed switch is shown on the account it was for; a successful switch shows a dismissible hint to restart running tool sessions; other errors and an offline server are shown at the top and cleared when the panel closes. Providers with sign-in offer a Sign in button that opens the browser and completes the local OAuth callback. Right-clicking an account offers Rename (inline), Pause or Resume (switchable providers only), and Remove, which asks for confirmation on the account. Dragging an account reorders it within its provider and dragging a section header reorders providers; the order is saved on the server, so the tray, TUI, and CLI follow it, and Escape or losing focus cancels the drag. The footer holds refresh with the usage age, the auto-switch toggle, and a menu with the auto-switch threshold, Settings, and Quit.
- **Settings**: Replaces the panel's content; Back or Escape returns, and losing focus closes it. Providers and accounts can be shown or hidden separately in the panel and the menu bar, and each provider's menu bar columns can show one or two of its usage windows (its first two primary ones by default). An Auto-switch page configures auto-switch, threshold, cooldown, strategy, and which switchable accounts participate. A Notifications page configures desktop notifications (enabled by default, sent natively via `UNUserNotificationCenter` in `Switch.app` with an `osascript` fallback when run unbundled): master and sound toggles, a test notification button, individual event toggles (auto-switch, usage warning with a 50%-95% threshold, limit reached, limit reset with an exhausted-only or any-usage trigger, and signed out), window and account scopes, and per-account muting. Launch at login manages a per-user LaunchAgent plist (`~/Library/LaunchAgents/dev.switch.desktop.plist`, which works across ad-hoc signed builds). These choices are kept in `desktop.json`, work while the server is offline, and only change what this app draws: hidden accounts still refresh and can be used by auto-switch, and the active account always shows in the panel. The right-click menu follows the panel and the tooltip follows the menu bar; with nothing left in the menu bar, the Switch mark is drawn. Hiding a provider keeps its accounts' own choices. An unreadable `desktop.json` is reported and never overwritten. Order (dragging a handle, or Option and arrow keys), names, pausing, and removal go through the server and are shared with the TUI and CLI.
- **Visual snapshots**: `SWITCH_DESKTOP_SNAPSHOTS=<dir> cargo test -p switch-desktop render_snapshots -- --ignored` renders light and dark panel PNGs and tray previews from fixture snapshots for UI review.

### Web dashboard (planned)

Intended for remote hosts such as a VPS reached only over a tailnet. Because each Switch server manages the live logins on the machine where it runs, a remote web dashboard switches tools on that remote machine rather than on a local Mac; for that reason the desktop app does not link to it. There is no browser login: the server runs with `--no-auth` behind `tailscale serve`, and Tailscale decides who can reach it.

## Running on a server

So far this has been tried only with `--demo` on macOS. The Linux path (Claude Code's login in `~/.claude/.credentials.json`, saved logins in `logins/`) has not been run on a real machine yet.

1. As the user that runs Claude Code, from the repository root: `scripts/install-linux.sh` (Linux x64 and arm64 are supported). It installs `~/.local/bin/swx`; rerun it to update, and it restarts the service if it is running. OpenTUI is linked statically, so the installed `swx` stands alone.
2. Run it as a systemd user service on a fixed port: copy `contrib/switch.service` to `~/.config/systemd/user/`, run `systemctl --user enable --now switch`, and `loginctl enable-linger $USER` so it keeps running after logout.
3. Publish it on the tailnet only: `tailscale serve --bg 4590` serves `https://<machine>.<tailnet>.ts.net` with a valid certificate. Never use `tailscale funnel`, which makes it public.
4. Sign in to Claude Code on the server and save each account with `swx add`. `swx tui` over SSH works against the service.

The server still listens on `127.0.0.1` only; `tailscale serve` is the way in. With `--no-auth`, anyone who can reach the port, including other local users of the machine, can switch and forget accounts.

## Adding a provider

1. Implement `Provider` in `crates/core` (`id`, `name`, `live`, `lock`, `replace_live`, `profile`, `usage` including `period_seconds` when rolling window lengths are known, `renew`, `expired`, and `lineage`). Override `switchable` for a usage-only provider and `sign_in` (an `OAuthSignIn`) when accounts are added by signing in through Switch.
2. Register the provider in `open_engine` in `crates/server/src/lib.rs`.
3. Add a monochrome SVG logo under `crates/desktop/assets/` and map the provider ID in `provider_mark` in `crates/desktop/src/tray_image.rs` (unmapped providers fall back to a solid circle).

No other client changes are required.

## Demo mode

`swx serve --demo` runs `Demo::engine()` using `Store::memory()` and `Vault::memory()`, pre-populated with synthetic Claude and Codex accounts, an unsaved Codex login, and a usage-only Antigravity account. The demo Antigravity sign-in page redirects straight back to the given redirect URI with the code `sam`, so `swx login` works without a browser account. It never touches real logins, the Keychain, or account files on disk.
