use super::push::ProgressNotification;
use crate::{error::Result, sync::cred::BasicAuthCredential};
use crossbeam_channel::Sender;
use git2::{Cred, Error as GitError, RemoteCallbacks};
use std::sync::{
	atomic::{AtomicBool, AtomicUsize, Ordering},
	Arc, Mutex,
};

///
#[derive(Default, Clone)]
pub struct CallbackStats {
	pub push_rejected_msg: Option<(String, String)>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ssh_fallback_is_bounded_and_shared_between_clones() {
		let home = tempfile::tempdir().unwrap();
		let ssh = home.path().join(".ssh");
		std::fs::create_dir(&ssh).unwrap();
		// Credential construction is lazy; these files only test candidate selection.
		std::fs::write(ssh.join("id_ed25519"), "test").unwrap();
		std::fs::write(ssh.join("id_rsa"), "test").unwrap();
		let callbacks = Callbacks::new(None, None);
		// Simulate the agent credential having been rejected by libgit2.
		callbacks.ssh_attempt.store(1, Ordering::Relaxed);
		let clone = callbacks.clone();
		assert!(callbacks
			.ssh_credentials("git", Some(home.path()))
			.is_ok());
		assert_eq!(callbacks.ssh_attempt.load(Ordering::Relaxed), 2);
		assert!(clone
			.ssh_credentials("git", Some(home.path()))
			.is_ok());
		assert_eq!(callbacks.ssh_attempt.load(Ordering::Relaxed), 4);
		assert!(callbacks
			.ssh_credentials("git", Some(home.path()))
			.is_err());
		assert!(clone
			.ssh_credentials("git", Some(home.path()))
			.is_err());
	}

	#[test]
	fn ssh_fallback_without_home_terminates() {
		let callbacks = Callbacks::new(None, None);
		callbacks.ssh_attempt.store(1, Ordering::Relaxed);
		assert!(callbacks.ssh_credentials("git", None).is_err());
	}

	#[test]
	fn password_credentials_are_not_retried() {
		let callbacks = Callbacks::new(
			None,
			Some(BasicAuthCredential::new(
				Some("user".into()),
				Some("password".into()),
			)),
		);
		let kind = git2::CredentialType::USER_PASS_PLAINTEXT;
		assert!(callbacks
			.credentials("https://example.com", None, kind)
			.is_ok());
		assert!(callbacks
			.credentials("https://example.com", None, kind)
			.is_err());
	}
}

///
#[derive(Clone)]
pub struct Callbacks {
	sender: Option<Sender<ProgressNotification>>,
	basic_credential: Option<BasicAuthCredential>,
	stats: Arc<Mutex<CallbackStats>>,
	first_call_to_credentials: Arc<AtomicBool>,
	ssh_attempt: Arc<AtomicUsize>,
}

impl Callbacks {
	///
	pub fn new(
		sender: Option<Sender<ProgressNotification>>,
		basic_credential: Option<BasicAuthCredential>,
	) -> Self {
		let stats = Arc::new(Mutex::new(CallbackStats::default()));

		Self {
			sender,
			basic_credential,
			stats,
			first_call_to_credentials: Arc::new(AtomicBool::new(
				true,
			)),
			ssh_attempt: Arc::new(AtomicUsize::new(0)),
		}
	}

	///
	pub fn get_stats(&self) -> Result<CallbackStats> {
		let stats = self.stats.lock()?;
		Ok(stats.clone())
	}

	///
	pub fn callbacks<'a>(&self) -> RemoteCallbacks<'a> {
		let mut callbacks = RemoteCallbacks::new();

		let this = self.clone();
		callbacks.push_transfer_progress(
			move |current, total, bytes| {
				this.push_transfer_progress(current, total, bytes);
			},
		);

		let this = self.clone();
		callbacks.update_tips(move |name, a, b| {
			this.update_tips(name, a, b);
			true
		});

		let this = self.clone();
		callbacks.transfer_progress(move |p| {
			this.transfer_progress(&p);
			true
		});

		let this = self.clone();
		callbacks.pack_progress(move |stage, current, total| {
			this.pack_progress(stage, current, total);
		});

		let this = self.clone();
		callbacks.push_update_reference(move |reference, msg| {
			this.push_update_reference(reference, msg);
			Ok(())
		});

		let this = self.clone();
		callbacks.credentials(
			move |url, username_from_url, allowed_types| {
				this.credentials(
					url,
					username_from_url,
					allowed_types,
				)
			},
		);

		callbacks.sideband_progress(move |data| {
			log::debug!(
				"sideband transfer: '{}'",
				String::from_utf8_lossy(data).trim()
			);
			true
		});

		callbacks
	}

