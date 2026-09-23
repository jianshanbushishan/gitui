use crate::{
	error::{Error, Result},
	progress::ProgressPercent,
	sync::{
		branch::branch_set_upstream_after_push,
		config::{
			push_default_strategy_config_repo,
			PushDefaultStrategyConfig,
		},
		cred::BasicAuthCredential,
		get_branch_upstream_merge,
		remotes::{proxy_auto, Callbacks},
		repository::repo,
		CommitId, RepoPath,
	},
};
use crossbeam_channel::Sender;
use git2::{
	Direction, PackBuilderStage, PushOptions, Remote, Repository,
};
use scopetime::scope_time;
use std::fmt::Write as _;

///
pub trait AsyncProgress: Clone + Send + Sync {
	///
	fn is_done(&self) -> bool;
	///
	fn progress(&self) -> ProgressPercent;
}

///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressNotification {
	/// A new phase of a fetch across all remotes.
	FetchPhase {
		remote: String,
		current: usize,
		total: usize,
		phase: FetchPhase,
	},
	///
	UpdateTips {
		///
		name: String,
		///
		a: CommitId,
		///
		b: CommitId,
	},
	///
	Transfer {
		///
		objects: usize,
		///
		total_objects: usize,
		///
		indexed_objects: usize,
	},
	///
	PushTransfer {
		///
		current: usize,
		///
		total: usize,
		///
		bytes: usize,
	},
	///
	Packing {
		///
		stage: PackBuilderStage,
		///
		total: usize,
		///
		current: usize,
	},
	///
	Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPhase {
	Branches,
	Tags,
	Done,
}

impl AsyncProgress for ProgressNotification {
	fn is_done(&self) -> bool {
		*self == Self::Done
	}
	fn progress(&self) -> ProgressPercent {
		match *self {
			Self::Packing {
				stage,
				current,
				total,
			} => match stage {
				PackBuilderStage::AddingObjects
				| PackBuilderStage::Deltafication => {
					ProgressPercent::new(current, total)
				}
			},
			Self::PushTransfer { current, total, .. } => {
				ProgressPercent::new(current, total)
			}
			Self::Transfer {
				objects,
				total_objects,
				..
			} => ProgressPercent::new(objects, total_objects),
			_ => ProgressPercent::full(),
		}
	}
}

///
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum PushType {
	///
	#[default]
	Branch,
	///
	Tag,
}

/// Push on the same connection that advertised the destination ref. The
/// server checks the advertised old OID when applying the update, so a
/// concurrent remote change after the advertisement rejects the push.
fn push_with_checked_lease(
	repo: &Repository,
	remote: &mut Remote<'_>,
	remote_name: &str,
	destination: &str,
	push_ref: &str,
	options: &mut PushOptions<'_>,
	basic_credential: Option<BasicAuthCredential>,
) -> Result<()> {
	let refuse = |reason: &str| {
		Error::Generic(format!(
			"force push to '{destination}' refused: {reason}"
		))
	};

	// A push URL may point at another repository, whose history the fetch
	// tracking ref does not represent.
	if let Some(push_url) = remote.pushurl()? {
		if push_url != remote.url()? {
			return Err(refuse("push URL differs from fetch URL; cannot verify the remote branch"));
		}
	}

	let mut tracking_ref = None;
	for refspec in remote.refspecs() {
		if refspec.direction() != Direction::Fetch
			|| !refspec.src_matches(destination)
		{
			continue;
		}
		let mapped =
			refspec.transform(destination)?.as_str()?.to_owned();
		if tracking_ref.as_ref().is_some_and(|old| old != &mapped) {
			return Err(refuse("ambiguous remote-tracking refs; check fetch refspecs"));
		}
		tracking_ref = Some(mapped);
	}
	let tracking_ref = tracking_ref.ok_or_else(|| {
		refuse("no matching fetch refspec for the destination branch")
	})?;
	let expected = repo
		.find_reference(&tracking_ref)
		.ok()
		.and_then(|reference| reference.resolve().ok()?.target())
		.ok_or_else(|| refuse(&format!("remote-tracking ref is missing; fetch '{remote_name}' and retry")))?;

	let callbacks = Callbacks::new(None, basic_credential);
	let mut connection = remote.connect_auth(
		Direction::Push,
		Some(callbacks.callbacks()),
		Some(proxy_auto()),
	)?;
	let actual = connection
		.list()?
		.iter()
		.find(|head| head.name() == destination)
		.map(git2::RemoteHead::oid);
	if actual != Some(expected) {
		return Err(refuse(&format!("remote branch changed since last fetch; fetch '{remote_name}' and retry")));
	}
	connection.remote().push(&[push_ref], Some(options))?;
	Ok(())
}

