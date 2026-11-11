//! Keeps concurrent Claude sessions in the same project from editing the
//! same files by giving every extra session its own git worktree.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Result, bail};

#[derive(Clone, Debug)]
pub struct Workspace {
    /// Directory Claude runs in.
    pub cwd: PathBuf,
    /// Main checkout (repo root, or the folder itself if not a git repo).
    pub project: PathBuf,
    /// Set when this session runs in a dedicated worktree.
    pub isolation: Option<Isolation>,
}

#[derive(Clone, Debug)]
pub struct Isolation {
    pub dir: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

impl Workspace {
    pub fn branch(&self) -> Option<String> {
        match &self.isolation {
            Some(iso) => Some(iso.branch.clone()),
            None => git(&self.cwd, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty()),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).ok().map(PathBuf::from)
}

/// The main checkout of `dir`, used to group sessions by project.
pub fn project_of(dir: &Path) -> PathBuf {
    repo_root(dir).unwrap_or_else(|| dir.to_path_buf())
}

pub fn shared(dir: &Path) -> Workspace {
    Workspace { cwd: dir.to_path_buf(), project: project_of(dir), isolation: None }
}

/// Create `<parent>/<repo>.worktrees/deck-N` on a new `deck/N` branch from HEAD.
pub fn isolated(dir: &Path) -> Result<Workspace> {
    // Picking a free name and creating the worktree must be atomic, or two
    // sessions started together would race for the same deck-N.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(root) = repo_root(dir) else {
        bail!(
            "{} is not a git repository, so a second session can't be isolated. \
             Run `git init` and make a commit first.",
            dir.display()
        );
    };
    let Ok(base_commit) = git(&root, &["rev-parse", "HEAD"]) else {
        bail!("{} has no commits yet; commit once so sessions can branch from it", root.display());
    };
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let container = root.parent().unwrap_or(&root).join(format!("{name}.worktrees"));

