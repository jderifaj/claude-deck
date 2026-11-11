# Known issues

## Browser button can open another project's server on the same port

**Seen:** 2026-11-02, in a session for `~/Sites/app-a`.

**What happened:** The app-a app runs in Docker with port `4321` published on all addresses (`*:4321`,
from `com.docker.backend`). At the same time, `~/Sites/site-b` had an `astro dev` server
listening only on `[::1]:4321`. When opened, `http://localhost:4321` showed site B's page
instead of app A, because the browser tries `::1` first and the more specific bind wins there.
`http://127.0.0.1:4321` showed the right app.

```
$ lsof -nP -iTCP:4321 -sTCP:LISTEN
node       65164  TCP [::1]:4321 (LISTEN)   # site-b astro dev
com.docke  97352  TCP *:4321 (LISTEN)       # app-a web container
```

**Why Claude Deck gets this wrong** (`src/browser.rs`):

1. `find_local_page` always builds `http://localhost:{port}`. It throws away the address the chosen process
   listens on, so when two processes share a port on different addresses, `localhost` can reach the wrong one.
2. `listening_ports` keeps only the port from each `lsof` `n` line, not the host, so it can't spot the clash.
3. Servers started with `docker compose up -d` are invisible to `find_server_port`. The listener is
   `com.docker.backend`, which isn't a child of the session and whose cwd isn't the project folder. A
   Docker-only project gets no page, or gets another project's server if that one matches.

**Possible fixes:**

- Keep the listen address with each port and build the URL from it: `127.0.0.1` for IPv4 or wildcard binds,
  `[::1]` for IPv6 loopback. Use `localhost` only when one process owns the port on every address.
