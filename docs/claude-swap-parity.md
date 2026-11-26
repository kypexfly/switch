# Feature comparison with claude-swap (`cswap`)

Todo-style feature comparison between Switch and [realiti4/claude-swap](https://github.com/realiti4/claude-swap) (`cswap`). Switch's TUI (`swx` / `swx tui`) is modeled on `cswap`'s interactive dashboard, while Switch uses a shared local HTTP + SSE server and a multi-provider engine underneath.

- `[x]` Implemented in Switch
- `[ ]` Not yet implemented in Switch

## Interactive TUI (`swx` / `swx tui` vs `cswap`)

- [x] Default interactive dashboard launched by the bare command (`swx` / `swx tui`)
- [x] Live account list with plan, status, usage bars, reset countdowns, and fetch age updated over SSE
- [x] Keyboard navigation (`up`/`down`, `j`/`k`) and mouse click selection
- [x] Switch to selected account (`enter`), next (`n`), and best (`b`)
- [x] Save current live login (`a`) and manual refresh (`r`)
- [x] Pause/resume an account in rotation (`e`), inline rename overlay (`m`), and confirmed remove (`x`, then `y`)
- [x] Toggle auto-switch (`s`) and adjust threshold in 5% steps (`+`/`-`)
- [x] Built-in help overlay (`?`) and quit (`q`, `ctrl+c`)
- [ ] Dedicated auto-switch / settings view (like `cswap`'s `g` Auto view) for configuring strategy, cooldown, hysteresis, and notifications
- [x] Usage bar pace tick in the TUI (implemented in `switch-desktop`, not yet in `switch-tui`)
- [x] Toggle between countdown and local reset clock time in the TUI (implemented in `switch-desktop`)
- [x] Reorder account slots and providers directly from the TUI (`J`/`K` or `shift+down`/`shift+up` for accounts, `]`/`[` for providers)
- [ ] Per-account token expiry / refresh health view in the TUI
- [ ] Launch an isolated parallel session from the selected TUI account

## Account management and CLI (`swx` vs `cswap`)

- [x] Save the current live login (`swx add [provider]`)
- [x] Switch to next account (`swx switch next`) and least-used account (`swx switch best`)
- [x] Switch to a specific account by number, alias, email, or ID (`swx use <account>`)
- [x] Usage dashboard printout with bars, reset times, countdowns, and terminal colors (`swx status`, `swx refresh`)
- [x] Account aliases, pause/resume (`enabled`), and account removal in the engine, HTTP API, and TUI
- [x] CLI subcommands for account and config management (`swx alias`, `swx pause` / `swx resume`, `swx remove`, `swx config`)
- [ ] Reorder or swap account slot positions (`cswap move N M`, `cswap swap N M`)
- [x] `next-available` switching strategy that skips exhausted/rate-limited accounts in sequence
- [x] Structured `--json` output flag on CLI commands (`status`, `switch`, `use`, `refresh`) for scripting and statuslines
- [ ] Token health diagnostics flag (`cswap list --token-status`)
- [ ] Full data reset command (`cswap purge`) to wipe saved accounts and vault entries with confirmation
- [ ] Self-upgrade command (`cswap upgrade`)

## Auto-switch and background monitoring (`swx serve` vs `cswap auto`)

- [x] Background polling daemon with per-account adaptive rate-limit scheduling and live-login sync
- [x] Automatic switching when any primary window reaches `threshold` and another enabled account is below it
- [x] Quarantine broken accounts (`needs_login` accounts are excluded from strategy and auto-switching until re-added)
- [x] Switch cooldown and hysteresis margin to avoid ping-ponging between accounts hovering near the threshold
- [x] Configurable auto-switch strategy (`best` vs `next-available`) and optional per-window thresholds
- [ ] One-shot evaluation mode (`--once`) and JSON event stream output for cron/systemd scripting
- [ ] Notification hook command (like `cswap`'s `notify.command`) triggered on auto-switch, exhaustion, or `needs_login`
- [x] Automated OS login service setup (a Linux systemd user unit is provided in `contrib/switch.service`; macOS LaunchAgent is managed by `Switch.app`'s Launch at login setting)

## Parallel sessions (`cswap run`)

- [ ] Launch a single terminal session on a chosen account without changing the global live login (`swx run <account> -- <args...>`)
- [ ] Option to isolate session history (`--no-share-history`) for parallel runs

## Credential safety, storage, and portability

- [x] Claude Code lock ordering (`.oauth_refresh.lock`, `.claude.lock`, `.claude.json.lock`) with heartbeat and stale-lock takeover
- [x] Outgoing live-login preservation with token lineage and profile verification (refusing switches on mismatch or unproven ownership)
- [x] Never renew the provider's live login; renew only saved logins and persist rotated tokens before use
- [x] Preserve machine-local credential fields (`mcpOAuth`, `mcpOAuthClientConfig`, `mcpXaaIdp`, `mcpXaaIdpConfig`, `pluginSecrets`) and `~/.claude.json` key order and float formatting
- [x] Atomic writes with rollback on partial failure; never overwrite an unparseable config file
- [x] Replaced-secret backup (`<id>.previous`) and macOS Keychain with automatic private-file fallback
- [ ] Export and import accounts for backup or machine migration (`cswap export` / `cswap import` with `--account`, `--full`, `--force`, and optional encryption)
- [ ] API-key account support alongside OAuth accounts (opt-in for auto-switch)
- [ ] Extra usage / pay-as-you-go spend tracking when reported by the provider
- [ ] Native OS keyring backends on Linux (Secret Service) and Windows (Credential Manager); non-macOS currently uses `0600` files in `logins/`

## Beyond `claude-swap`

Features Switch already provides (or designs for) outside `claude-swap`'s scope:

- [x] Provider-agnostic engine, protocol, and clients (Claude Code switching and Antigravity usage tracking via browser sign-in, ready for additional tools)
- [x] Shared local HTTP + SSE server keeping CLI, TUI, and desktop clients in sync
- [x] Native macOS menu bar app (`Switch.app`) with multi-account template tray icon, popover usage cards, pace ticks, drag-to-reorder, right-click account menu, and panel/menu bar visibility settings
- [x] In-memory `--demo` mode with synthetic multi-provider accounts for safe development and UI testing
- [ ] Remote web dashboard (served over a private network such as Tailscale with `--no-auth`)
