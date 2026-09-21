//! Opt-in regression probe for status scans against a real, very large
//! repository. It is ignored by default because CI does not have such a repo.

use asyncgit::sync::{
	status::get_status_split, RepoPath, ShowUntrackedFilesConfig,
};
use std::time::{Duration, Instant};

#[test]
#[ignore = "set GITUI_LARGE_REPO to an existing large repository"]
fn large_repository_status_finishes_without_mapping_the_whole_pack() {
	let path = std::env::var_os("GITUI_LARGE_REPO")
		.expect("GITUI_LARGE_REPO must point to a large repository");
	let repo: RepoPath = std::path::PathBuf::from(path).into();

	let started = Instant::now();
	let (staged, workdir) =
		get_status_split(&repo, Some(ShowUntrackedFilesConfig::All))
			.expect("the large-repository status scan must succeed");
	let elapsed = started.elapsed();

	eprintln!(
		"large repository status: {} staged, {} worktree, {elapsed:?}",
		staged.len(),
		workdir.len()
	);
	assert!(
		elapsed < Duration::from_secs(30),
		"status scan exceeded the 30 second smoke-test limit: {elapsed:?}"
	);
}
