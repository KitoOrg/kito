//! Commits made outside Kito.
//!
//! Kito writes the git history of a repository and pushes it. On a team
//! not everybody works through Kito: somebody commits with plain git and
//! pushes to the same remote. Until now that stopped the pushes, which are
//! never forced, until somebody put the two histories together by hand.
//!
//! Now the commits the remote has and the main lane does not are brought
//! in like any other work: through a lane. The lane starts from the
//! checkpoint the commits were made on; each commit is replayed in it with
//! its own author, its message as the reason and its hash as the task; and
//! the lane is integrated like every lane, merged function by function
//! with what the main lane did meanwhile, and checked. The checkpoint of
//! that integration is exported as a git merge with the remote's last
//! commit as second parent: the remote can then fast-forward to it.
//!
//! This module does the part that needs the repository: it is given a
//! commit that was fetched, and fills the lane. Fetching, integrating and
//! pushing are the daemon's.

use crate::error::{Error, Result};
use crate::repo::Repo;
use kito_core::{Author, AuthorKind, FileMode, LaneId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The file that remembers commits waiting in a lane to be integrated.
const WAITING: &str = "pull.json";

/// The name that signs the lane (each commit keeps its own author).
const AGENT: &str = "git";

/// Commits that were brought into a lane and are not on the main lane yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Waiting {
    /// The last commit brought in.
    tip: String,
    lane: String,
}

/// What a pull brought into a lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pull {
    /// The lane to integrate.
    pub lane: LaneId,
    /// How many commits were replayed in it by this call.
    pub commits: usize,
    /// The commit the lane is now up to.
    pub tip: String,
}

/// A git commit id from its 40 hexadecimal digits.
pub(crate) fn oid(hex: &str) -> Option<[u8; 20]> {
    let hex = hex.trim();
    if hex.len() != 40 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 20];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * at..2 * at + 2], 16).ok()?;
    }
    Some(out)
}

fn hex(id: &[u8; 20]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn command(root: &Path, args: &[&str]) -> Command {
    let mut git = Command::new("git");
    git.arg("-C").arg(root).args(args).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    git
}

/// What a git command printed; an error with what it complained about.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = command(root, args).output().map_err(|e| Error::Other(format!("git: {e}")))?;
    match output.status.success() {
        true => Ok(output.stdout),
        false => {
            let said = String::from_utf8_lossy(&output.stderr);
            let last = said.lines().rfind(|line| !line.trim().is_empty()).unwrap_or("it failed").trim();
            Err(Error::Other(format!("git {}: {last}", args.first().copied().unwrap_or_default())))
        }
    }
}

