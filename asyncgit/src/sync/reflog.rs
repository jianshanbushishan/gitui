use super::{CommitId, RepoPath};
use crate::{error::Result, sync::repository::repo};
use scopetime::scope_time;

/// Reflog action type
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReflogAction {
	/// commit: created a new commit
	Commit,
	/// checkout: moving from X to Y
	Checkout {
		/// source branch/commit
		from: Option<String>,
		/// target branch/commit
		to: Option<String>,
	},
	/// reset: moving to X
	Reset {
		/// target
		to: Option<String>,
	},
	/// rebase: (various phases)
	Rebase,
	/// merge: created a merge commit
	Merge,
	/// pull: fetched and merged/rebased
	Pull,
	/// clone: cloned repository
	Clone,
	/// branch: created/deleted branch
	Branch,
	/// amend: amended a commit
	Amend,
	/// cherry-pick: cherry-picked a commit
	CherryPick,
	/// Unknown action
	Unknown,
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
	s.as_bytes()
		.get(..prefix.len())
		.is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

impl ReflogAction {
	/// Parse action from reflog message
	pub fn from_message(message: &str) -> Self {
		// Handle empty message
		if message.is_empty() {
			return Self::Unknown;
		}

		if starts_with_ignore_ascii_case(message, "commit:") {
			Self::Commit
		} else if starts_with_ignore_ascii_case(message, "checkout:") {
			// Parse "checkout: moving from X to Y"
			let rest =
				message.strip_prefix("checkout:").unwrap_or(message);
			let rest = rest.trim();

			let (from, to) = rest.strip_prefix("moving from").map_or_else(
				// Just "checkout: X" format
				|| (None, Some(rest.to_string())),
				|rest| {
					rest.split_once(" to ").map_or(
						(None, None),
						|(before, after)| {
							if after.contains(" to ") {
								(None, None)
							} else {
								(
									Some(before.trim().to_string()),
									Some(after.trim().to_string()),
								)
							}
						},
					)
				},
			);

			Self::Checkout { from, to }
		} else if starts_with_ignore_ascii_case(message, "reset:") {
			// Parse "reset: moving to X"
			let rest =
				message.strip_prefix("reset:").unwrap_or(message);
			let rest = rest.trim();

			let to = if rest.starts_with("moving to") {
				rest.strip_prefix("moving to")
					.map(|s| s.trim().to_string())
			} else {
				Some(rest.to_string())
			};

			Self::Reset { to }
		} else if starts_with_ignore_ascii_case(message, "rebase") {
			Self::Rebase
		} else if starts_with_ignore_ascii_case(message, "merge:") {
			Self::Merge
		} else if starts_with_ignore_ascii_case(message, "pull") {
			Self::Pull
		} else if starts_with_ignore_ascii_case(message, "clone:") {
			Self::Clone
		} else if starts_with_ignore_ascii_case(message, "branch:") {
			Self::Branch
		} else if starts_with_ignore_ascii_case(message, "amend:") {
			Self::Amend
		} else if starts_with_ignore_ascii_case(
			message,
			"cherry-pick:",
		) {
			Self::CherryPick
		} else {
			Self::Unknown
		}
	}

	/// Get short display name for the action
	pub const fn short_name(&self) -> &'static str {
		match self {
			Self::Commit => "commit",
			Self::Checkout { .. } => "checkout",
			Self::Reset { .. } => "reset",
			Self::Rebase => "rebase",
			Self::Merge => "merge",
			Self::Pull => "pull",
			Self::Clone => "clone",
			Self::Branch => "branch",
			Self::Amend => "amend",
			Self::CherryPick => "cherry-pick",
			Self::Unknown => "other",
		}
	}

	/// Get the change description (from -> to for checkout, to for reset)
	pub fn change_description(&self) -> Option<String> {
		match self {
			Self::Checkout { from, to } => match (from, to) {
				(Some(f), Some(t)) => Some(format!("{f} -> {t}")),
				(Some(f), None) => Some(format!("{f} -> ?")),
				(None, Some(t)) => Some(format!("-> {t}")),
				(None, None) => None,
			},
			Self::Reset { to } => to.clone(),
			_ => None,
		}
	}
}

///
#[derive(Clone, Debug)]
pub struct ReflogEntry {
	/// ref name (e.g., "HEAD", "refs/heads/master", "HEAD -> origin/main")
	pub ref_name: String,
	/// old commit id (None for new refs)
	pub old_oid: Option<CommitId>,
	/// new commit id
	pub new_oid: CommitId,
	/// reflog message (the action, e.g., "commit: ...")
	pub message: String,
	/// parsed action type
	pub action: ReflogAction,
	/// timestamp (seconds since epoch)
	pub timestamp: i64,
	/// committer name
	pub committer: String,
	/// commit subject (the commit message's first line)
	pub commit_subject: String,
}

///
pub fn get_reflog(repo_path: &RepoPath) -> Result<Vec<ReflogEntry>> {
	scope_time!("get_reflog");

	let repo = repo(repo_path)?;
	let mut entries = Vec::new();

	// Get HEAD reflog using git2 native API
	if let Ok(reflog) = repo.reflog("HEAD") {
		entries.reserve(reflog.len());
		let mut subject_cache: std::collections::HashMap<
			git2::Oid,
			String,
		> = std::collections::HashMap::new();

		for idx in 0..reflog.len() {
			if let Some(entry) = reflog.get(idx) {
				let raw_new_oid = entry.id_new();
				let new_oid = CommitId::new(raw_new_oid);
				let old_oid = if entry.id_old().is_zero() {
					None
				} else {
					Some(CommitId::new(entry.id_old()))
				};

				let message = entry
					.message()
					.ok()
					.flatten()
					.unwrap_or_default();
				let action = ReflogAction::from_message(message);

				// Get committer info
				let committer = entry.committer();
				let committer_name = committer
					.name()
					.map(String::from)
					.ok()
					.unwrap_or_default();

				// Get commit subject by looking up the commit (memoized by OID)
				let commit_subject = subject_cache
					.entry(raw_new_oid)
					.or_insert_with(|| {
						repo.find_commit(raw_new_oid)
							.ok()
							.and_then(|c| {
								c.summary()
									.ok()
									.flatten()
									.map(String::from)
							})
							.unwrap_or_default()
					})
					.clone();

				entries.push(ReflogEntry {
					ref_name: "HEAD".to_string(),
					old_oid,
					new_oid,
					message: message.to_string(),
					action,
					timestamp: committer.when().seconds(),
					committer: committer_name,
					commit_subject,
				});
			}
		}
	}

	Ok(entries)
}
