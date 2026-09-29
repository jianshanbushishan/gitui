use std::process::Command;

fn track_git_path(path: &str) {
	if let Ok(output) = Command::new("git")
		.args(["rev-parse", "--git-path", path])
		.output()
	{
		if output.status.success() {
			let path = String::from_utf8_lossy(&output.stdout);
			println!("cargo:rerun-if-changed={}", path.trim());
		}
	}
}

fn get_version() -> String {
	let package_version = env!("CARGO_PKG_VERSION");
	if let Ok(commit) = std::env::var("BUILD_GIT_COMMIT_ID") {
		return format!(
			"{package_version}-{}",
			commit.chars().take(8).collect::<String>()
		);
	}

	let describe = Command::new("git")
		.arg("describe")
		.arg("--tags")
		.arg("--always")
		.arg("--dirty")
		.arg("--match")
		.arg(format!("v{package_version}"))
		.arg("--match")
		.arg(package_version)
		.output();

	match describe {
		Ok(output) => {
			let raw = String::from_utf8_lossy(&output.stdout);
			let line = raw.lines().next().unwrap_or("").trim();
			if line.is_empty() {
				return package_version.to_string();
			}
			if let Some(suffix) = line
				.strip_prefix(&format!("v{package_version}"))
				.or_else(|| line.strip_prefix(package_version))
			{
				format!("{package_version}{suffix}")
			} else {
				format!("{package_version}-{line}")
			}
		}
		Err(_) => package_version.to_string(),
	}
}

fn main() {
	let build_name = if std::env::var("GITUI_RELEASE").is_ok() {
		env!("CARGO_PKG_VERSION").to_string()
	} else {
		get_version()
	};

	println!("cargo:warning=buildname '{build_name}'");
	println!("cargo:rustc-env=GITUI_BUILD_NAME={build_name}");

	println!("cargo:rerun-if-changed=build.rs");
	println!("cargo:rerun-if-env-changed=BUILD_GIT_COMMIT_ID");
	println!("cargo:rerun-if-env-changed=GITUI_RELEASE");
	track_git_path("HEAD");
	track_git_path("logs/HEAD");
	track_git_path("refs/tags");
	track_git_path("packed-refs");
	if let Ok(output) = Command::new("git")
		.args(["symbolic-ref", "--quiet", "HEAD"])
		.output()
	{
		if output.status.success() {
			let branch = String::from_utf8_lossy(&output.stdout);
			track_git_path(branch.trim());
		}
	}
}