/// `true` if a git command that answers with its exit code says yes.
fn says_yes(root: &Path, args: &[&str]) -> bool {
    command(root, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn line(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// A file of a commit; `None` if the commit has no such file.
fn blob(root: &Path, commit: &str, path: &str) -> Option<Vec<u8>> {
    let output = command(root, &["cat-file", "blob", &format!("{commit}:{path}")])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// How a commit has a file: as a file, an executable or a link.
fn mode(root: &Path, commit: &str, path: &str) -> FileMode {
    let listed = git(root, &["ls-tree", commit, "--", path]).map(line).unwrap_or_default();
    match listed.split_whitespace().next() {
        Some("100755") => FileMode::Executable,
        Some("120000") => FileMode::Symlink,
        _ => FileMode::Regular,
    }
}

impl Repo {
    fn waiting_file(&self) -> PathBuf {
        self.root.join(kito_store::STORE_DIR).join(WAITING)
    }

    /// The commits brought into a lane that is not integrated yet: the
    /// last of them, and the lane.
    pub fn pull_waiting(&self) -> Option<(String, LaneId)> {
        let bytes = std::fs::read(self.waiting_file()).ok()?;
        let waiting: Waiting = serde_json::from_slice(&bytes).ok()?;
        let lane: LaneId = waiting.lane.parse().ok()?;
        self.lanes.contains_key(&lane).then_some((waiting.tip, lane))
    }

    /// The commit the last exported checkpoint was exported as.
    fn exported_tip(&self) -> Option<String> {
        let checkpoints: Vec<_> = self.store.checkpoints().collect();
        checkpoints.iter().rev().find_map(|checkpoint| checkpoint.git_commit.as_ref().map(hex))
    }

    /// Brings into a lane the commits that `tip` (a commit of this
    /// repository's git, usually just fetched) has and the main lane does
    /// not. Returns the lane to integrate; `None` if there is nothing to
    /// bring. Called again with a later commit, it adds to the same lane.
    pub fn pull_import(&mut self, tip: &str) -> Result<Option<Pull>> {
        let root = self.root.clone();
        let tip = line(git(&root, &["rev-parse", "--verify", "--quiet", &format!("{tip}^{{commit}}")])?);
        let ours = self
            .exported_tip()
            .ok_or_else(|| Error::Other("no checkpoint of this repository is in git yet".into()))?;
        if says_yes(&root, &["merge-base", "--is-ancestor", &tip, &ours]) {
            return Ok(None);
        }
        // A lane that is waiting: the same commits, or the same and more.
        let waiting = self.pull_waiting();
        let (lane, from) = match waiting {
            Some((was, lane)) if was == tip => return Ok(Some(Pull { lane, commits: 0, tip })),
            Some((was, lane)) if says_yes(&root, &["merge-base", "--is-ancestor", &was, &tip]) => {
                // More commits on top of the ones it has: it declares what they change too.
                let more = changed_between(&root, &was, &tip)?;
                if !more.is_empty() {
                    self.extend_scope(lane, &more)?;
                }
                (lane, was)
            }
            other => {
                // The remote was rewritten under the lane: it starts again.
                if let Some((_, lane)) = other {
                    self.close_lane(lane)?;
                }
                self.pull_lane(&ours, &tip)?
            }
        };
        let listed =
            line(git(&root, &["rev-list", "--reverse", "--first-parent", &format!("{from}..{tip}")])?);
        let commits: Vec<&str> = listed.lines().collect();
        let signs = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?.work.author.clone();
        let replayed = commits.iter().try_for_each(|commit| self.pull_commit(lane, commit));
        if let Some(at) = self.lanes.get_mut(&lane) {
            at.work.author = signs;
        }
        if let Err(e) = replayed {
            // Nothing half brought in is left behind.
            self.close_lane(lane)?;
            let _ = std::fs::remove_file(self.waiting_file());
            return Err(e);
        }
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.record.git_tip = Some(tip.clone());
        let state = at.record.state;
        self.put_lane_state(lane, state)?;
        self.store.commit()?;
        let waiting = Waiting { tip: tip.clone(), lane: lane.to_string() };
        let bytes = serde_json::to_vec(&waiting).map_err(|e| Error::Other(e.to_string()))?;
        std::fs::write(self.waiting_file(), bytes).map_err(Error::io(&self.waiting_file()))?;
        Ok(Some(Pull { lane, commits: commits.len(), tip }))
    }

    /// Opens the lane the commits are replayed in: from the checkpoint
    /// they were made on, declaring every file they change. Returns it
    /// with the commit to replay from.
    fn pull_lane(&mut self, ours: &str, tip: &str) -> Result<(LaneId, String)> {
        let root = self.root.clone();
        let base = git(&root, &["merge-base", ours, tip]).map(line).map_err(|_| {
            Error::Other(
                "the remote's branch has no history in common with this repository: there is nothing to \
                 merge it on"
                    .into(),
            )
        })?;
        // The first checkpoint whose commit has that commit in its history:
        // the commit itself when Kito made it, the merge that brought it
        // in when it came from outside.
        let exported: Vec<(String, String)> = self
            .store
            .checkpoints()
            .filter_map(|checkpoint| Some((checkpoint.id.to_string(), hex(checkpoint.git_commit.as_ref()?))))
            .collect();
        let start = exported
            .iter()
            .find(|(_, commit)| {
                *commit == base || says_yes(&root, &["merge-base", "--is-ancestor", &base, commit])
            })
            .map(|(checkpoint, _)| checkpoint.clone())
            .ok_or_else(|| {
                Error::Other("the remote's commits start from a commit this repository never had".into())
            })?;
        let mut scope = changed_between(&root, &base, tip)?;
        if scope.is_empty() {
            scope.push("./".to_owned());
        }
        let short: String = tip.chars().take(10).collect();
        let opened = self.open_lane(
            AGENT,
            None,
            Some(&start),
            &format!("commits made with git, up to {short}"),
            &scope,
        )?;
        let lane: LaneId = opened.id.parse().map_err(|_| Error::Other("the lane has no number".into()))?;
        Ok((lane, base))
    }

    /// Replays one commit in the lane: what it changed against its first
    /// parent, under its author, with its message as the reason.
    fn pull_commit(&mut self, lane: LaneId, commit: &str) -> Result<()> {
        let root = self.root.clone();
        let said = git(&root, &["show", "-s", "--format=%an%x00%ae%x00%s", commit])?;
        let said = String::from_utf8_lossy(&said).trim_end().to_owned();
        let mut parts = said.splitn(3, '\0');
        let (name, email, subject) = (
            parts.next().unwrap_or(AGENT),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let short: String = commit.chars().take(10).collect();
        let subject = match subject.trim().is_empty() {
            true => format!("commit {short}"),
            false => subject.trim().to_owned(),
        };
        self.set_intent(lane, &subject, Some(format!("git {short}")), None)?;
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.work.author = Author {
            kind: AuthorKind::Human,
            name: name.to_owned(),
            email: Some(email.to_owned()).filter(|email| !email.is_empty()),
        };
        let work = at.work.root.clone();
        let parent = format!("{commit}^1");
        let listed = git(
            &root,
            &["diff-tree", "-r", "--no-renames", "--no-commit-id", "--name-status", "-z", &parent, commit],
        )?;
        let mut fields = listed.split(|byte| *byte == 0).filter(|field| !field.is_empty());
        while let (Some(_status), Some(path)) = (fields.next(), fields.next()) {
            let path = String::from_utf8_lossy(path).into_owned();
            let rel = crate::scope::clean(&path)?;
            let file = work.join(&rel);
            let (was, is) = (blob(&root, &parent, &path), blob(&root, commit, &path));
            let here = std::fs::read(&file).ok();
            match is {
                // The commit removed it.
                None if here.is_none() => {}
                None if here == was => crate::integrate::remove_in(&work, &rel)?,
                None => return Err(does_not_apply(&short, &subject, &path)),
                Some(is) => {
                    // The lane has the file as the commit found it: it
                    // gets it as the commit left it. Otherwise the change
                    // is carried over, if it applies.
                    let content = match here {
                        ref here if *here == was || here.as_ref() == Some(&is) => is,
                        Some(_) => self
                            .carry_over(&file, was.as_deref().unwrap_or_default(), &is)
                            .ok_or_else(|| does_not_apply(&short, &subject, &path))?,
                        None => return Err(does_not_apply(&short, &subject, &path)),
                    };
                    crate::repo::write_file(&work, &rel, mode(&root, commit, &path), &content)?;
                }
            }
        }
        self.scan_lane(lane)?;
        Ok(())
    }

    /// The change from `was` to `is`, made on a file that has moved on
    /// from `was`: a three-way merge by lines. `None` if they do not go
    /// together.
    fn carry_over(&self, file: &Path, was: &[u8], is: &[u8]) -> Option<Vec<u8>> {
        let scratch = self.root.join(kito_store::STORE_DIR).join("pull-scratch");
        std::fs::create_dir_all(&scratch).ok()?;
        let (base, theirs) = (scratch.join("base"), scratch.join("theirs"));
        std::fs::write(&base, was).ok()?;
        std::fs::write(&theirs, is).ok()?;
        let merged = Command::new("git")
            .arg("merge-file")
            .arg("-p")
            .args([file, base.as_path(), theirs.as_path()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok();
        let _ = std::fs::remove_dir_all(&scratch);
        merged.filter(|merged| merged.status.success()).map(|merged| merged.stdout)
    }

    /// The commits of a lane change nothing the main lane does not have
    /// already (the same change was made here, or every conflict was
    /// decided for the main lane). The two histories are joined all the
    /// same, or the remote would refuse every push from now on: a
    /// checkpoint with no change of its own, exported as a merge with the
    /// last of the commits. Returns it.
    pub fn pull_joined(&mut self, lane: LaneId) -> Result<String> {
        // What the lane brought is remembered next to it: the lane itself
        // forgot when it was brought up to the main lane.
        let tip = self.pull_waiting().filter(|(_, waiting)| *waiting == lane).and_then(|(tip, _)| oid(&tip));
        if tip.is_none() {
            return Err(Error::Other(format!("{lane} brings no commits made with git")));
        }
        let head = self.main.head.ok_or(Error::NothingToCheckpoint)?;
        let checkpoint = kito_core::Checkpoint {
            id: self.store.next_checkpoint_id(),
            head,
            label: "commits made with git: nothing to change here".to_owned(),
            created_by: self.main.author.clone(),
            verification: None,
            git_commit: None,
            approved_by: None,
            git_merge: tip,
        };
        self.store.put_checkpoint(&checkpoint)?;
        self.store.commit()?;
        if self.config.git.export_on_checkpoint {
            self.export_git()?;
        }
        self.pull_done(lane)?;
        Ok(checkpoint.id.to_string())
    }

    /// The commits of a lane are on the main lane: the lane is closed and
    /// nothing waits any more.
    pub fn pull_done(&mut self, lane: LaneId) -> Result<()> {
        if self.lanes.contains_key(&lane) {
            self.close_lane(lane)?;
        }
        let _ = std::fs::remove_file(self.waiting_file());
        Ok(())
    }
}

/// The files that differ between two commits.
fn changed_between(root: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let changed = git(root, &["diff", "--name-only", "--no-renames", "-z", from, to])?;
    Ok(changed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn does_not_apply(short: &str, subject: &str, path: &str) -> Error {
    Error::Other(format!(
        "commit {short} (\"{subject}\") changes {path} in a way that does not apply to what this repository \
         had when the commits were made: the commits of the remote were not brought in. Merge them by hand \
         with git on a clone, and push the result"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_id_is_forty_hexadecimal_digits() {
        let id = oid("00112233445566778899aabbccddeeff00112233").unwrap();
        assert_eq!((id[0], id[1], id[19]), (0x00, 0x11, 0x33));
        assert_eq!(hex(&id), "00112233445566778899aabbccddeeff00112233");
        assert!(oid("0011").is_none());
        assert!(oid(&"zz".repeat(20)).is_none());
    }
}
