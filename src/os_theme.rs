//! Process-wide detection of the external-tool color mode (`bat`, `delta`)
//! based on the OS light/dark color scheme.
//!
//! At startup [`detect_color_scheme`] is queried on Windows and Linux
//! and pinned via [`set`]. Unsupported platforms return Unknown.
//! The `bat`/`delta` spawn sites then read it via [`get`]. The detected
//! scheme must be passed explicitly because both tools normally see a pipe
//! rather than the terminal when launched by gitui.
//!
//! Both the detected scheme and the resolved themes are constant for the
//! lifetime of the process (a single snapshot at launch), so a plain
//! [`std::sync::OnceLock`] is sufficient — no need to thread the values
//! through every call signature.

use std::sync::OnceLock;

/// The OS color scheme as detected at launch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
	// Constructed by the Windows and Linux detectors below.
	#[cfg_attr(
		not(any(windows, target_os = "linux")),
		allow(dead_code)
	)]
	Light,
	#[cfg_attr(
		not(any(windows, target_os = "linux")),
		allow(dead_code)
	)]
	Dark,
	/// Detection is unavailable or the value could not be
	/// read. Callers fall back to current behavior (no forced theme).
	#[default]
	Unknown,
}

impl ColorScheme {
	/// `bat` special theme value selecting its configured/default theme for
	/// the already-detected terminal background.
	pub const fn bat_theme(self) -> Option<&'static str> {
		match self {
			Self::Light => Some("light"),
			Self::Dark => Some("dark"),
			Self::Unknown => None,
		}
	}

	/// Delta flag selecting both its syntax theme and its addition/deletion
	/// palette. Setting only `--syntax-theme` leaves delta's dark line
	/// backgrounds active when stdout is a pipe.
	pub const fn delta_flag(self) -> Option<&'static str> {
		match self {
			Self::Light => Some("--light"),
			Self::Dark => Some("--dark"),
			Self::Unknown => None,
		}
	}
}

static COLOR_SCHEME: OnceLock<ColorScheme> = OnceLock::new();

/// Pin the detected scheme. Called exactly once at startup; subsequent
/// calls are silently ignored (the first value wins).
pub fn set(scheme: ColorScheme) {
	let _ = COLOR_SCHEME.set(scheme);
}

/// The scheme detected at startup. Returns [`ColorScheme::Unknown`] if
/// [`set`] was never called, so callers leave external tools unchanged.
pub fn get() -> ColorScheme {
	COLOR_SCHEME.get().copied().unwrap_or_default()
}

/// Detect the OS color scheme.
///
/// On Windows this reads the `AppsUseLightTheme` registry value, which
/// Windows Terminal follows when its `theme` is set to `system` (the
/// default). Linux queries the desktop portal, then GNOME settings.
/// Unsupported platforms or failed detection return [`ColorScheme::Unknown`].
#[allow(clippy::missing_const_for_fn)]
pub fn detect_color_scheme() -> ColorScheme {
	#[cfg(windows)]
	{
		use std::process::Command;

		let out = Command::new("reg")
			.args([
				"query",
				"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
				"/v",
				"AppsUseLightTheme",
			])
			.output();

		if let Ok(o) = out {
			let s = String::from_utf8_lossy(&o.stdout);
			// The line looks like:
			// `    AppsUseLightTheme    REG_DWORD    0x1`
			if s.contains("0x1") {
				return ColorScheme::Light;
			}
			if s.contains("0x0") {
				return ColorScheme::Dark;
			}
		}

		ColorScheme::Unknown
	}

	#[cfg(target_os = "linux")]
	{
		detect_linux_color_scheme()
	}

	#[cfg(not(any(windows, target_os = "linux")))]
	{
		ColorScheme::Unknown
	}
}

