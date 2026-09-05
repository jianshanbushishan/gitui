use image::{DynamicImage, ImageReader};
use once_cell::sync::OnceCell;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use std::io::Cursor;

static IMAGE_PICKER: OnceCell<Picker> = OnceCell::new();

/// Detect the terminal's best image protocol.
///
/// This must run after entering the alternate screen and before the input
/// thread starts, because capability detection temporarily reads terminal
/// responses from stdin.
pub fn init_terminal_image_support() {
	let picker = Picker::from_query_stdio().unwrap_or_else(|error| {
		log::debug!(
			"terminal image protocol detection failed ({error}); using halfblocks"
		);
		Picker::halfblocks()
	});
	log::info!(
		"terminal image protocol: {:?}",
		picker.protocol_type()
	);
	let _ = IMAGE_PICKER.set(picker);
}

/// Decode image bytes using their file signature rather than the extension.
pub fn decode(bytes: &[u8]) -> image::ImageResult<DynamicImage> {
	let mut reader =
		ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
	reader.limits(image::Limits::default());
	reader.decode()
}

/// Decode an image and create terminal-specific, resizeable render state.
/// Tests and other non-interactive callers safely fall back to halfblocks if
/// terminal capability detection has not been initialized.
pub fn protocol(
	bytes: &[u8],
) -> image::ImageResult<StatefulProtocol> {
	let image = decode(bytes)?;
	let picker = IMAGE_PICKER.get_or_init(Picker::halfblocks);
	Ok(picker.new_resize_protocol(image))
}

#[cfg(test)]
mod tests {
	use super::decode;
	use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
	use std::io::Cursor;

	#[test]
	fn decodes_png_by_signature() {
		let source = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
			2,
			3,
			Rgba([1, 2, 3, 255]),
		));
		let mut encoded = Cursor::new(Vec::new());
		source.write_to(&mut encoded, ImageFormat::Png).unwrap();

		let decoded = decode(encoded.get_ref()).unwrap();
		assert_eq!((decoded.width(), decoded.height()), (2, 3));
	}

	#[test]
	fn rejects_non_image_bytes() {
		assert!(decode(b"plain text").is_err());
	}
}
