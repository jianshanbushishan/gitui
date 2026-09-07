use crate::AsyncAppNotification;
use crossbeam_channel::{bounded, Receiver, Sender};
use image::{DynamicImage, ImageReader};
use once_cell::sync::OnceCell;
use ratatui::{
	buffer::Buffer,
	layout::Rect,
	widgets::{Paragraph, Widget},
};
use ratatui_image::{
	picker::Picker, protocol::Protocol, Image, Resize,
};
use std::{
	collections::VecDeque,
	io::Cursor,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
};

static IMAGE_PICKER: OnceCell<Picker> = OnceCell::new();
const MAX_PREVIEW_EDGE: u32 = 800;
const CACHE_ENTRIES: usize = 4;

/// Detect the terminal's best image protocol before starting the input thread.
pub fn init_terminal_image_support() {
	let picker = Picker::from_query_stdio().unwrap_or_else(|error| {
		log::debug!("terminal image protocol detection failed ({error}); using halfblocks");
		Picker::halfblocks()
	});
	log::info!(
		"terminal image protocol: {:?}",
		picker.protocol_type()
	);
	let _ = IMAGE_PICKER.set(picker);
}

/// Recognize image signatures without decoding on the UI thread.
pub fn is_image(bytes: &[u8]) -> bool {
	image::guess_format(bytes).is_ok()
}

fn decode(bytes: &[u8]) -> image::ImageResult<DynamicImage> {
	let mut reader =
		ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
	reader.limits(image::Limits::default());
	reader.decode()
}

fn thumbnail(image: DynamicImage) -> DynamicImage {
	if image.width().max(image.height()) > MAX_PREVIEW_EDGE {
		image.thumbnail(MAX_PREVIEW_EDGE, MAX_PREVIEW_EDGE)
	} else {
		image
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
	content: u64,
	width: u16,
	height: u16,
}

type Encoded = Result<Arc<Protocol>, String>;

struct Request {
	generation: u64,
	key: Key,
	bytes: Arc<[u8]>,
}

struct Completion {
	generation: u64,
	key: Key,
	encoded: Encoded,
}

#[derive(Default)]
struct Mailbox {
	request: Option<Request>,
	completion: Option<Completion>,
}

#[derive(Default)]
struct Shared {
	generation: AtomicU64,
	mailbox: Mutex<Mailbox>,
}

/// A lazy background encoder with a single replaceable request slot.
/// Dropping it disconnects the worker without waiting for an active encoding.
pub struct ImagePreview {
	sender: Sender<AsyncAppNotification>,
	shared: Arc<Shared>,
	wake: Option<Sender<()>>,
	source: Option<(u64, Arc<[u8]>)>,
	requested: Option<Key>,
	encoded: Option<Encoded>,
}

impl ImagePreview {
	pub fn new(sender: Sender<AsyncAppNotification>) -> Self {
		Self {
			sender,
			shared: Arc::default(),
			wake: None,
			source: None,
			requested: None,
			encoded: None,
		}
	}

	pub fn set(&mut self, bytes: &[u8], content_hash: u64) {
		if self
			.source
			.as_ref()
			.is_some_and(|(hash, _)| *hash == content_hash)
		{
			return;
		}
		self.clear();
		self.source = Some((content_hash, Arc::from(bytes)));
	}

	pub fn clear(&mut self) {
		self.cancel();
		self.source = None;
	}

	fn cancel(&mut self) {
		self.shared.generation.fetch_add(1, Ordering::Relaxed);
		let mut mailbox = self
			.shared
			.mailbox
			.lock()
			.expect("image mailbox poisoned");
		mailbox.request = None;
		mailbox.completion = None;
		drop(mailbox);
		self.requested = None;
		self.encoded = None;
	}

	pub fn is_pending(&self) -> bool {
		self.requested.is_some()
			&& self.encoded.is_none()
			&& self
				.shared
				.mailbox
				.lock()
				.expect("image mailbox poisoned")
				.completion
				.is_none()
	}

	pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
		if area.width == 0 || area.height == 0 {
			self.cancel();
			return;
		}
		let Some((content, bytes)) = self.source.as_ref() else {
			return;
		};
		let key = Key {
			content: *content,
			width: area.width,
			height: area.height,
		};
		if self.requested != Some(key) {
			let bytes = Arc::clone(bytes);
			self.cancel();
			self.requested = Some(key);
			if self.wake.is_none() {
				let (wake, receiver) = bounded(1);
				let shared = Arc::clone(&self.shared);
				let sender = self.sender.clone();
				match std::thread::Builder::new()
					.name("image-preview".into())
					.spawn(move || {
						worker(&shared, &receiver, &sender);
					}) {
					Ok(_) => self.wake = Some(wake),
					Err(error) => {
						self.encoded = Some(Err(error.to_string()));
					}
				}
			}
			if let Some(wake) = &self.wake {
				self.shared
					.mailbox
					.lock()
					.expect("image mailbox poisoned")
					.request = Some(Request {
					generation: self
						.shared
						.generation
						.load(Ordering::Relaxed),
					key,
					bytes,
				});
				let _ = wake.try_send(());
			}
		}
		let completion = self
			.shared
			.mailbox
			.lock()
			.expect("image mailbox poisoned")
			.completion
			.take();
		if let Some(completion) = completion {
			if completion.generation
				== self.shared.generation.load(Ordering::Relaxed)
				&& self.requested == Some(completion.key)
			{
				self.encoded = Some(completion.encoded);
			}
		}
		match self.encoded.as_ref() {
			Some(Ok(protocol)) => {
				Image::new(protocol).render(area, buf);
			}
			Some(Err(error)) => Paragraph::new(format!(
				"Image preview failed: {error}"
			))
			.render(area, buf),
			None => {
				Paragraph::new("Loading image…").render(area, buf);
			}
		}
	}
}

