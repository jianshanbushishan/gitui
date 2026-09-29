use std::process::Command;

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
	// git 状态（tag/commit/分支）变化时刷新版本号。
	// git 在 commit/checkout 时会重写 .git/HEAD，借此触发重跑。
	println!("cargo:rerun-if-changed=.git/HEAD");
}
