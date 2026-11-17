//! Whether a window's branch has commits to push or pull, for the indicator
//! beside its branch badge. Everything here runs `git`, so call it off the
//! main thread.

use std::{
    path::Path,
    process::Command,
    time::Duration,
};

use crate::{git_identity::GitIdentity, webserver::output_within};

/// Commits the branch has that its upstream doesn't (`ahead`), and the
/// reverse (`behind`). With no upstream, `ahead` counts commits on no remote
/// branch at all, and `behind` is 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SyncStatus {
    pub ahead: u32,
    pub behind: u32,
    pub has_upstream: bool,
}

/// Longest a `git fetch` may take before it's stopped (a slow or hung remote).
const FETCH_LIMIT: Duration = Duration::from_secs(20);
const COUNT_LIMIT: Duration = Duration::from_secs(5);

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

/// Count ahead/behind from the local refs only (no network). None outside a
/// repo, or on a detached HEAD with no upstream.
pub fn status(dir: &Path) -> Option<SyncStatus> {
    let counted = output_within(
        git(dir).args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"]),
        COUNT_LIMIT,
    );
    if let Some(out) = counted {
        let (ahead, behind) = parse_left_right(&out)?;
        return Some(SyncStatus { ahead, behind, has_upstream: true });
    }
    // No upstream: commits that aren't on any remote branch yet.
    output_within(git(dir).args(["symbolic-ref", "-q", "HEAD"]), COUNT_LIMIT)?;
    let out = output_within(git(dir).args(["rev-list", "--count", "HEAD", "--not", "--remotes"]), COUNT_LIMIT)?;
    Some(SyncStatus { ahead: out.trim().parse().ok()?, behind: 0, has_upstream: false })
}

/// `git fetch` the current branch's remote quietly, as the window's GitHub
/// identity when it has one. Never prompts: a remote that wants a password
/// or an unknown host key just fails. Returns whether it worked.
pub fn fetch(dir: &Path, identity: Option<&GitIdentity>) -> bool {
    let mut cmd = git(dir);
    // The same HTTPS helper the window's own git uses; an empty value first
    // drops the helpers from git config. `-c` goes before the subcommand.
    if let Some(helper) = identity.and_then(GitIdentity::https_helper) {
        let key = "credential.https://github.com.helper";
        cmd.arg("-c").arg(format!("{key}=")).arg("-c").arg(format!("{key}={helper}"));
    }
    cmd.args(["fetch", "--quiet", "--no-tags", "--no-recurse-submodules"]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let ssh = identity.and_then(GitIdentity::ssh_command).unwrap_or_else(|| "ssh".into());
    cmd.env("GIT_SSH_COMMAND", format!("{ssh} -o BatchMode=yes"));
    if let Some(gh) = identity.and_then(|g| g.gh_config_dir.as_ref()) {
        cmd.env("GH_CONFIG_DIR", crate::git_identity::expand_tilde(gh));
    }
    output_within(&mut cmd, FETCH_LIMIT).is_some()
}

/// `git rev-list --left-right --count` prints "<ahead>\t<behind>".
fn parse_left_right(out: &str) -> Option<(u32, u32)> {
    let mut parts = out.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_left_right_counts() {
        assert_eq!(parse_left_right("2\t5\n"), Some((2, 5)));
        assert_eq!(parse_left_right("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right(""), None);
        assert_eq!(parse_left_right("x\t1"), None);
    }
}