#[cfg(test)]
pub fn push_branch(
	repo_path: &RepoPath,
	remote: &str,
	branch: &str,
	force: bool,
	delete: bool,
	basic_credential: Option<BasicAuthCredential>,
	progress_sender: Option<Sender<ProgressNotification>>,
) -> Result<()> {
	push_raw(
		repo_path,
		remote,
		branch,
		PushType::Branch,
		force,
		delete,
		basic_credential,
		progress_sender,
	)
}

//TODO: cleanup
#[allow(clippy::too_many_arguments)]
pub fn push_raw(
	repo_path: &RepoPath,
	remote: &str,
	branch: &str,
	ref_type: PushType,
	force: bool,
	delete: bool,
	basic_credential: Option<BasicAuthCredential>,
	progress_sender: Option<Sender<ProgressNotification>>,
) -> Result<()> {
	scope_time!("push");

	let repo = repo(repo_path)?;
	let mut remote_handle = repo.find_remote(remote)?;

	let push_default_strategy =
		push_default_strategy_config_repo(&repo)?;

	let mut options = PushOptions::new();
	options.proxy_options(proxy_auto());

	let callbacks =
		Callbacks::new(progress_sender, basic_credential.clone());
	options.remote_callbacks(callbacks.callbacks());
	options.packbuilder_parallelism(0);

	let branch_modifier = match (force, delete) {
		(true, true) => "+:",
		(false, true) => ":",
		(true, false) => "+",
		(false, false) => "",
	};
	let git_ref_type = match ref_type {
		PushType::Branch => "heads",
		PushType::Tag => "tags",
	};

	let mut push_ref =
		format!("{branch_modifier}refs/{git_ref_type}/{branch}");

	if !delete
		&& ref_type == PushType::Branch
		&& push_default_strategy
			== PushDefaultStrategyConfig::Upstream
	{
		if let Ok(Some(branch_upstream_merge)) =
			get_branch_upstream_merge(repo_path, branch)
		{
			let _ = write!(push_ref, ":{branch_upstream_merge}");
		}
	}

	log::debug!("push to: {push_ref}");
	if force && !delete && ref_type == PushType::Branch {
		let destination = push_ref.split_once(':').map_or_else(
			|| format!("refs/heads/{branch}"),
			|(_, dst)| dst.to_owned(),
		);
		push_with_checked_lease(
			&repo,
			&mut remote_handle,
			remote,
			&destination,
			&push_ref,
			&mut options,
			basic_credential.clone(),
		)?;
	} else {
		remote_handle.push(&[push_ref], Some(&mut options))?;
	}

	if let Some((reference, msg)) =
		callbacks.get_stats()?.push_rejected_msg
	{
		return Err(Error::Generic(format!(
			"push to '{reference}' rejected: {msg}"
		)));
	}

	if !delete {
		branch_set_upstream_after_push(&repo, branch)?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::{
		self,
		tests::{
			get_commit_ids, repo_clone, repo_init, repo_init_bare,
			write_commit_file,
		},
	};
	use git2::Repository;
	use std::{fs::File, io::Write, path::Path};

	#[test]
	fn test_force_push() {
		// This test mimics the scenario of 2 people having 2
		// local branches and both modifying the same file then
		// both pushing, sequentially
		let (tmp_repo_dir, repo) = repo_init().unwrap();
		let (tmp_other_repo_dir, other_repo) = repo_init().unwrap();
		let (tmp_upstream_dir, _) = repo_init_bare().unwrap();

		repo.remote(
			"origin",
			tmp_upstream_dir.path().to_str().unwrap(),
		)
		.unwrap();

		other_repo
			.remote(
				"origin",
				tmp_upstream_dir.path().to_str().unwrap(),
			)
			.unwrap();

		let tmp_repo_file_path =
			tmp_repo_dir.path().join("temp_file.txt");
		let mut tmp_repo_file =
			File::create(tmp_repo_file_path).unwrap();
		writeln!(tmp_repo_file, "TempSomething").unwrap();

		sync::commit(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"repo_1_commit",
		)
		.unwrap();

		push_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			false,
			false,
			None,
			None,
		)
		.unwrap();
		other_repo
			.find_remote("origin")
			.unwrap()
			.fetch(&[] as &[&str], None, None)
			.unwrap();

		let tmp_other_repo_file_path =
			tmp_other_repo_dir.path().join("temp_file.txt");
		let mut tmp_other_repo_file =
			File::create(tmp_other_repo_file_path).unwrap();
		writeln!(tmp_other_repo_file, "TempElse").unwrap();

		sync::commit(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"repo_2_commit",
		)
		.unwrap();

		// Attempt a normal push,
		// should fail as branches diverged
		assert!(push_branch(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			false,
			false,
			None,
			None,
		)
		.is_err());

		// Attempt force push,
		// should work as it forces the push through
		assert!(push_branch(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.is_ok());
	}

	#[test]
	fn test_force_push_rewrites_history() {
		// This test mimics the scenario of 2 people having 2
		// local branches and both modifying the same file then
		// both pushing, sequentially

		let (tmp_repo_dir, repo) = repo_init().unwrap();
		let (tmp_other_repo_dir, other_repo) = repo_init().unwrap();
		let (tmp_upstream_dir, upstream) = repo_init_bare().unwrap();

		repo.remote(
			"origin",
			tmp_upstream_dir.path().to_str().unwrap(),
		)
		.unwrap();

		other_repo
			.remote(
				"origin",
				tmp_upstream_dir.path().to_str().unwrap(),
			)
			.unwrap();

		let tmp_repo_file_path =
			tmp_repo_dir.path().join("temp_file.txt");
		let mut tmp_repo_file =
			File::create(tmp_repo_file_path).unwrap();
		writeln!(tmp_repo_file, "TempSomething").unwrap();

		sync::stage_add_file(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			Path::new("temp_file.txt"),
		)
		.unwrap();

		let repo_1_commit = sync::commit(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"repo_1_commit",
		)
		.unwrap();

		//NOTE: make sure the commit actually contains that file
		assert_eq!(
			sync::get_commit_files(
				&tmp_repo_dir.path().to_str().unwrap().into(),
				repo_1_commit,
				None
			)
			.unwrap()[0]
				.path,
			String::from("temp_file.txt")
		);

		let commits = get_commit_ids(&repo, 1);
		assert!(commits.contains(&repo_1_commit));

		push_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			false,
			false,
			None,
			None,
		)
		.unwrap();
		other_repo
			.find_remote("origin")
			.unwrap()
			.fetch(&[] as &[&str], None, None)
			.unwrap();

		let tmp_other_repo_file_path =
			tmp_other_repo_dir.path().join("temp_file.txt");
		let mut tmp_other_repo_file =
			File::create(tmp_other_repo_file_path).unwrap();
		writeln!(tmp_other_repo_file, "TempElse").unwrap();

		sync::stage_add_file(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			Path::new("temp_file.txt"),
		)
		.unwrap();

		let repo_2_commit = sync::commit(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"repo_2_commit",
		)
		.unwrap();

		let repo_2_parent = other_repo
			.find_commit(repo_2_commit.into())
			.unwrap()
			.parents()
			.next()
			.unwrap()
			.id();

		let commits = get_commit_ids(&other_repo, 1);
		assert!(commits.contains(&repo_2_commit));

		// Attempt a normal push,
		// should fail as branches diverged
		assert!(push_branch(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			false,
			false,
			None,
			None,
		)
		.is_err());

		// Check that the other commit is not in upstream,
		// a normal push would not rewrite history
		let commits = get_commit_ids(&upstream, 1);
		assert!(!commits.contains(&repo_2_commit));

		// Attempt force push,
		// should work as it forces the push through

		push_branch(
			&tmp_other_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.unwrap();

		let commits = get_commit_ids(&upstream, 1);
		assert!(commits.contains(&repo_2_commit));

		let new_upstream_parent =
			Repository::init_bare(tmp_upstream_dir.path())
				.unwrap()
				.find_commit(repo_2_commit.into())
				.unwrap()
				.parents()
				.next()
				.unwrap()
				.id();
		assert_eq!(new_upstream_parent, repo_2_parent,);
	}

	#[test]
	fn test_force_push_requires_current_tracking_ref() {
		let (first_dir, first_repo) = repo_init().unwrap();
		let (second_dir, second_repo) = repo_init().unwrap();
		let (remote_dir, remote_repo) = repo_init_bare().unwrap();
		let remote_path = remote_dir.path().to_str().unwrap();
		first_repo.remote("origin", remote_path).unwrap();
		second_repo.remote("origin", remote_path).unwrap();
		let first_path = &first_dir.path().to_str().unwrap().into();
		let second_path = &second_dir.path().to_str().unwrap().into();

		let initial = write_commit_file(
			&first_repo,
			"first.txt",
			"initial",
			"initial",
		);
		push_branch(
			first_path, "origin", "master", false, false, None, None,
		)
		.unwrap();
		write_commit_file(
			&second_repo,
			"second.txt",
			"second",
			"second",
		);

		// A local branch without a fetched view of the remote cannot lease it.
		let missing = push_branch(
			second_path,
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.unwrap_err()
		.to_string();
		assert!(missing.contains("remote-tracking ref is missing"));
		assert_eq!(
			remote_repo.refname_to_id("refs/heads/master").unwrap(),
			initial.into()
		);

		second_repo
			.find_remote("origin")
			.unwrap()
			.fetch(&[] as &[&str], None, None)
			.unwrap();
		let newer = write_commit_file(
			&first_repo,
			"first.txt",
			"updated",
			"updated",
		);
		push_branch(
			first_path, "origin", "master", false, false, None, None,
		)
		.unwrap();

		let stale = push_branch(
			second_path,
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.unwrap_err()
		.to_string();
		assert!(
			stale.contains("remote branch changed since last fetch")
		);
		assert_eq!(
			remote_repo.refname_to_id("refs/heads/master").unwrap(),
			newer.into()
		);

		// Once the user has fetched the new remote state, rewriting it is
		// an informed choice and the lease passes.
		second_repo
			.find_remote("origin")
			.unwrap()
			.fetch(&[] as &[&str], None, None)
			.unwrap();
		push_branch(
			second_path,
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.unwrap();
		assert_eq!(
			remote_repo.refname_to_id("refs/heads/master").unwrap(),
			second_repo.refname_to_id("refs/heads/master").unwrap()
		);
	}

	#[test]
	fn test_force_push_leases_upstream_destination() {
		let (local_dir, local_repo) = repo_init().unwrap();
		let (remote_dir, remote_repo) = repo_init_bare().unwrap();
		local_repo
			.remote("origin", remote_dir.path().to_str().unwrap())
			.unwrap();
		let pushed = write_commit_file(
			&local_repo,
			"file.txt",
			"first",
			"first",
		);
		local_repo
			.find_remote("origin")
			.unwrap()
			.push(&["refs/heads/master:refs/heads/review"], None)
			.unwrap();
		let mut config = local_repo.config().unwrap();
		config.set_str("push.default", "upstream").unwrap();
		config.set_str("branch.master.remote", "origin").unwrap();
		config
			.set_str("branch.master.merge", "refs/heads/review")
			.unwrap();
		local_repo
			.find_remote("origin")
			.unwrap()
			.fetch(&[] as &[&str], None, None)
			.unwrap();
		assert_eq!(
			local_repo
				.refname_to_id("refs/remotes/origin/review")
				.unwrap(),
			pushed.into()
		);
		let rewritten = write_commit_file(
			&local_repo,
			"file.txt",
			"rewrite",
			"rewrite",
		);
		push_branch(
			&local_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			true,
			false,
			None,
			None,
		)
		.unwrap();
		assert_eq!(
			remote_repo.refname_to_id("refs/heads/review").unwrap(),
			rewritten.into()
		);
		assert!(remote_repo
			.refname_to_id("refs/heads/master")
			.is_err());
	}

	#[test]
	fn test_delete_remote_branch() {
		// This test mimics the scenario of a user creating a branch, push it, and then remove it on the remote

		let (upstream_dir, upstream_repo) = repo_init_bare().unwrap();

		let (tmp_repo_dir, repo) =
			repo_clone(upstream_dir.path().to_str().unwrap())
				.unwrap();

		// You need a commit before being able to branch !
		let commit_1 = write_commit_file(
			&repo,
			"temp_file.txt",
			"SomeContent",
			"Initial commit",
		);

		let commits = get_commit_ids(&repo, 1);
		assert!(commits.contains(&commit_1));

		push_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"master",
			false,
			false,
			None,
			None,
		)
		.unwrap();

		// Create the local branch
		sync::create_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"test_branch",
		)
		.unwrap();

		// Push the local branch
		push_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"test_branch",
			false,
			false,
			None,
			None,
		)
		.unwrap();

		// Test if the branch exits on the remote
		assert!(upstream_repo
			.branches(None)
			.unwrap()
			.map(std::result::Result::unwrap)
			.map(|(i, _)| i.name().unwrap().unwrap().to_string())
			.any(|i| &i == "test_branch"));

		// Delete the remote branch
		assert!(push_branch(
			&tmp_repo_dir.path().to_str().unwrap().into(),
			"origin",
			"test_branch",
			false,
			true,
			None,
			None,
		)
		.is_ok());

		// Test that the branch has be remove from the remote
		assert!(!upstream_repo
			.branches(None)
			.unwrap()
			.map(std::result::Result::unwrap)
			.map(|(i, _)| i.name().unwrap().unwrap().to_string())
			.any(|i| &i == "test_branch"));
	}
}