- When the chosen port is also bound by a process outside the session, show that in the notice ("port 4321
  is also used by site-b").
- For Docker, match `docker compose ps --format json` (`Project`/`WorkingDir` and `Publishers`) against the
  session folder to find its published ports.

**Update (same day):** Point 3 is confirmed on its own. With app-a moved to port 4322 and nothing else on
that port, the Chrome button opens nothing for the session. The only listener is `com.docker.backend`
(cwd `~/Library/Containers/com.docker.docker/Data`), so `find_server_port` returns `None`. The project has no
`index.html` to fall back to.

## A window's Git identity overrides the repo's own author, and doesn't cover HTTPS pushes

**Seen:** 2026-11-02, in a session for `~/Sites/app-a` (remote
`https://github.com/me/app-a.git`, private, owned by `me`).

**What happened:**

- The repo's `.git/config` sets `user.name = me` and
  `user.email = 12345+me@users.noreply.github.com`, and its earlier commits all use that author.
- The window ran under the `work-user` identity, so `GitIdentity::apply_env` (`src/git_identity.rs`) set
  `GIT_AUTHOR_*` and `GIT_COMMITTER_*` to `work-user <me@work.example>`. Environment variables beat
  `.git/config`, so a commit in that repo went out as the work identity without any warning. It was caught
  before pushing and rewritten by hand.
- After a restart the window had `My Name <me@example.com>`. That's still not the repo's
  configured author, and it would put a real email address on GitHub instead of the noreply one.
- The push failed with "Repository not found". The identity's `ssh_key` only applies to SSH remotes, and this
  remote is HTTPS, so Git used the `osxkeychain` login for github.com, which belonged to the work account.
  `gh` was also logged in as `work-user`. It was fixed outside Claude Deck with `gh auth switch` and
  `gh auth setup-git`.

**Possible fixes:**

- Don't override an author the repo sets itself. If the repo's local config has `user.name`/`user.email`,
  leave `GIT_AUTHOR_*`/`GIT_COMMITTER_*` unset, or warn in the window when they differ.
- Pick the identity from the remote's owner: `github.com/me/...` → the `me` identity.
- Cover HTTPS remotes. Point Git's credentials at the identity's `gh` login, for example by setting
  `GIT_CONFIG_*` to `credential.https://github.com.helper = !gh auth git-credential` with that identity's
  `GH_CONFIG_DIR`, so a push uses the same account as the author.
- Show the active identity (author, email, `gh` account) in the window so a mismatch is visible before
  committing.

**Workaround in place (same day), and what Claude Deck should do instead:** the user wants each project
directory to keep its own GitHub account, without switching the account for the whole Mac. A user-level Claude
Code SessionStart hook does this for now: `~/.claude/hooks/github-account.sh`, registered in
`~/.claude/settings.json`.

- The account is saved per repo in `.git/config` as `claude.githubUser`, which is never committed. The first time
  a repo is seen, the hook picks the remote's owner if `gh` is logged in as that account, otherwise `gh`'s active
  account, and saves it. `git config claude.githubUser <login>` changes it.
- For that session only, through `CLAUDE_ENV_FILE`:
  `GH_TOKEN="$(gh auth token --user <login>)"`. That command is re-run for each shell command, so the token is
  never written to a file. With `gh auth setup-git`, `git push` uses that account too. The commit author comes
  from the repo's own `user.name`/`user.email`; if the repo has none, it's set to the account's noreply address.
- It never runs `gh auth switch`, so sessions for other projects keep their own accounts.

Moving this into Claude Deck would fix the problems above at the source:
- When `GitIdentity::apply_env` starts a window, read `claude.githubUser` from the folder's repo, or match the
  remote's owner, and use the identity for that login instead of the window's default.
- Use the repo's own `user.name`/`user.email` for the author when it has them.
- Set `GH_TOKEN` (or a per-login `GH_CONFIG_DIR`) for the window, so HTTPS pushes use the same account as the
  author.
- Show the result in the window ("GitHub: me · saved for this folder"), and let the user change it there,
  writing `claude.githubUser`.
- Once Claude Deck does this, the Claude Code hook can be removed.

## Beachball at launch while windows look up their GitHub identity

**Seen:** 2026-11-04, once, on restarting the app. No hang report was written, so the cause below is the
likely one from reading the code, not confirmed with a sample.

**What happened:** The app showed the spinning beachball before its window finished loading. It hadn't done
this before commit 064cbc3 ("Pick a window's GitHub identity from its folder first").

**Why Claude Deck gets this wrong:** `Settings::git_for_folder` (`src/settings.rs`) calls
`git_identity::folder_logins`, which runs two `git` processes one after the other (`git config --get
claude.githubUser`, `git remote get-url origin`) and waits for them, on the main UI thread:

1. once for every window opened at launch (`Workbench::add_session_as`, via `git_for`),
2. on every account switch (`perform_switch`),
3. on every render of the Git menu (`render_git_menu`, twice per frame),
4. when saving a choice (`save_folder_login` in `request_git_switch`).

They take about 20ms when warm. But `/usr/bin/git` is an `xcrun` shim, and its first call after a reboot,
an Xcode update or a cleared xcrun cache can take several seconds. With a few windows opening at once, the
main thread stalls long enough to beachball.

**Possible fixes:**

- Never start a process from the main thread for this. Keep a per-folder cache on `Workbench`
  (`HashMap<PathBuf, Vec<String>>` of folder logins), filled on the background executor, the way
  `catalog::scan()` and `browser::installed()` already are.
- At launch, look up every folder being opened (and the recent projects) in one background task, then
  create those windows' sessions when it returns, so each terminal starts with the right identity rather than
  restarting after a late lookup.
- `git_for` and `render_git_menu` read only the cache. A folder not yet cached falls back to the Claude
  account's default and starts a background lookup that calls `cx.notify()` when it finishes.
- Run `save_folder_login` in the background too, and update the cache right away, so the menu shows
  "saved for this folder" with no wait.
- Optionally, read `.git/config` directly (finding the repo's common dir for worktrees) instead of running
  `git`, which avoids the xcrun shim entirely.

**Fixed (same day):** opening a window now does every `git` call in the background (`Opening::load` in
`src/workbench.rs`): the repo root and branch lookups that were already on the main thread, as well as the
folder's logins. The choice of main checkout or worktree, and adding the window, still happen together on the
main thread, so two windows opened at once can't both take the main checkout. Folder logins are cached per
window, the Git menu and account switches read only that cache, and saving a choice writes `.git/config`
in the background. The smoke test still blocks on `git`. The `.git/config`-direct idea wasn't needed.

**To confirm if it happens again:** run `sample <pid> 5` (or Activity Monitor → Sample Process) during the
hang and look for `git_identity::git` or `waitpid` on the main thread.
