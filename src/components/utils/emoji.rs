use once_cell::sync::Lazy;
use std::borrow::Cow;

static EMOJI_REPLACER: Lazy<gh_emoji::Replacer> =
	Lazy::new(gh_emoji::Replacer::new);

// Replace markdown emojis with Unicode equivalent
// :hammer: --> 🔨
#[inline]
pub fn emojifi_string(s: String) -> String {
	if s.as_bytes().iter().filter(|&&b| b == b':').take(2).count() < 2
	{
		return s;
	}
	if let Cow::Owned(altered_s) = EMOJI_REPLACER.replace_all(&s) {
		altered_s
	} else {
		s
	}
}