/// Query only small settings responses, with a deadline so unavailable desktop
/// services cannot indefinitely block startup (including over SSH).
#[cfg(target_os = "linux")]
fn settings_output(program: &str, args: &[&str]) -> Option<String> {
	use std::{
		process::{Command, Stdio},
		time::{Duration, Instant},
	};

	let mut child = Command::new(program)
		.args(args)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.ok()?;
	let deadline = Instant::now() + Duration::from_secs(1);
	loop {
		match child.try_wait() {
			Ok(Some(status)) if status.success() => {
				let output = child.wait_with_output().ok()?;
				return String::from_utf8(output.stdout).ok();
			}
			Ok(Some(_)) => return None,
			Ok(None) if Instant::now() < deadline => {
				std::thread::sleep(Duration::from_millis(10));
			}
			_ => {
				let _ = child.kill();
				let _ = child.wait();
				return None;
			}
		}
	}
}

#[cfg(target_os = "linux")]
fn parse_portal_scheme(output: &str) -> ColorScheme {
	// Settings.Read wraps its variant result: busctl prints "v v u 2".
	// Accept a single variant too, but reject unrelated/malformed values.
	let words: Vec<_> = output.split_whitespace().collect();
	match words.as_slice() {
		["v", "v", "u", "1"] | ["v", "u", "1"] => ColorScheme::Dark,
		["v", "v", "u", "2"] | ["v", "u", "2"] => ColorScheme::Light,
		_ => ColorScheme::Unknown,
	}
}

#[cfg(target_os = "linux")]
fn parse_gsettings_scheme(output: &str) -> ColorScheme {
	match output.trim() {
		"'prefer-light'" => ColorScheme::Light,
		"'prefer-dark'" => ColorScheme::Dark,
		_ => ColorScheme::Unknown,
	}
}

#[cfg(target_os = "linux")]
fn detect_linux_color_scheme() -> ColorScheme {
	let portal = settings_output(
		"busctl",
		&[
			"--user",
			"--timeout=1",
			"call",
			"org.freedesktop.portal.Desktop",
			"/org/freedesktop/portal/desktop",
			"org.freedesktop.portal.Settings",
			"Read",
			"ss",
			"org.freedesktop.appearance",
			"color-scheme",
		],
	)
	.map_or(ColorScheme::Unknown, |s| parse_portal_scheme(&s));
	if portal != ColorScheme::Unknown {
		return portal;
	}

	settings_output(
		"gsettings",
		&["get", "org.gnome.desktop.interface", "color-scheme"],
	)
	.map_or(ColorScheme::Unknown, |s| parse_gsettings_scheme(&s))
}

#[cfg(test)]
mod tests {
	use super::ColorScheme;

	#[cfg(target_os = "linux")]
	#[test]
	fn linux_settings_values() {
		use super::{parse_gsettings_scheme, parse_portal_scheme};
		for (text, expected) in [
			("v v u 1", ColorScheme::Dark),
			("v v u 2\n", ColorScheme::Light),
			("v u 2", ColorScheme::Light),
			("v v u 0", ColorScheme::Unknown),
			("v v u 3", ColorScheme::Unknown),
			("error 2", ColorScheme::Unknown),
			("", ColorScheme::Unknown),
		] {
			assert_eq!(parse_portal_scheme(text), expected, "{text}");
		}
		for (text, expected) in [
			("'prefer-light'\n", ColorScheme::Light),
			("'prefer-dark'", ColorScheme::Dark),
			("'default'", ColorScheme::Unknown),
			("", ColorScheme::Unknown),
			("'unexpected'", ColorScheme::Unknown),
		] {
			assert_eq!(
				parse_gsettings_scheme(text),
				expected,
				"{text}"
			);
		}
	}

	#[test]
	fn detected_scheme_maps_to_external_tool_modes() {
		assert_eq!(ColorScheme::Light.bat_theme(), Some("light"));
		assert_eq!(ColorScheme::Light.delta_flag(), Some("--light"));
		assert_eq!(ColorScheme::Dark.bat_theme(), Some("dark"));
		assert_eq!(ColorScheme::Dark.delta_flag(), Some("--dark"));
		assert_eq!(ColorScheme::Unknown.bat_theme(), None);
		assert_eq!(ColorScheme::Unknown.delta_flag(), None);
	}
}
