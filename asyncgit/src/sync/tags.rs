use super::{commits_info::gix_get_message, CommitId, RepoPath};
use crate::{
	error::Result,
	sync::{gix_repo, repository::repo},
};
use scopetime::scope_time;
use std::collections::{BTreeMap, HashMap};

///
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Tag {
	/// tag name
	pub name: String,
	/// tag annotation
	pub annotation: Option<String>,
}

impl Tag {
	///
	pub fn new(name: &str) -> Self {
		Self {
			name: name.into(),
			annotation: None,
		}
	}
}

/// all tags pointing to a single commit
pub type CommitTags = Vec<Tag>;
/// hashmap of tag target commit hash to tag names
pub type Tags = BTreeMap<CommitId, CommitTags>;

///
pub struct TagWithMetadata {
	///
	pub name: String,
	///
	pub author: String,
	///
	pub time: i64,
	///
	pub message: String,
	///
	pub commit_id: CommitId,
	///
	pub annotation: Option<String>,
}

static MAX_MESSAGE_WIDTH: usize = 100;

fn peel_tag_reference(
	mut reference: gix::Reference<'_>,
) -> Option<(gix::Commit<'_>, String, Option<String>)> {
	let target_id = reference.follow_to_object().ok()?;
	let object = target_id.object().ok()?;
	match object.kind {
		gix::object::Kind::Tag => {
			let (name, annotation) =
				object.try_to_tag_ref().map_or_else(
					|_| (reference.name().shorten().to_string(), None),
					|decoded| {
						(
							decoded.name.to_string(),
							Some(decoded.message.to_string()),
						)
					},
				);
			let commit = object
				.peel_to_kind(gix::object::Kind::Commit)
				.ok()?
				.into_commit();
			Some((commit, name, annotation))
		}
		gix::object::Kind::Commit => {
			let name = reference.name().shorten().to_string();
			Some((object.into_commit(), name, None))
		}
		_ => None,
	}
}

/// returns `Tags` type filled with all tags found in repo
pub fn get_tags(repo_path: &RepoPath) -> Result<Tags> {
	scope_time!("get_tags");

	let mut res = Tags::new();

	let mut repo: gix::Repository = gix_repo(repo_path)?;
	repo.object_cache_size_if_unset(2_usize.pow(14));
	let platform = repo.references()?;
	for reference in (platform.tags()?).flatten() {
		if let Some((commit, name, annotation)) =
			peel_tag_reference(reference)
		{
			res.entry(commit.into())
				.or_default()
				.push(Tag { name, annotation });
		}
	}

	Ok(res)
}

///
pub fn get_tags_with_metadata(
	repo_path: &RepoPath,
) -> Result<Vec<TagWithMetadata>> {
	scope_time!("get_tags_with_metadata");

	let mut repo: gix::Repository = gix_repo(repo_path)?;
	repo.object_cache_size_if_unset(2_usize.pow(14));
	let mailmap = repo.open_mailmap();
	let platform = repo.references()?;

	let mut commit_cache: HashMap<CommitId, (String, i64, String)> =
		HashMap::new();
	let mut tags: Vec<TagWithMetadata> = Vec::new();

	for reference in (platform.tags()?).flatten() {
		if let Some((commit, name, annotation)) =
			peel_tag_reference(reference)
		{
			let commit_id: CommitId = commit.id.into();
			let (author, time, message) =
				if let Some(cached) = commit_cache.get(&commit_id) {
					cached.clone()
				} else if let Ok(commit_ref) = commit.decode() {
					let author = commit_ref.author().map_or_else(
						|_| String::from("<unknown>"),
						|sig| {
							mailmap
								.resolve_cow(sig)
								.name
								.to_string()
						},
					);
					let time =
						commit_ref.time().map_or(0, |t| t.seconds);
					let message = gix_get_message(
						&commit_ref,
						Some(MAX_MESSAGE_WIDTH),
					);
					let entry = (author, time, message);
					commit_cache.insert(commit_id, entry.clone());
					entry
				} else {
					continue;
				};

			tags.push(TagWithMetadata {
				name,
				author,
				time,
				message,
				commit_id,
				annotation,
			});
		}
	}

	tags.sort_unstable_by_key(|b| std::cmp::Reverse(b.time));

	Ok(tags)
}

///
pub fn delete_tag(
	repo_path: &RepoPath,
	tag_name: &str,
) -> Result<()> {
	scope_time!("delete_tag");

	let repo = repo(repo_path)?;
	repo.tag_delete(tag_name)?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::tests::repo_init;
	use git2::ObjectType;

	#[test]
	fn test_smoke() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert!(get_tags(repo_path).unwrap().is_empty());
	}

	#[test]
	fn test_multitags() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		let sig = repo.signature().unwrap();
		let head_id = repo.head().unwrap().target().unwrap();
		let target = repo
			.find_object(
				repo.head().unwrap().target().unwrap(),
				Some(ObjectType::Commit),
			)
			.unwrap();

		repo.tag("a", &target, &sig, "", false).unwrap();
		repo.tag("b", &target, &sig, "", false).unwrap();

		assert_eq!(
			get_tags(repo_path).unwrap()[&CommitId::new(head_id)]
				.iter()
				.map(|t| &t.name)
				.collect::<Vec<_>>(),
			vec!["a", "b"]
		);

		let tags = get_tags_with_metadata(repo_path).unwrap();

		assert_eq!(tags.len(), 2);
		assert_eq!(tags[0].name, "a");
		assert_eq!(tags[0].message, "initial");
		assert_eq!(tags[1].name, "b");
		assert_eq!(tags[1].message, "initial");
		assert_eq!(tags[0].commit_id, tags[1].commit_id);

		delete_tag(repo_path, "a").unwrap();

		let tags = get_tags(repo_path).unwrap();

		assert_eq!(tags.len(), 1);

		delete_tag(repo_path, "b").unwrap();

		let tags = get_tags(repo_path).unwrap();

		assert_eq!(tags.len(), 0);
	}
}
