//! Git LFS detection, pointer metadata and explicit object transfers.

use super::{git_cli, repo, CommitId, RepoPath};
use crate::error::{Error, Result};
use git2::{ObjectType, Repository, TreeWalkMode, TreeWalkResult};
use serde::Deserialize;
use std::{
	collections::{HashMap, HashSet},
	ffi::OsStr,
	fs,
	io::Read,
	path::{Component, Path, PathBuf},
	sync::{Mutex, OnceLock},
};

const POINTER_LIMIT: usize = 1024;
type TreeCache = HashMap<(PathBuf, git2::Oid), bool>;
static TREE_CACHE: OnceLock<Mutex<TreeCache>> = OnceLock::new();

/// Identity and size of the actual file represented by a Git LFS pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsPointer {
	/// SHA-256 of the actual file.
	pub oid: String,
	/// Declared size in bytes.
	pub size: u64,
}

/// Version to inspect for LFS metadata.
#[derive(Clone, Copy, Debug)]
pub enum LfsSource {
	/// Working file, falling back to its index pointer for identity.
	Worktree,
	/// Staged file.
	Index,
	/// File in a specific commit.
	Commit(CommitId),
}

/// Form of the selected file in the working tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LfsWorktree {
	/// There is no working file at this path.
	Missing,
	/// The working file contains a pointer.
	Pointer,
	/// The working file contains regular content.
	Content,
	/// No working-tree information is available.
	Unknown,
}

/// File metadata, separate from the Git modification state.
#[derive(Clone, Debug)]
pub struct LfsFileInfo {
	/// Pointer belonging to the selected version, if known.
	pub pointer: Option<LfsPointer>,
	/// Whether that pointer's object exists in the local LFS cache.
	pub local_object: Option<bool>,
	/// Form of the path in the working tree.
	pub worktree: LfsWorktree,
}

/// Installed client, effective filters and presence of a pre-push hook.
#[derive(Clone, Debug)]
pub struct LfsEnvironment {
	/// Version reported by Git LFS, when available.
	pub version: Option<String>,
	/// Effective clean and smudge filter setup.
	pub filters_ready: bool,
	/// Whether a pre-push hook is present. Custom hooks may chain to LFS.
	pub pre_push_hook: bool,
}

/// Parse a bounded Git LFS pointer without reading or downloading its object.
pub fn parse_pointer(bytes: &[u8]) -> Option<LfsPointer> {
	if bytes.len() >= POINTER_LIMIT {
		return None;
	}
	let text = std::str::from_utf8(bytes).ok()?;
	let mut lines = text.lines();
	if !matches!(
		lines.next()?,
		"version https://git-lfs.github.com/spec/v1"
			| "version https://hawser.github.com/spec/v1"
			| "version http://git-media.io/v/2"
	) {
		return None;
	}
	let mut oid = None;
	let mut size = None;
	for line in lines {
		if let Some(value) = line.strip_prefix("oid sha256:") {
			if oid.is_some() || !valid_oid(value) {
				return None;
			}
			oid = Some(value.to_owned());
		} else if let Some(value) = line.strip_prefix("size ") {
			if oid.is_none()
				|| size.is_some()
				|| !value.bytes().all(|b| b.is_ascii_digit())
			{
				return None;
			}
			size = u64::try_from(value.parse::<i64>().ok()?).ok();
		} else {
			let (key, value) =
				line.strip_prefix("ext-")?.split_once(" sha256:")?;
			let (priority, name) = key.split_once('-')?;
			if size.is_some()
				|| priority.parse::<u8>().ok()? > 9
				|| name.is_empty()
				|| !valid_oid(value)
			{
				return None;
			}
		}
	}
	Some(LfsPointer {
		oid: oid?,
		size: size?,
	})
}

