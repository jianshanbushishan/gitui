use crate::{
	error::Result,
	sync::{self, branch::get_branch_name_repo, RepoPathRef},
};
use sync::Head;

///
pub struct BranchName {
	last_result: Option<(Head, String)>,
	repo: RepoPathRef,
}

impl BranchName {
	///
	pub const fn new(repo: RepoPathRef) -> Self {
		Self {
			repo,
			last_result: None,
		}
	}

	///
	pub fn lookup(&mut self) -> Result<String> {
		let repo = sync::repo(&self.repo.borrow())?;
		let head = repo.head()?;
		let name = sync::utils::bytes2string(head.name_bytes())?;
		let id = head.peel_to_commit()?.id().into();
		let current_head = Head { name, id };

		if let Some((last_head, branch_name)) =
			self.last_result.as_ref()
		{
			if *last_head == current_head {
				return Ok(branch_name.clone());
			}
		}

		let branch_name = get_branch_name_repo(&repo)?;
		self.last_result = Some((current_head, branch_name.clone()));
		Ok(branch_name)
	}

	///
	pub fn last(&self) -> Option<String> {
		self.last_result.as_ref().map(|last| last.1.clone())
	}

	/// Borrow the cached branch name without allocating.
	pub fn last_str(&self) -> Option<&str> {
		self.last_result.as_ref().map(|last| last.1.as_str())
	}
}