impl Drop for ImagePreview {
	fn drop(&mut self) {
		self.shared.generation.fetch_add(1, Ordering::Relaxed);
	}
}

fn worker(
	shared: &Shared,
	receiver: &Receiver<()>,
	sender: &Sender<AsyncAppNotification>,
) {
	let picker = IMAGE_PICKER.get_or_init(Picker::halfblocks);
	let mut cache: VecDeque<(Key, Encoded)> = VecDeque::new();
	while receiver.recv().is_ok() {
		let request = shared
			.mailbox
			.lock()
			.expect("image mailbox poisoned")
			.request
			.take();
		let Some(request) = request else {
			continue;
		};
		if request.generation
			!= shared.generation.load(Ordering::Relaxed)
		{
			continue;
		}
		let encoded = if let Some(index) =
			cache.iter().position(|(key, _)| *key == request.key)
		{
			let entry =
				cache.remove(index).expect("cache index exists");
			let encoded = entry.1.clone();
			cache.push_back(entry);
			encoded
		} else {
			let image = decode(&request.bytes)
				.map(thumbnail)
				.map_err(|error| error.to_string());
			// Switching files or resizing while decoding skips the expensive encoder.
			if request.generation
				!= shared.generation.load(Ordering::Relaxed)
			{
				continue;
			}
			let encoded = image.and_then(|image| {
				picker
					.new_protocol(
						image,
						Rect::new(
							0,
							0,
							request.key.width,
							request.key.height,
						),
						Resize::Fit(None),
					)
					.map(Arc::new)
					.map_err(|error| error.to_string())
			});
			if cache.len() == CACHE_ENTRIES {
				cache.pop_front();
			}
			cache.push_back((request.key, encoded.clone()));
			encoded
		};
		let mut mailbox =
			shared.mailbox.lock().expect("image mailbox poisoned");
		if request.generation
			== shared.generation.load(Ordering::Relaxed)
		{
			mailbox.completion = Some(Completion {
				generation: request.generation,
				key: request.key,
				encoded,
			});
			drop(mailbox);
			let _ =
				sender.try_send(AsyncAppNotification::ImagePreview);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use image::{ImageFormat, Rgba, RgbaImage};
	use std::time::Duration;

	fn png() -> Vec<u8> {
		let source = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
			20,
			30,
			Rgba([1, 2, 3, 255]),
		));
		let mut encoded = Cursor::new(Vec::new());
		source.write_to(&mut encoded, ImageFormat::Png).unwrap();
		encoded.into_inner()
	}

	#[test]
	fn detects_signatures_without_decoding() {
		assert!(is_image(b"\x89PNG\r\n\x1a\n"));
		assert!(decode(b"\x89PNG\r\n\x1a\n").is_err());
		assert!(!is_image(b"plain text"));
		assert_eq!(decode(&png()).unwrap().width(), 20);
	}

	#[test]
	fn caps_preview_preserving_aspect_ratio_without_enlarging() {
		let resized = thumbnail(DynamicImage::new_rgba8(2048, 1024));
		assert_eq!((resized.width(), resized.height()), (800, 400));
		let small = thumbnail(DynamicImage::new_rgba8(20, 30));
		assert_eq!((small.width(), small.height()), (20, 30));
	}

	fn complete(
		preview: &mut ImagePreview,
		notifications: &Receiver<AsyncAppNotification>,
		area: Rect,
	) -> Arc<Protocol> {
		let mut buf = Buffer::empty(area);
		preview.render(area, &mut buf);
		while preview.encoded.is_none() {
			notifications
				.recv_timeout(Duration::from_secs(5))
				.unwrap();
			preview.render(area, &mut buf);
		}
		preview.encoded.as_ref().unwrap().as_ref().unwrap().clone()
	}

	#[test]
	fn caches_content_and_dimensions_and_invalidates_changed_content()
	{
		let (sender, receiver) = bounded(8);
		let mut preview = ImagePreview::new(sender);
		assert!(preview.wake.is_none());
		preview.set(&png(), 1);
		let area = Rect::new(0, 0, 10, 10);
		let first = complete(&mut preview, &receiver, area);
		let moved = complete(
			&mut preview,
			&receiver,
			Rect::new(2, 2, 10, 10),
		);
		assert!(Arc::ptr_eq(&first, &moved));
		let resized =
			complete(&mut preview, &receiver, Rect::new(0, 0, 1, 1));
		assert!(!Arc::ptr_eq(&first, &resized));
		let restored = complete(&mut preview, &receiver, area);
		assert!(Arc::ptr_eq(&first, &restored));
		preview.set(&png(), 2);
		let changed = complete(&mut preview, &receiver, area);
		assert!(!Arc::ptr_eq(&first, &changed));
	}

	#[test]
	fn clear_discards_completed_work_and_zero_area_does_not_start_worker(
	) {
		let (sender, receiver) = bounded(8);
		let mut preview = ImagePreview::new(sender);
		preview.set(&png(), 1);
		preview.render(
			Rect::default(),
			&mut Buffer::empty(Rect::default()),
		);
		assert!(preview.wake.is_none());
		let area = Rect::new(0, 0, 10, 10);
		preview.render(area, &mut Buffer::empty(area));
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		preview.clear();
		assert!(!preview.is_pending());
		assert!(preview
			.shared
			.mailbox
			.lock()
			.unwrap()
			.completion
			.is_none());
		assert!(preview.encoded.is_none());
	}

	#[test]
	fn latest_request_replaces_queued_work_and_stale_completion_is_ignored(
	) {
		let (sender, _) = bounded(8);
		let mut preview = ImagePreview::new(sender);
		// Keep the wake receiver alive without starting a worker, making
		// the request/completion interleaving deterministic.
		let (wake, _receiver) = bounded(1);
		preview.wake = Some(wake);
		let area = Rect::new(0, 0, 10, 10);
		let mut buf = Buffer::empty(area);
		preview.set(&png(), 1);
		assert!(!preview.is_pending());
		preview.render(area, &mut buf);
		let old_generation =
			preview.shared.generation.load(Ordering::Relaxed);
		let old_key = preview.requested.unwrap();
		preview.set(&png(), 2);
		preview.render(area, &mut buf);
		assert!(preview.is_pending());
		{
			let mut mailbox = preview.shared.mailbox.lock().unwrap();
			assert_eq!(
				mailbox.request.as_ref().unwrap().key.content,
				2
			);
			mailbox.completion = Some(Completion {
				generation: old_generation,
				key: old_key,
				encoded: Err("stale result".into()),
			});
		}
		preview.render(area, &mut buf);
		assert!(preview.encoded.is_none());
		assert!(preview.is_pending());
		preview.render(
			Rect::default(),
			&mut Buffer::empty(Rect::default()),
		);
		assert!(!preview.is_pending());
	}

	#[test]
	fn cache_evicts_oldest_entry_at_capacity() {
		let (sender, receiver) = bounded(8);
		let mut preview = ImagePreview::new(sender);
		let area = Rect::new(0, 0, 10, 10);
		preview.set(&png(), 0);
		let first = complete(&mut preview, &receiver, area);
		for content in 1..=CACHE_ENTRIES as u64 {
			preview.set(&png(), content);
			complete(&mut preview, &receiver, area);
		}
		preview.set(&png(), 0);
		let evicted = complete(&mut preview, &receiver, area);
		assert!(!Arc::ptr_eq(&first, &evicted));
	}
	#[test]
	fn corrupt_image_reports_an_error() {
		let (sender, receiver) = bounded(8);
		let mut preview = ImagePreview::new(sender);
		preview.set(b"\x89PNG\r\n\x1a\n", 1);
		let area = Rect::new(0, 0, 60, 2);
		let mut buf = Buffer::empty(area);
		preview.render(area, &mut buf);
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		preview.render(area, &mut buf);
		assert!(!preview.is_pending());
		assert!(preview.encoded.as_ref().unwrap().is_err());
		assert_eq!(buf[(0, 0)].symbol(), "I");
	}
}