fn valid_oid(value: &str) -> bool {
	value.len() == 64
		&& value
			.bytes()
			.all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn git(
	repo: &Repository,
	args: impl IntoIterator<Item = impl AsRef<OsStr>>,
	skip_smudge: bool,
) -> Result<Vec<u8>> {
	git_cli::checked(git_cli::output(repo, args, skip_smudge)?)
}

fn blob_pointer(
	repo: &Repository,
	id: git2::Oid,
) -> Option<LfsPointer> {
	let (size, kind) = repo.odb().ok()?.read_header(id).ok()?;
	if kind != ObjectType::Blob || size >= POINTER_LIMIT {
		return None;
	}
	parse_pointer(repo.find_blob(id).ok()?.content())
}

fn index_pointer(
	repo: &Repository,
	path: &Path,
) -> Option<LfsPointer> {
	blob_pointer(repo, repo.index().ok()?.get_path(path, 0)?.id)
}

fn tree_pointer(
	repo: &Repository,
	tree: &git2::Tree<'_>,
	path: &Path,
) -> Option<LfsPointer> {
	let entry = tree.get_path(path).ok()?;
	if !matches!(entry.filemode(), 0o100_644 | 0o100_755) {
		return None;
	}
	blob_pointer(repo, entry.id())
}

fn attributes_use_lfs(bytes: &[u8]) -> bool {
	bytes.split(|b| *b == b'\n').any(|line| {
		let mut fields = line
			.split(u8::is_ascii_whitespace)
			.filter(|s| !s.is_empty());
		fields.next().is_some_and(|first| !first.starts_with(b"#"))
			&& fields.any(|field| field == b"filter=lfs")
	})
}

fn attributes_file_uses_lfs(path: &Path) -> bool {
	fs::read(path).is_ok_and(|bytes| attributes_use_lfs(&bytes))
}

fn attributes_blob_uses_lfs(
	repo: &Repository,
	id: git2::Oid,
) -> bool {
	repo.find_blob(id)
		.is_ok_and(|blob| attributes_use_lfs(blob.content()))
}

/// Whether the current repository has LFS rules or repository-local LFS configuration.
pub fn is_repo(repo: &Repository) -> Result<bool> {
	let config = repo.config()?;
	if config.get_string("lfs.url").is_ok()
		|| config
			.get_path("core.attributesfile")
			.is_ok_and(|path| attributes_file_uses_lfs(&path))
		|| attributes_file_uses_lfs(
			&repo.commondir().join("info/attributes"),
		) || default_attributes_path()
		.is_some_and(|p| attributes_file_uses_lfs(&p))
	{
		return Ok(true);
	}
	let workdir = repo.workdir();
	if workdir.is_some_and(|p| {
		p.join(".lfsconfig").exists()
			|| attributes_file_uses_lfs(&p.join(".gitattributes"))
	}) {
		return Ok(true);
	}
	if !repo.is_bare() {
		for entry in repo.index()?.iter() {
			if entry.path == b".lfsconfig" {
				return Ok(true);
			}
			if (entry.path == b".gitattributes"
				|| entry.path.ends_with(b"/.gitattributes"))
				&& (attributes_blob_uses_lfs(repo, entry.id)
					|| workdir
						.zip(std::str::from_utf8(&entry.path).ok())
						.is_some_and(|(dir, path)| {
							attributes_file_uses_lfs(&dir.join(path))
						})) {
				return Ok(true);
			}
		}
	}
	if repo.head().is_ok() {
		return is_repo_at(repo, "HEAD");
	}
	Ok(false)
}

fn default_attributes_path() -> Option<PathBuf> {
	let config = std::env::var_os("XDG_CONFIG_HOME")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.or_else(|| {
			dirs::home_dir().map(|home| home.join(".config"))
		})?;
	Some(config.join("git/attributes"))
}

/// Whether a target commit contains LFS attributes or configuration.
pub fn is_repo_at(
	repo: &Repository,
	reference: &str,
) -> Result<bool> {
	let object = repo.revparse_single(reference)?;
	let peeled = object.peel(ObjectType::Any)?;
	let tree = match peeled.kind() {
		Some(ObjectType::Commit | ObjectType::Tree) => {
			peeled.peel_to_tree()?
		}
		Some(ObjectType::Blob) => {
			return Ok(blob_pointer(repo, peeled.id()).is_some())
		}
		_ => return Ok(false),
	};
	// Tree contents are immutable. Cache only this decision, never worktree
	// attributes or effective configuration which can change between scans.
	let cache = TREE_CACHE.get_or_init(Mutex::default);
	let key = (repo.commondir().to_path_buf(), tree.id());
	if let Some(found) = cache.lock()?.get(&key) {
		return Ok(*found);
	}
	let mut found = tree.get_name(".lfsconfig").is_some();
	if !found {
		let walk = tree.walk(TreeWalkMode::PreOrder, |_, entry| {
			if entry.name_bytes() == b".gitattributes"
				&& attributes_blob_uses_lfs(repo, entry.id())
			{
				found = true;
				TreeWalkResult::Abort
			} else {
				TreeWalkResult::Ok
			}
		});
		if !found {
			walk?;
		}
	}
	let mut cache = cache.lock()?;
	if cache.len() >= 64 {
		cache.clear();
	}
	cache.insert(key, found);
	Ok(found)
}

pub(crate) fn guard(
	repo: &Repository,
	operation: &str,
	target: Option<&str>,
) -> Result<()> {
	if is_repo(repo)?
		|| target
			.map_or(Ok(false), |target| is_repo_at(repo, target))?
	{
		return Err(Error::Generic(format!(
			"{operation} is not supported in Git LFS repositories yet. Use Git from the command line."
		)));
	}
	Ok(())
}

pub(crate) fn ensure_push_hook(
	repo: &Repository,
	refspec_sources: impl IntoIterator<Item = impl AsRef<str>>,
	remote_tips: &[git2::Oid],
) -> Result<()> {
	let refuse = || {
		Error::Generic(
        "Git LFS requires a pre-push hook. Initialize Git LFS for this repository or configure the existing hooks path.".into(),
    )
	};
	let mut walk = repo.revwalk()?;
	let mut has_sources = false;
	for source in refspec_sources {
		if !has_sources && is_repo(repo)? {
			return Err(refuse());
		}
		has_sources = true;
		if is_repo_at(repo, source.as_ref())? {
			return Err(refuse());
		}
		if let Ok(commit) =
			repo.revparse_single(source.as_ref())?.peel_to_commit()
		{
			walk.push(commit.id())?;
		}
	}
	for tip in remote_tips {
		// The advertised remote history may not be present locally yet.
		if let Ok(commit) = repo
			.find_object(*tip, None)
			.and_then(|o| o.peel_to_commit())
		{
			walk.hide(commit.id())?;
		}
	}
	// Tip attributes alone miss LFS objects added and removed in unpushed
	// history. Inspect only changed blobs, without loading large assets.
	let mut inspected = HashSet::new();
	for id in walk {
		let commit = repo.find_commit(id?)?;
		let tree = commit.tree()?;
		let parent = if commit.parent_count() == 0 {
			None
		} else {
			Some(commit.parent(0)?.tree()?)
		};
		let diff = repo.diff_tree_to_tree(
			parent.as_ref(),
			Some(&tree),
			None,
		)?;
		for delta in diff.deltas() {
			let id = delta.new_file().id();
			if !id.is_zero()
				&& inspected.insert(id)
				&& blob_pointer(repo, id).is_some()
			{
				return Err(refuse());
			}
		}
	}
	Ok(())
}

fn git_lfs_version(repo: &Repository) -> Option<String> {
	let output = git(repo, ["lfs", "version"], false).ok()?;
	Some(String::from_utf8_lossy(&output).trim().to_owned())
}

fn config_value(repo: &Repository, name: &str) -> Option<String> {
	let bytes = git(repo, ["config", "--get", name], false).ok()?;
	Some(String::from_utf8_lossy(&bytes).trim().to_owned())
}

fn filters_ready(repo: &Repository) -> bool {
	let required = git(
		repo,
		["config", "--bool", "--get", "filter.lfs.required"],
		false,
	)
	.is_ok_and(|value| value == b"true\n" || value == b"true\r\n");
	required
		&& (config_value(repo, "filter.lfs.process")
			.is_some_and(|s| !s.is_empty())
			|| (config_value(repo, "filter.lfs.clean")
				.is_some_and(|s| !s.is_empty())
				&& config_value(repo, "filter.lfs.smudge")
					.is_some_and(|s| !s.is_empty())))
}

pub(crate) fn require_ready(repo: &Repository) -> Result<()> {
	if git_lfs_version(repo).is_none() {
		return Err(Error::Generic("Git LFS is required for this operation. Install git-lfs and try again.".into()));
	}
	if !filters_ready(repo) {
		return Err(Error::Generic("Git LFS filters are not configured. Initialize Git LFS for this repository.".into()));
	}
	Ok(())
}

/// Inspect the Git LFS environment without changing configuration.
pub fn environment(repo_path: &RepoPath) -> Result<LfsEnvironment> {
	let repo = repo(repo_path)?;
	Ok(LfsEnvironment {
		version: git_lfs_version(&repo),
		filters_ready: filters_ready(&repo),
		pre_push_hook: git2_hooks::hook_available(
			&repo,
			None,
			git2_hooks::HOOK_PRE_PUSH,
		)?,
	})
}

/// Install local filters and the standard hook, preserving an existing custom hook.
pub fn initialize(repo_path: &RepoPath) -> Result<()> {
	let repo = repo(repo_path)?;
	let output = git(
		&repo,
		[
			"rev-parse",
			"--path-format=absolute",
			"--git-path",
			"hooks/pre-push",
		],
		false,
	)?;
	let hook = PathBuf::from(
		String::from_utf8(output)?.trim_end_matches(['\r', '\n']),
	);
	if !within_repo(&repo, &hook)? {
		return Err(Error::Generic(
			"The configured hooks directory is outside this repository. Integrate Git LFS into that hook from the command line.".into(),
		));
	}
	git(&repo, ["lfs", "install", "--local"], false)?;
	Ok(())
}

fn within_repo(repo: &Repository, path: &Path) -> Result<bool> {
	let common = repo.commondir().canonicalize()?;
	let worktree =
		repo.workdir().map(Path::canonicalize).transpose()?;
	for ancestor in path.ancestors() {
		match ancestor.canonicalize() {
			Ok(absolute) => {
				return Ok(absolute.starts_with(&common)
					|| worktree.as_ref().is_some_and(|root| {
						absolute.starts_with(root)
					}))
			}
			Err(error)
				if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
	}
	Ok(false)
}

fn checked_path(path: &Path) -> Result<&str> {
	if path.components().next().is_none()
		|| !path
			.components()
			.all(|part| matches!(part, Component::Normal(_)))
	{
		return Err(Error::Generic(
			"Expected a repository-relative Git LFS file path."
				.into(),
		));
	}
	path.to_str().ok_or_else(|| {
		Error::Generic("Git LFS file path is not UTF-8.".into())
	})
}

fn attributed_paths(
	repo: &Repository,
	paths: &[&str],
) -> Result<HashSet<String>> {
	let input = paths
		.iter()
		.flat_map(|path| path.bytes().chain([0]))
		.collect();
	let output = git_cli::with_input(
		repo,
		&["check-attr", "-z", "--stdin", "filter"],
		input,
	)?;
	let fields: Vec<_> = output
		.strip_suffix(&[0])
		.unwrap_or(&output)
		.split(|byte| *byte == 0)
		.collect();
	let mut tracked = HashSet::new();
	let (entries, remainder) = fields.as_chunks::<3>();
	if !remainder.is_empty() {
		return Err(Error::Generic(
			"Invalid response from git check-attr.".into(),
		));
	}
	for [path, _, value] in entries {
		if *value == b"lfs" {
			tracked.insert(String::from_utf8(path.to_vec())?);
		}
	}
	Ok(tracked)
}

pub(crate) fn is_path(
	repo: &Repository,
	path: &Path,
) -> Result<bool> {
	if index_pointer(repo, path).is_some() {
		return Ok(true);
	}
	// Avoid requiring an external Git for ordinary paths. Git provides the final
	// attribute result when repository or per-path configuration suggests LFS.
	let local_attr =
		repo.get_attr(
			path,
			"filter",
			git2::AttrCheckFlags::FILE_THEN_INDEX,
		)
		.ok()
		.flatten() == Some("lfs");
	if local_attr || is_repo(repo)? {
		let Some(path_str) = path.to_str() else {
			return Ok(local_attr);
		};
		return Ok(
			attributed_paths(repo, &[path_str])?.contains(path_str)
		);
	}
	Ok(false)
}

pub(crate) fn is_any_path(
	repo: &Repository,
	pattern: &str,
) -> Result<bool> {
	let mut command = git_cli::command(repo);
	let output = match command
		.args([
			"--literal-pathspecs",
			"ls-files",
			"-z",
			"--cached",
			"--others",
			"--exclude-standard",
			"--",
			pattern,
		])
		.output()
	{
		Ok(output) => git_cli::checked(output)?,
		Err(error)
			if error.kind() == std::io::ErrorKind::NotFound =>
		{
			return Ok(false)
		}
		Err(error) => return Err(error.into()),
	};
	let paths = output
		.split(|byte| *byte == 0)
		.filter(|path| !path.is_empty())
		.filter_map(|path| std::str::from_utf8(path).ok());
	// Most repositories have no LFS rules. Avoid spawning check-attr for
	// every ordinary bulk stage; Git remains authoritative for candidates.
	let candidates = paths
		.filter(|path| {
			repo.get_attr(
				Path::new(path),
				"filter",
				git2::AttrCheckFlags::FILE_THEN_INDEX,
			)
			.ok()
			.flatten() == Some("lfs")
		})
		.collect::<Vec<_>>();
	Ok(!candidates.is_empty()
		&& !attributed_paths(repo, &candidates)?.is_empty())
}

/// Find the given paths which match LFS attributes or contain an index pointer.
pub fn tracked_paths(
	repo_path: &RepoPath,
	paths: &[String],
) -> Result<HashSet<String>> {
	let repo = repo(repo_path)?;
	if paths.is_empty() {
		return Ok(HashSet::new());
	}
	let lfs_repo = is_repo(&repo)?;
	let candidates = paths
		.iter()
		.filter(|path| {
			lfs_repo
				|| repo
					.get_attr(
						Path::new(path),
						"filter",
						git2::AttrCheckFlags::FILE_THEN_INDEX,
					)
					.ok()
					.flatten() == Some("lfs")
		})
		.map(String::as_str)
		.collect::<Vec<_>>();
	let mut tracked = if candidates.is_empty() {
		HashSet::new()
	} else {
		attributed_paths(&repo, &candidates)?
	};
	let index = repo.index()?;
	for path in paths {
		if !tracked.contains(path)
			&& index.get_path(Path::new(path), 0).is_some_and(
				|entry| blob_pointer(&repo, entry.id).is_some(),
			) {
			tracked.insert(path.clone());
		}
	}
	Ok(tracked)
}

fn worktree_pointer(
	repo: &Repository,
	path: &Path,
) -> (LfsWorktree, Option<LfsPointer>) {
	let Some(root) = repo.workdir() else {
		return (LfsWorktree::Unknown, None);
	};
	let path = root.join(path);
	let metadata = match fs::symlink_metadata(&path) {
		Ok(metadata) => metadata,
		Err(error)
			if error.kind() == std::io::ErrorKind::NotFound =>
		{
			return (LfsWorktree::Missing, None)
		}
		Err(_) => return (LfsWorktree::Unknown, None),
	};
	if !metadata.is_file() {
		return (LfsWorktree::Unknown, None);
	}
	if metadata.len() >= POINTER_LIMIT as u64 {
		return (LfsWorktree::Content, None);
	}
	let mut bytes = Vec::new();
	if fs::File::open(path)
		.and_then(|file| {
			file.take(POINTER_LIMIT as u64).read_to_end(&mut bytes)
		})
		.is_err()
	{
		return (LfsWorktree::Unknown, None);
	}
	parse_pointer(&bytes).map_or((LfsWorktree::Content, None), |p| {
		(LfsWorktree::Pointer, Some(p))
	})
}

fn media_directory(repo: &Repository) -> Result<PathBuf> {
	let output = git(repo, ["lfs", "env"], false)?;
	let output = String::from_utf8(output)?;
	output
		.lines()
		.find_map(|line| line.strip_prefix("LocalMediaDir="))
		.filter(|path| !path.is_empty())
		.map(PathBuf::from)
		.ok_or_else(|| {
			Error::Generic(
				"Git LFS did not report its local object directory."
					.into(),
			)
		})
}

fn object_path(store: &Path, pointer: &LfsPointer) -> PathBuf {
	store
		.join(&pointer.oid[..2])
		.join(&pointer.oid[2..4])
		.join(&pointer.oid)
}

fn object_exists(
	repo: &Repository,
	pointer: &LfsPointer,
) -> Option<bool> {
	Some(object_path(&media_directory(repo).ok()?, pointer).is_file())
}

/// Inspect the pointer and local content status of one file.
pub fn file_info(
	repo_path: &RepoPath,
	path: &str,
	source: LfsSource,
) -> Result<Option<LfsFileInfo>> {
	let repo = repo(repo_path)?;
	let path = Path::new(path);
	checked_path(path)?;
	let (worktree, working_pointer) = worktree_pointer(&repo, path);
	let pointer = match source {
		LfsSource::Worktree => {
			working_pointer.or_else(|| index_pointer(&repo, path))
		}
		LfsSource::Index => index_pointer(&repo, path),
		LfsSource::Commit(commit) => {
			let tree = repo.find_commit(commit.into())?.tree()?;
			tree_pointer(&repo, &tree, path)
		}
	};
	if pointer.is_none()
		&& (matches!(source, LfsSource::Commit(_))
			|| !is_path(&repo, path)?)
	{
		return Ok(None);
	}
	let local_object =
		pointer.as_ref().and_then(|p| object_exists(&repo, p));
	Ok(Some(LfsFileInfo {
		pointer,
		local_object,
		worktree,
	}))
}

fn literal_pattern(path: &str) -> Result<String> {
	checked_path(Path::new(path))?;
	if path.contains(['\r', '\n', '\\'])
		|| path.ends_with(char::is_whitespace)
	{
		return Err(Error::Generic("This file name cannot be expressed as an exact Git LFS path filter.".into()));
	}
	let mut pattern = String::from("/");
	for ch in path.chars() {
		if matches!(ch, '*' | '?' | '[' | ']') {
			pattern.push('\\');
		}
		pattern.push(ch);
	}
	Ok(pattern)
}

#[derive(Deserialize)]
struct LfsFiles {
	files: Option<Vec<LfsPath>>,
}

#[derive(Deserialize)]
struct LfsPath {
	name: String,
}

pub(crate) fn checkout_local(
	repo: &Repository,
	path: Option<&Path>,
) -> Result<()> {
	let files = if let Some(path) = path {
		vec![LfsPath {
			name: checked_path(path)?.to_owned(),
		}]
	} else {
		let output = git(repo, ["lfs", "ls-files", "--json"], false)?;
		let list: LfsFiles = serde_json::from_slice(&output)
			.map_err(|error| {
				Error::Generic(format!(
					"Could not read Git LFS file list: {error}"
				))
			})?;
		list.files.unwrap_or_default()
	};
	checkout_index_local(
		repo,
		&files
			.iter()
			.map(|file| file.name.as_str())
			.collect::<Vec<_>>(),
	)
}

/// Expand existing pointers using their index version, including unborn HEAD.
pub(crate) fn checkout_index_local(
	repo: &Repository,
	paths: &[&str],
) -> Result<()> {
	let mut index = repo.index()?;
	index.read(true)?;
	let mut candidates = Vec::new();
	for name in paths {
		let path = Path::new(name);
		checked_path(path)?;
		if checkout_candidate(repo, &index, path) {
			candidates.push((path, literal_pattern(name)?));
		}
	}
	// git-lfs checkout requires HEAD even though the restored version comes
	// from the index. Before the first commit, materialize local objects only.
	if repo.head().is_err_and(|error| {
		error.code() == git2::ErrorCode::UnbornBranch
	}) {
		restore_unborn(
			repo,
			candidates.iter().map(|(path, _)| *path),
		)?;
		return check_restore(
			repo,
			candidates.into_iter().map(|(path, _)| path),
		);
	}
	// Only existing, unchanged pointers are selected. An unfiltered Git LFS
	// checkout would also restore files that the user deleted.
	for batch in candidates.chunks(16) {
		git(
			repo,
			["lfs", "checkout", "--"].into_iter().chain(
				batch.iter().map(|(_, pattern)| pattern.as_str()),
			),
			false,
		)?;
	}
	check_restore(repo, candidates.into_iter().map(|(path, _)| path))
}

fn restore_unborn<'a>(
	repo: &Repository,
	paths: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
	let store = media_directory(repo)?;
	let root = repo.workdir().ok_or(Error::NoWorkDir)?;
	for path in paths {
		let (_, Some(pointer)) = worktree_pointer(repo, path) else {
			continue;
		};
		let object = object_path(&store, &pointer);
		if !object.is_file() {
			continue;
		}
		let destination = root.join(path);
		let original = fs::read(&destination)?;
		// Extension objects need their smudge pipeline, which checkout cannot
		// run before HEAD exists. Do not expose transformed cache bytes.
		if original
			.split(|b| *b == b'\n')
			.any(|line| line.starts_with(b"ext-"))
		{
			return Err(Error::Generic("Git LFS extension files cannot be restored before the first commit.".into()));
		}
		let temporary = tempfile::NamedTempFile::new_in(
			destination.parent().ok_or(Error::NoWorkDir)?,
		)?;
		let length = fs::copy(object, temporary.path())?;
		if length != pointer.size {
			return Err(Error::Generic(
				"The local Git LFS object has an unexpected size."
					.into(),
			));
		}
		fs::set_permissions(
			temporary.path(),
			fs::metadata(&destination)?.permissions(),
		)?;
		// Leave edits made during the copy intact.
		if worktree_pointer(repo, path).1.as_ref() != Some(&pointer) {
			continue;
		}
		temporary
			.persist(&destination)
			.map_err(|error| Error::Generic(error.to_string()))?;
	}
	Ok(())
}

fn checkout_candidate(
	repo: &Repository,
	index: &git2::Index,
	path: &Path,
) -> bool {
	let (_, working) = worktree_pointer(repo, path);
	let Some(working) = working else {
		return false;
	};
	let staged = index
		.get_path(path, 0)
		.and_then(|entry| blob_pointer(repo, entry.id));
	staged.as_ref() == Some(&working)
}
fn check_restore<'a>(
	repo: &Repository,
	paths: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
	let remaining = paths
		.into_iter()
		.filter(|path| {
			worktree_pointer(repo, path).0 == LfsWorktree::Pointer
		})
		.count();
	if remaining != 0 {
		return Err(Error::Generic(format!(
			"{remaining} Git LFS file(s) are still pointers. Download the required LFS objects to restore them."
		)));
	}
	Ok(())
}