    let mut n = 1;
    let (wt_dir, branch) = loop {
        let wt_dir = container.join(format!("deck-{n}"));
        let branch = format!("deck/{n}");
        let branch_taken = git(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        if !wt_dir.exists() && !branch_taken {
            break (wt_dir, branch);
        }
        n += 1;
    };

    std::fs::create_dir_all(&container)?;
    git(&root, &["worktree", "add", "-b", &branch, &wt_dir.to_string_lossy(), &base_commit])?;

    // Open the same subfolder inside the worktree.
    let cwd = dir.strip_prefix(&root).map(|rel| wt_dir.join(rel)).unwrap_or_else(|_| wt_dir.clone());
    Ok(Workspace {
        cwd,
        project: root,
        isolation: Some(Isolation { dir: wt_dir, branch, base_commit }),
    })
}

pub enum Cleanup {
    NotIsolated,
    Removed { branch: String },
    Kept { branch: String, dir: PathBuf, reason: String },
}

/// Remove an isolated worktree if nothing would be lost; otherwise keep it.
pub fn cleanup(ws: &Workspace) -> Cleanup {
    let Some(iso) = &ws.isolation else { return Cleanup::NotIsolated };
    let keep = |reason: String| Cleanup::Kept { branch: iso.branch.clone(), dir: iso.dir.clone(), reason };

    match git(&iso.dir, &["status", "--porcelain"]) {
        Ok(s) if !s.is_empty() => return keep("it has uncommitted changes".into()),
        Err(e) => return keep(e.to_string()),
        _ => {}
    }
    let ahead = git(&ws.project, &["rev-list", "--count", &format!("{}..{}", iso.base_commit, iso.branch)])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    if ahead > 0 {
        return keep(format!("it has {ahead} new commit{}", if ahead == 1 { "" } else { "s" }));
    }
    if let Err(e) = git(&ws.project, &["worktree", "remove", &iso.dir.to_string_lossy()]) {
        return keep(e.to_string());
    }
    let _ = git(&ws.project, &["branch", "-D", &iso.branch]);
    if let Some(container) = iso.dir.parent() {
        let _ = std::fs::remove_dir(container); // only succeeds when empty
    }
    Cleanup::Removed { branch: iso.branch.clone() }
}

/// Directories some process is currently running in (via `lsof`), or
/// `None` if that can't be determined.
fn busy_dirs() -> Option<Vec<PathBuf>> {
    let out = Command::new("lsof").args(["-d", "cwd", "-Fn"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let dirs: Vec<PathBuf> = text.lines().filter_map(|l| l.strip_prefix('n')).map(PathBuf::from).collect();
    (!dirs.is_empty()).then_some(dirs)
}

/// Remove `deck-N` worktrees left behind by a crash or force quit: only
/// those with no changes, no commits missing from the main checkout's
/// HEAD, and no process running inside them. Returns the removed paths.
pub fn sweep_orphans(projects: &[PathBuf]) -> Vec<PathBuf> {
    let Some(busy) = busy_dirs() else { return Vec::new() };
    let mut removed = Vec::new();
    for project in projects {
        let Ok(list) = git(project, &["worktree", "list", "--porcelain"]) else { continue };
        for block in list.split("\n\n") {
            let dir = block.lines().find_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from);
            let branch = block.lines().find_map(|l| l.strip_prefix("branch refs/heads/"));
            let (Some(dir), Some(branch)) = (dir, branch) else { continue };
            let is_deck = branch.starts_with("deck/")
                && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("deck-"))
                && dir.parent().is_some_and(|p| p.to_string_lossy().ends_with(".worktrees"));
            if !is_deck || busy.iter().any(|b| b.starts_with(&dir)) {
                continue;
            }
            let clean = git(&dir, &["status", "--porcelain"]).is_ok_and(|s| s.is_empty());
            let merged = git(project, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok();
            if clean && merged && git(project, &["worktree", "remove", &dir.to_string_lossy()]).is_ok() {
                let _ = git(project, &["branch", "-D", branch]);
                if let Some(container) = dir.parent() {
                    let _ = std::fs::remove_dir(container);
                }
                removed.push(dir);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let base = std::env::temp_dir().join(format!("deck-test-{}-{}", std::process::id(), rand_suffix()));
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for args in [&["init", "-q"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"]] {
            git(&root, args).unwrap();
        }
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("sub/b.txt"), "b").unwrap();
        git(&root, &["add", "."]).unwrap();
        git(&root, &["commit", "-qm", "init"]).unwrap();
        root.canonicalize().unwrap()
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn isolated_sessions_get_distinct_worktrees_and_clean_up() {
        let root = repo();
        let a = isolated(&root).unwrap();
        let b = isolated(&root.join("sub")).unwrap();
        let (ia, ib) = (a.isolation.clone().unwrap(), b.isolation.clone().unwrap());
        assert_ne!(ia.dir, ib.dir);
        assert_ne!(ia.branch, ib.branch);
        assert_eq!(a.project, root);
        assert!(b.cwd.ends_with("sub"), "subfolder preserved: {:?}", b.cwd);
        assert!(ia.dir.join("a.txt").exists());

        // Writing in one worktree doesn't touch the other or the main checkout.
        std::fs::write(ia.dir.join("a.txt"), "changed").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(ib.dir.join("a.txt")).unwrap(), "a");

        // Dirty worktree is kept; clean one is removed with its branch.
        assert!(matches!(cleanup(&a), Cleanup::Kept { .. }));
        assert!(matches!(cleanup(&b), Cleanup::Removed { .. }));
        assert!(!ib.dir.exists());
        assert!(git(&root, &["rev-parse", "--verify", "--quiet", &ib.branch]).is_err());

        // Committed work is kept too.
        git(&ia.dir, &["commit", "-qam", "work"]).unwrap();
        match cleanup(&a) {
            Cleanup::Kept { reason, .. } => assert!(reason.contains("1 new commit"), "{reason}"),
            _ => panic!("expected kept"),
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn sweep_removes_only_unused_clean_worktrees() {
        let root = repo();
        let idle = isolated(&root).unwrap();
        let dirty = isolated(&root).unwrap();
        let committed = isolated(&root).unwrap();
        std::fs::write(dirty.cwd.join("a.txt"), "edit").unwrap();
        std::fs::write(committed.cwd.join("new.txt"), "x").unwrap();
        git(&committed.cwd, &["add", "."]).unwrap();
        git(&committed.cwd, &["commit", "-qm", "work"]).unwrap();

        let removed = sweep_orphans(&[root.clone()]);
        assert_eq!(removed, vec![idle.isolation.unwrap().dir]);
        assert!(dirty.cwd.exists() && committed.cwd.exists());
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn non_git_folder_cannot_be_isolated() {
        let dir = std::env::temp_dir().join(format!("deck-plain-{}", rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(isolated(&dir).is_err());
        assert!(shared(&dir).isolation.is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
