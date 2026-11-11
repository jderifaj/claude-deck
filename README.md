# Claude Deck

A VS Code-style desktop app (Rust + [GPUI](https://gpui.rs)) for running many Claude Code sessions side by side.

```sh
./scripts/bundle.sh                 # builds dist/Claude Deck.app
cargo run --release -- ~/Sites/proj # or run directly, opening sessions for folders
```

## Sessions
- **⌘T** new session in the current project · **⌘O** open folder · **⌘W** close
- **⌥⌘←/→** switch sessions · **⌘\\** grid ↔ tabs · **⌘B** side bar · **⇧⌘E / ⇧⌘T** sessions / themes
- Shift/Option+Enter inserts a newline in Claude's prompt; ⌘C / ⌘V copy and paste.

## Browsers
The Chrome, Safari and Firefox buttons in the far-left bar (shown for those installed) open the active session's
local page: a server listening on a local port that the session started or that runs in its folder (preferring
the session's own, then the lowest port), else the folder's `index.html`. A notice shows which URL opened.

Below them, the Device Hub button (shown once Xcode is installed) lists the iPhone and iPad simulators
Xcode has (any you add in DeviceHub show up the next time the menu opens). Picking one opens the same page
in Safari on that device, booting it if needed. A device on several iOS versions is listed once: the
running copy, else the newest. The simulator window opens in DeviceHub.app (Xcode 27 on), or
Simulator.app on older Xcodes.

## Safety gates
Sessions working on Claude Deck itself (its repo, or one of its worktrees: a folder whose `Cargo.toml` names the
`claude-deck` package) start with Claude Deck's own `--settings` file (`~/.claude-deck/sessions/`, see
`src/guard.rs`). That file ranks above your user and project settings, and its deny/ask rules win over any allow rule:
- **Shell sandbox** (macOS Seatbelt): commands and anything they start can write only inside the session's
  folder and temp, can't read secrets (`~/.ssh`, cloud/`gh` credentials, Keychains, Mail, Messages, browser
  data, Photos…), and reach new network hosts only with approval. Running a command outside the sandbox always asks.
- **File tools**: can't read those secrets; can't edit system folders, `~/Library`, shell startup files,
  `~/.gitconfig`, Claude's settings or these gates; ask before reading Documents, Desktop, Downloads and other
  personal folders (except the one the session works in).
- **Commands**: `sudo`, `su`, `security` (Keychain), `csrutil` and `rm -rf` of `/` or `~` are refused;
  `osascript`, `launchctl`, `crontab`, `defaults write`, `diskutil`, `chown` and recursive `chmod` ask.
- Bypass-permissions mode is disabled. If the gates can't be written, the session doesn't start.

Sessions in any other folder run with your normal Claude Code settings (plus the status line), with none of these gates.

## File isolation
The first session in a git repo runs in the repo itself. Every additional session in the same repo gets its own
git worktree at `<parent>/<repo>.worktrees/deck-N` on branch `deck/N`, so sessions never write to the same files.
Merge those branches when you're done (`git merge deck/2`).

When a session closes (or the app quits), its worktree is removed only if it has no uncommitted changes and
no new commits; otherwise it's kept and you're told where it is. Non-git folders can only have one session.
Note that worktrees contain only tracked files: ignored files such as `.env` or `node_modules` aren't copied.

## Accounts
Every window shows which Claude account it runs as (colored chip in the pane header, tab avatar, side bar and
status bar). Click the chip to switch that window to another account, carrying the current conversation over or
starting fresh, or choose **Add another account…** and sign in inside the window.

Each account is a separate Claude Code profile: your normal login (`~/.claude`) is the default, and added
accounts live in `~/.claude-deck/accounts/<name>` (used as `CLAUDE_CONFIG_DIR`), each with its own login and
history. Login status is checked with `claude auth status` every 10 seconds.

## Themes
The Themes view (⇧⌘T) sets the default theme, which the app and every new window use. Themes come from
VS Code's built-in themes and theme extensions installed in VS Code. To tell windows apart, give a window its
own theme with the palette button in its header.

## Usage bar
The top bar shows the 5-hour and 7-day plan usage for each signed-in account, with reset countdowns.
Sessions started by Claude Deck use `claude-deck --statusline <account>` as Claude Code's status line, which
records the `rate_limits` Claude Code reports in `~/.claude-deck/usage/`. It fills in after Claude's first reply
in a session, and only reflects activity seen by Claude Deck sessions. No credentials are read.

## Settings
`~/.claude-deck/settings.json`: `claudeCommand` (e.g. `"claude --model opus"`), `alwaysIsolate`,
`fontFamily`, `fontSize`, `colorTheme`, `recentProjects`.

## License
MIT, see [LICENSE](LICENSE). The vendored copy of GPUI in `vendor/gpui` is Apache-2.0
(see `vendor/gpui/LICENSE-APACHE`); its local changes are listed in `Cargo.toml`.

Claude Deck is an independent project. It isn't made, endorsed or supported by Anthropic.
"Claude" and the Claude logo are trademarks of Anthropic.