/// Restore existing working-tree pointers using objects already in the local cache.
pub fn restore_local(
	repo_path: &RepoPath,
	path: Option<&str>,
) -> Result<()> {
	let repo = repo(repo_path)?;
	require_ready(&repo)?;
	checkout_local(&repo, path.map(Path::new))
}

/// Download objects referenced by the current commit and restore matching pointers.
pub fn download(
	repo_path: &RepoPath,
	remote: Option<&str>,
	path: Option<&str>,
) -> Result<()> {
	let repo = repo(repo_path)?;
	require_ready(&repo)?;
	let head = repo.head()?.peel_to_commit()?;
	let head_id = head.id().to_string();
	let mut args =
		vec!["-c", "lfs.fetchrecentalways=false", "lfs", "fetch"];
	let include;
	if let Some(path) = path {
		checked_path(Path::new(path))?;
		let pointer =
			tree_pointer(&repo, &head.tree()?, Path::new(path));
		if pointer.is_none()
			|| pointer != index_pointer(&repo, Path::new(path))
		{
			return Err(Error::Generic("Select an LFS file whose index version matches HEAD.".into()));
		}
		if path.contains(',') {
			return Err(Error::Generic("Git LFS include filters cannot select this file alone. Download the current version instead.".into()));
		}
		include = format!("--include={}", literal_pattern(path)?);
		args.extend([include.as_str(), "--exclude="]);
	}
	// An explicit ref needs an explicit remote. Git's upstream remote is the
	// normal default; Git LFS additionally accepts remote.lfsdefault.
	let default_remote;
	let remote = if let Some(remote) = remote {
		remote
	} else {
		default_remote = repo
			.head()
			.ok()
			.and_then(|head| {
				head.shorthand().ok().and_then(|name| {
					config_value(
						&repo,
						&format!("branch.{name}.remote"),
					)
				})
			})
			.or_else(|| config_value(&repo, "remote.lfsdefault"))
			.or_else(|| {
				super::get_default_remote_for_fetch(repo_path).ok()
			})
			.unwrap_or_else(|| "origin".into());
		&default_remote
	};
	args.extend(["--", remote, &head_id]);
	git(&repo, args, false)?;
	if repo.head()?.peel_to_commit()?.id() != head.id() {
		return Err(Error::Generic("Git LFS objects were downloaded, but HEAD changed. Run restore for the current version.".into()));
	}
	checkout_local(&repo, path.map(Path::new))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::tests::{repo_init, write_commit_file};

	#[test]
	fn missing_hook_checks_unpushed_history_after_lfs_file_removal() {
		let (_temp, repo) = repo_init().unwrap();
		let pointer = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 42\n", "a".repeat(64));
		let lfs_commit = write_commit_file(
			&repo,
			"asset.bin",
			&pointer,
			"add pointer",
		);
		let mut index = repo.index().unwrap();
		index.remove_path(Path::new("asset.bin")).unwrap();
		index.write().unwrap();
		fs::remove_file(repo.workdir().unwrap().join("asset.bin"))
			.unwrap();
		write_commit_file(
			&repo,
			"notes.txt",
			"ordinary",
			"remove pointer",
		);
		assert!(!is_repo(&repo).unwrap());
		let error =
			ensure_push_hook(&repo, ["HEAD"], &[]).unwrap_err();
		assert!(error.to_string().contains("pre-push hook"));
		// Objects already reachable from the advertised remote tip do not
		// require a fresh LFS upload for an ordinary removal commit.
		ensure_push_hook(&repo, ["HEAD"], &[lfs_commit.into()])
			.unwrap();
		ensure_push_hook(&repo, std::iter::empty::<&str>(), &[])
			.unwrap();
	}

	#[test]
	fn deleted_staged_attributes_do_not_hide_lfs_in_head() {
		let (_temp, repo) = repo_init().unwrap();
		assert!(!is_repo_at(&repo, "HEAD").unwrap());
		write_commit_file(
			&repo,
			".gitattributes",
			"*.bin filter=lfs\n",
			"LFS rules",
		);
		let mut index = repo.index().unwrap();
		index.remove_path(Path::new(".gitattributes")).unwrap();
		index.write().unwrap();
		fs::remove_file(
			repo.workdir().unwrap().join(".gitattributes"),
		)
		.unwrap();
		assert!(is_repo(&repo).unwrap());
		assert!(is_repo_at(&repo, "HEAD").unwrap());
	}
}