	fn push_update_reference(
		&self,
		reference: &str,
		msg: Option<&str>,
	) {
		log::debug!("push_update_reference: '{reference}' {msg:?}");

		if let Ok(mut stats) = self.stats.lock() {
			stats.push_rejected_msg = msg
				.map(|msg| (reference.to_string(), msg.to_string()));
		}
	}

	fn pack_progress(
		&self,
		stage: git2::PackBuilderStage,
		current: usize,
		total: usize,
	) {
		log::debug!("packing: {stage:?} - {current}/{total}");
		self.sender.clone().map(|sender| {
			sender.send(ProgressNotification::Packing {
				stage,
				current,
				total,
			})
		});
	}

	fn transfer_progress(&self, p: &git2::Progress) {
		log::debug!(
			"transfer: {}/{}",
			p.received_objects(),
			p.total_objects()
		);
		self.sender.clone().map(|sender| {
			sender.send(ProgressNotification::Transfer {
				objects: p.received_objects(),
				total_objects: p.total_objects(),
				indexed_objects: p.indexed_objects(),
			})
		});
	}

	fn update_tips(&self, name: &str, a: git2::Oid, b: git2::Oid) {
		log::debug!("update tips: '{name}' [{a}] [{b}]");
		self.sender.clone().map(|sender| {
			sender.send(ProgressNotification::UpdateTips {
				name: name.to_string(),
				a: a.into(),
				b: b.into(),
			})
		});
	}

	fn push_transfer_progress(
		&self,
		current: usize,
		total: usize,
		bytes: usize,
	) {
		log::debug!("progress: {current}/{total} ({bytes} B)");
		self.sender.clone().map(|sender| {
			sender.send(ProgressNotification::PushTransfer {
				current,
				total,
				bytes,
			})
		});
	}

	// If credentials are bad, we don't ask the user to re-fill their creds. We push an error and they will be able to restart their action (for example a push) and retype their creds.
	// This behavior is explained in a issue on git2-rs project : https://github.com/rust-lang/git2-rs/issues/347
	// An implementation reference is done in cargo : https://github.com/rust-lang/cargo/blob/9fb208dddb12a3081230a5fd8f470e01df8faa25/src/cargo/sources/git/utils.rs#L588
	// There is also a guide about libgit2 authentication : https://libgit2.org/docs/guides/authentication/
	fn credentials(
		&self,
		url: &str,
		username_from_url: Option<&str>,
		allowed_types: git2::CredentialType,
	) -> std::result::Result<Cred, GitError> {
		log::debug!(
			"creds: '{url}' {username_from_url:?} ({allowed_types:?})",
		);

		if allowed_types.is_ssh_key() {
			let username = username_from_url.ok_or_else(|| {
				GitError::from_str(
					"Couldn't extract username from url.",
				)
			})?;
			return self.ssh_credentials(
				username,
				dirs::home_dir().as_deref(),
			);
		}

		// Password credentials are tried only once to avoid retry loops.
		if self.first_call_to_credentials.load(Ordering::Relaxed) {
			self.first_call_to_credentials
				.store(false, Ordering::Relaxed);
		} else {
			return Err(GitError::from_str("Bad credentials."));
		}

		match &self.basic_credential {
			Some(BasicAuthCredential {
				username: Some(user),
				password: Some(pwd),
			}) if allowed_types.is_user_pass_plaintext() => {
				Cred::userpass_plaintext(user, pwd)
			}
			Some(BasicAuthCredential {
				username: Some(user),
				password: _,
			}) if allowed_types.is_username() => Cred::username(user),
			_ if allowed_types.is_default() => Cred::default(),
			_ => Err(GitError::from_str("Couldn't find credentials")),
		}
	}

	fn ssh_credentials(
		&self,
		username: &str,
		home: Option<&std::path::Path>,
	) -> std::result::Result<Cred, GitError> {
		const KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];
		loop {
			let attempt =
				self.ssh_attempt.fetch_add(1, Ordering::Relaxed);
			if attempt == 0 {
				if let Ok(cred) = Cred::ssh_key_from_agent(username) {
					return Ok(cred);
				}
				continue;
			}
			let Some(key) = KEYS.get(attempt - 1) else {
				return Err(GitError::from_str(
					"SSH authentication failed. Load your key into ssh-agent; default keys in ~/.ssh were also tried.",
				));
			};
			if let Some(home) = home {
				let private_key = home.join(".ssh").join(key);
				if private_key.is_file() {
					// libssh2 derives the public key; encrypted keys still need an agent.
					if let Ok(cred) = Cred::ssh_key(
						username,
						None,
						&private_key,
						None,
					) {
						return Ok(cred);
					}
				}
			}
		}
	}
}
