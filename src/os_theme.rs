//! Process-wide detection of the external-tool color mode (`bat`, `delta`)
//! based on the OS light/dark color scheme.
//!
//! At startup [`detect_color_scheme`] is queried (Windows only; on other
//! platforms it returns [`ColorScheme::Unknown`]) and pinned via [`set`].
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
	Light,
	Dark,
	/// Detection is unavailable (non-Windows) or the value could not be
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
/// default). On other platforms, or if the value can't be read, this
/// returns [`ColorScheme::Unknown`].
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

	#[cfg(not(windows))]
	{
		ColorScheme::Unknown
	}
}

#[cfg(test)]
mod tests {
	use super::ColorScheme;

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
