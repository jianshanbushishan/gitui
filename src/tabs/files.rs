use std::path::{Path, PathBuf};

use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState, RevisionFilesComponent,
	},
	keys::{key_match, SharedKeyConfig},
	AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, RepoPathRef},
	Error as GitError,
};
use crossterm::event::Event;

pub struct FilesTab {
	repo: RepoPathRef,
	key_config: SharedKeyConfig,
	visible: bool,
	files: RevisionFilesComponent,
}

impl FilesTab {
	///
	pub fn new(
		env: &Environment,
		select_file: Option<PathBuf>,
	) -> Self {
		Self {
			visible: false,
			files: RevisionFilesComponent::new(env, select_file),
			repo: env.repo.clone(),
			key_config: env.key_config.clone(),
		}
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			match sync::get_head(&self.repo.borrow()) {
				Ok(head) => self.files.set_commit(head)?,
				Err(GitError::NoHead) => self.files.set_no_commits(),
				Err(GitError::Git(error))
					if error.code()
						== git2::ErrorCode::UnbornBranch =>
				{
					self.files.set_no_commits();
				}
				Err(error) => {
					self.files.set_head_error(&error.to_string())
				}
			}
		}

		Ok(())
	}

	///
	pub fn anything_pending(&self) -> bool {
		self.files.any_work_pending()
	}

	///
	pub fn update_async(
		&mut self,
		ev: AsyncNotification,
	) -> Result<()> {
		if self.is_visible() {
			self.files.update(ev)?;
		}

		Ok(())
	}

	pub fn file_finder_update(&mut self, file: &Path) {
		self.files.find_file(file);
	}

	pub fn content_search_selected(
		&mut self,
		query: &str,
		line: usize,
		matching_lines: &[usize],
	) {
		self.files.content_search_selected(
			query,
			line,
			matching_lines,
		);
	}
}

impl DrawableComponent for FilesTab {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		if self.is_visible() {
			self.files.draw(f, rect)?;
		}
		Ok(())
	}
}

impl Component for FilesTab {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			return self.files.commands(out, force_all);
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.visible {
			if let Event::Key(key) = ev {
				if self.files.can_retry()
					&& self.files.revision().is_none()
					&& key_match(key, self.key_config.keys.enter)
				{
					self.update()?;
					return Ok(EventState::Consumed);
				}
			}
			return self.files.event(ev);
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.update()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unborn_head_shows_empty_state_and_can_retry_after_commit() {
		let (dir, repo) = git2_testing::repo_init_empty();
		let env = Environment::test_env();
		*env.repo.borrow_mut() = dir.path().to_str().unwrap().into();
		let mut tab = FilesTab::new(&env, None);
		tab.show().unwrap();

		assert!(tab.files.can_retry());
		assert!(tab.files.revision().is_none());

		let tree = repo.treebuilder(None).unwrap().write().unwrap();
		let tree = repo.find_tree(tree).unwrap();
		let signature = repo.signature().unwrap();
		let head = repo
			.commit(
				Some("HEAD"),
				&signature,
				&signature,
				"initial",
				&tree,
				&[],
			)
			.unwrap();

		let enter = (&env.key_config.keys.enter).into();
		assert!(tab.event(&Event::Key(enter)).unwrap().is_consumed());
		assert_eq!(
			tab.files.revision().map(|c| c.id),
			Some(head.into())
		);
	}
}
