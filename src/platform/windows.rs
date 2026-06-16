/*
SPDX-License-Identifier: Apache-2.0 OR MIT

Copyright 2022 The Arboard contributors

The project to which this file belongs is licensed under either of
the Apache 2.0 or the MIT license at the licensee's choice. The terms
and conditions of the chosen license apply to this file.
*/

use crate::{
	common::{into_unknown, private, Error, ImageData, ImageRgba},
	ClipboardData, ClipboardFormat,
};
use clipboard_win::{formats::Html, options, Getter};
use std::{borrow::Cow, marker::PhantomData, thread, time::Duration};
use windows_sys::Win32::Foundation::{
	ERROR_CLIPBOARD_NOT_OPEN, ERROR_IO_INCOMPLETE, ERROR_NOT_FOUND, ERROR_SUCCESS,
};

const CFSTR_MIME_RICHTEXT: &str = "text/richtext";
const CFSTR_MIME_PNG: &str = "image/png";
const CFSTR_MIME_SVG_XML: &str = "image/svg+xml";

// If there're multiple threads or processes trying to access the clipboard at the same time,
// the previous clipboard owner will fail to access the clipboard.
// This is a common issue on Windows, so we just return `ClipboardOccupied` in this case.
// The user can retry the operation later.
#[inline]
fn last_error(msg: &str) -> Error {
	let last_err = std::io::Error::last_os_error();
	let raw_os_error = last_err.raw_os_error().unwrap_or(ERROR_SUCCESS as _);
	if raw_os_error == ERROR_CLIPBOARD_NOT_OPEN as _ || raw_os_error == ERROR_IO_INCOMPLETE as _ {
		Error::ClipboardOccupied
	} else if raw_os_error == ERROR_NOT_FOUND as _ {
		Error::ContentNotAvailable
	} else {
		into_unknown(msg, last_err)
	}
}

#[inline]
fn map_error_code(msg: &str, error_code: clipboard_win::ErrorCode) -> Error {
	let raw_code = error_code.raw_code();
	if raw_code == ERROR_CLIPBOARD_NOT_OPEN as _ || raw_code == ERROR_IO_INCOMPLETE as _ {
		Error::ClipboardOccupied
	} else if raw_code == ERROR_NOT_FOUND as _ {
		Error::ContentNotAvailable
	} else {
		into_unknown(msg, error_code)
	}
}

mod image_data {
	use super::*;
	use crate::common::ScopeGuard;
	use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
	use std::{convert::TryInto, ffi::c_void, io, mem::size_of, ptr::copy_nonoverlapping};
	use windows_sys::Win32::{
		Foundation::HGLOBAL,
		Graphics::Gdi::{
			CreateDIBitmap, DeleteObject, GetDC, GetDIBits, BITMAPINFO, BITMAPINFOHEADER,
			BITMAPV5HEADER, BI_BITFIELDS, BI_RGB, CBM_INIT, DIB_RGB_COLORS, HBITMAP, HDC,
			LCS_GM_IMAGES, RGBQUAD,
		},
		System::{
			DataExchange::SetClipboardData,
			Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND},
			Ole::CF_DIBV5,
		},
	};

	unsafe fn global_unlock_checked(hdata: isize) {
		// If the memory object is unlocked after decrementing the lock count, the function
		// returns zero and GetLastError returns NO_ERROR. If it fails, the return value is
		// zero and GetLastError returns a value other than NO_ERROR.
		if GlobalUnlock(hdata) == 0 {
			let err = io::Error::last_os_error();
			if err.raw_os_error() != Some(0) {
				log::error!("Failed calling GlobalUnlock when writing data: {}", err);
			}
		}
	}

	pub(super) fn add_cf_dibv5(image: ImageRgba) -> Result<(), Error> {
		// This constant is missing in windows-rs
		// https://github.com/microsoft/windows-rs/issues/2711
		#[allow(non_upper_case_globals)]
		const LCS_sRGB: u32 = 0x7352_4742;

		let header_size = size_of::<BITMAPV5HEADER>();
		let header = BITMAPV5HEADER {
			bV5Size: header_size as u32,
			bV5Width: image.width as i32,
			bV5Height: image.height as i32,
			bV5Planes: 1,
			bV5BitCount: 32,
			bV5Compression: BI_BITFIELDS,
			bV5SizeImage: (4 * image.width * image.height) as u32,
			bV5XPelsPerMeter: 0,
			bV5YPelsPerMeter: 0,
			bV5ClrUsed: 0,
			bV5ClrImportant: 0,
			bV5RedMask: 0x00ff0000,
			bV5GreenMask: 0x0000ff00,
			bV5BlueMask: 0x000000ff,
			bV5AlphaMask: 0xff000000,
			bV5CSType: LCS_sRGB,
			// SAFETY: Windows ignores this field because `bV5CSType` is not set to `LCS_CALIBRATED_RGB`.
			bV5Endpoints: unsafe { std::mem::zeroed() },
			bV5GammaRed: 0,
			bV5GammaGreen: 0,
			bV5GammaBlue: 0,
			bV5Intent: LCS_GM_IMAGES as u32, // I'm not sure about this.
			bV5ProfileData: 0,
			bV5ProfileSize: 0,
			bV5Reserved: 0,
		};

		// In theory we don't need to flip the image because we could just specify
		// a negative height in the header, which according to the documentation, indicates that the
		// image rows are in top-to-bottom order. HOWEVER: MS Word (and WordPad) cannot paste an image
		// that has a negative height in its header.
		let image = flip_v(image);

		let data_size = header_size + image.bytes.len();
		let hdata = unsafe { global_alloc(data_size)? };
		unsafe {
			let data_ptr = global_lock(hdata)?;
			let _unlock = ScopeGuard::new(|| global_unlock_checked(hdata));

			copy_nonoverlapping::<u8>((&header) as *const _ as *const u8, data_ptr, header_size);

			// Not using the `add` function, because that has a restriction, that the result cannot overflow isize
			let pixels_dst = (data_ptr as usize + header_size) as *mut u8;
			copy_nonoverlapping::<u8>(image.bytes.as_ptr(), pixels_dst, image.bytes.len());

			let dst_pixels_slice = std::slice::from_raw_parts_mut(pixels_dst, image.bytes.len());

			// If the non-allocating version of the function failed, we need to assign the new bytes to
			// the global allocation.
			if let Cow::Owned(new_pixels) = rgba_to_win(dst_pixels_slice) {
				// SAFETY: `data_ptr` is valid to write to and has no outstanding mutable borrows, and
				// `new_pixels` will be the same length as the original bytes.
				copy_nonoverlapping::<u8>(new_pixels.as_ptr(), data_ptr, new_pixels.len())
			}
		}

		if unsafe { SetClipboardData(CF_DIBV5 as u32, hdata as _) } == 0 {
			unsafe { DeleteObject(hdata as _) };
			Err(last_error("SetClipboardData failed with error"))
		} else {
			Ok(())
		}
	}

	pub(super) fn add_png_file_from_rgba(image: &ImageRgba) -> Result<(), Error> {
		// Try encoding the image as PNG.
		let mut buf = Vec::new();
		let encoder = PngEncoder::new(&mut buf);
		encoder
			.write_image(
				&image.bytes,
				image.width as u32,
				image.height as u32,
				ExtendedColorType::Rgba8,
			)
			.map_err(|e| into_unknown("failed to write png from rgba", e))?;
		add_png_file(&buf)
	}

	pub(super) fn add_png_file(buf: &[u8]) -> Result<(), Error> {
		// Register PNG format.
		let format_id = register_format_(CFSTR_MIME_PNG)?;

		let data_size = buf.len();
		let hdata = unsafe { global_alloc(data_size)? };

		unsafe {
			let pixels_dst = global_lock(hdata)?;
			copy_nonoverlapping::<u8>(buf.as_ptr(), pixels_dst, data_size);
			global_unlock_checked(hdata);
		}

		if unsafe { SetClipboardData(format_id, hdata as _) } == 0 {
			unsafe { DeleteObject(hdata as _) };
			Err(last_error("SetClipboardData failed with error"))
		} else {
			Ok(())
		}
	}

	unsafe fn global_alloc(bytes: usize) -> Result<HGLOBAL, Error> {
		let hdata = GlobalAlloc(GHND, bytes);
		if hdata == 0 {
			Err(last_error("Could not allocate global memory object"))
		} else {
			Ok(hdata)
		}
	}

	unsafe fn global_lock(hmem: HGLOBAL) -> Result<*mut u8, Error> {
		let data_ptr = GlobalLock(hmem) as *mut u8;
		if data_ptr.is_null() {
			Err(last_error("Could not lock the global memory object"))
		} else {
			Ok(data_ptr)
		}
	}

	pub(super) fn read_cf_dibv5(dibv5: &[u8]) -> Result<ImageData<'static>, Error> {
		// The DIBV5 format is a BITMAPV5HEADER followed by the pixel data according to
		// https://docs.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats

		// These constants are missing in windows-rs
		const PROFILE_EMBEDDED: u32 = 0x4D42_4544;
		const PROFILE_LINKED: u32 = 0x4C49_4E4B;

		// so first let's get a pointer to the header
		let header_size = size_of::<BITMAPV5HEADER>();
		if dibv5.len() < header_size {
			return Err(Error::unknown("When reading the DIBV5 data, it contained fewer bytes than the BITMAPV5HEADER size. This is invalid."));
		}
		let header = unsafe { &*(dibv5.as_ptr() as *const BITMAPV5HEADER) };

		let has_profile =
			header.bV5CSType == PROFILE_LINKED || header.bV5CSType == PROFILE_EMBEDDED;

		let pixel_data_start = if has_profile {
			header.bV5ProfileData as isize + header.bV5ProfileSize as isize
		} else {
			header_size as isize
		};

		unsafe {
			let image_bytes = dibv5.as_ptr().offset(pixel_data_start) as *const _;
			let hdc = get_screen_device_context()?;
			let hbitmap = create_bitmap_from_dib(hdc, header as _, image_bytes)?;
			// Now extract the pixels in a desired format
			let w = header.bV5Width;
			let h = header.bV5Height.abs();
			let result_size = w as usize * h as usize * 4;

			let mut result_bytes = Vec::<u8>::with_capacity(result_size);

			let mut output_header = BITMAPINFO {
				bmiColors: [RGBQUAD { rgbRed: 0, rgbGreen: 0, rgbBlue: 0, rgbReserved: 0 }],
				bmiHeader: BITMAPINFOHEADER {
					biSize: size_of::<BITMAPINFOHEADER>() as u32,
					biWidth: w,
					biHeight: -h,
					biBitCount: 32,
					biPlanes: 1,
					biCompression: BI_RGB as u32,
					biSizeImage: 0,
					biXPelsPerMeter: 0,
					biYPelsPerMeter: 0,
					biClrUsed: 0,
					biClrImportant: 0,
				},
			};

			let lines = convert_bitmap_to_rgb(
				hdc,
				hbitmap,
				h as _,
				result_bytes.as_mut_ptr() as _,
				&mut output_header as _,
			)?;
			let read_len = lines as usize * w as usize * 4;
			assert!(
				read_len <= result_bytes.capacity(),
				"Segmentation fault. Read more bytes than allocated to pixel buffer",
			);
			result_bytes.set_len(read_len);

			let mut result_bytes = win_to_rgba(&mut result_bytes);
			repair_missing_alpha(&mut result_bytes, dibv5_alpha_format(header));

			let result = ImageData::rgba(w as _, h as _, Cow::Owned(result_bytes));
			Ok(result)
		}
	}

	#[derive(Clone, Copy, Debug, Eq, PartialEq)]
	enum Dibv5AlphaFormat {
		Present,
		Missing,
		Unknown,
	}

	fn dibv5_alpha_format(header: &BITMAPV5HEADER) -> Dibv5AlphaFormat {
		// Some applications set bV5AlphaMask even with BI_RGB, despite the docs
		// saying the high byte is unused for BI_RGB. Preserve alpha whenever the
		// producer explicitly provides an alpha mask.
		if header.bV5AlphaMask != 0 {
			return Dibv5AlphaFormat::Present;
		}

		match header.bV5Compression {
			BI_RGB | BI_BITFIELDS => Dibv5AlphaFormat::Missing,
			_ => Dibv5AlphaFormat::Unknown,
		}
	}

	fn repair_missing_alpha(bytes: &mut [u8], alpha_format: Dibv5AlphaFormat) -> bool {
		match alpha_format {
			Dibv5AlphaFormat::Present => false,
			Dibv5AlphaFormat::Missing => set_alpha_opaque(bytes),
			Dibv5AlphaFormat::Unknown => repair_missing_alpha_if_opaque_rgb(bytes),
		}
	}

	fn set_alpha_opaque(bytes: &mut [u8]) -> bool {
		debug_assert_eq!(bytes.len() % 4, 0);

		let mut changed = false;
		for pixel in bytes.chunks_exact_mut(4) {
			changed |= pixel[3] != 255;
			pixel[3] = 255;
		}

		changed
	}

	/// Some Windows screenshot tools put 32-bit DIB data on the clipboard with
	/// the alpha byte left as zero for every pixel even though the RGB channels
	/// contain the visible screenshot. Use this only when the DIBV5 header does
	/// not tell us whether the alpha channel is present.
	fn repair_missing_alpha_if_opaque_rgb(bytes: &mut [u8]) -> bool {
		debug_assert_eq!(bytes.len() % 4, 0);

		let mut has_rgb_content = false;
		let mut has_alpha_content = false;

		for pixel in bytes.chunks_exact(4) {
			has_rgb_content |= pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0;
			has_alpha_content |= pixel[3] != 0;

			if has_rgb_content && has_alpha_content {
				return false;
			}
		}

		if has_rgb_content && !has_alpha_content {
			for pixel in bytes.chunks_exact_mut(4) {
				pixel[3] = 255;
			}
			true
		} else {
			false
		}
	}

	fn get_screen_device_context() -> Result<HDC, Error> {
		// SAFETY: Calling `GetDC` with `NULL` is safe.
		let hdc = unsafe { GetDC(0) };
		if hdc == 0 {
			Err(Error::unknown("Failed to get the device context. GetDC returned null"))
		} else {
			Ok(hdc)
		}
	}

	unsafe fn create_bitmap_from_dib(
		hdc: HDC,
		header: *const BITMAPV5HEADER,
		image_bytes: *const c_void,
	) -> Result<HBITMAP, Error> {
		let hbitmap = CreateDIBitmap(
			hdc,
			header as _,
			CBM_INIT as u32,
			image_bytes,
			header as _,
			DIB_RGB_COLORS,
		);
		if hbitmap == 0 {
			Err(Error::unknown(
				"Failed to create the HBITMAP while reading DIBV5. CreateDIBitmap returned null",
			))
		} else {
			Ok(hbitmap)
		}
	}

	/// Copies the bitmap image into given buffer with DIB RGB format and
	/// returns the number of scan lines copied from the bitmap.
	unsafe fn convert_bitmap_to_rgb(
		hdc: HDC,
		hbitmap: HBITMAP,
		lines: u32,
		dst: *mut c_void,
		header: *mut BITMAPINFO,
	) -> Result<i32, Error> {
		let lines = GetDIBits(hdc, hbitmap, 0, lines, dst, header, DIB_RGB_COLORS);
		if lines == 0 {
			Err(Error::unknown("Could not get the bitmap bits, GetDIBits returned 0"))
		} else {
			Ok(lines)
		}
	}

	/// Converts the RGBA (u8) pixel data into the bitmap-native ARGB (u32)
	/// format in-place.
	///
	/// Safety: the `bytes` slice must have a length that's a multiple of 4
	#[allow(clippy::identity_op, clippy::erasing_op)]
	#[must_use]
	pub(super) unsafe fn rgba_to_win(bytes: &mut [u8]) -> Cow<'_, [u8]> {
		// Check safety invariants to catch obvious bugs.
		debug_assert_eq!(bytes.len() % 4, 0);

		let mut u32pixels_buffer = convert_bytes_to_u32s(bytes);
		let u32pixels = match u32pixels_buffer {
			ImageDataCow::Borrowed(ref mut b) => b,
			ImageDataCow::Owned(ref mut b) => b.as_mut_slice(),
		};

		for p in u32pixels.iter_mut() {
			let [mut r, mut g, mut b, mut a] = p.to_ne_bytes().map(u32::from);
			r <<= 2 * 8;
			g <<= 1 * 8;
			b <<= 0 * 8;
			a <<= 3 * 8;

			*p = r | g | b | a;
		}

		match u32pixels_buffer {
			ImageDataCow::Borrowed(_) => Cow::Borrowed(bytes),
			ImageDataCow::Owned(bytes) => {
				Cow::Owned(bytes.into_iter().flat_map(|b| b.to_ne_bytes()).collect())
			}
		}
	}

	/// Vertically flips the image pixels in memory
	fn flip_v(image: ImageRgba) -> ImageRgba<'static> {
		let w = image.width;
		let h = image.height;

		let mut bytes = image.bytes.into_owned();

		let rowsize = w * 4; // each pixel is 4 bytes
		let mut tmp_a = vec![0; rowsize];
		// I believe this could be done safely with `as_chunks_mut`, but that's not stable yet
		for a_row_id in 0..(h / 2) {
			let b_row_id = h - a_row_id - 1;

			// swap rows `first_id` and `second_id`
			let a_byte_start = a_row_id * rowsize;
			let a_byte_end = a_byte_start + rowsize;
			let b_byte_start = b_row_id * rowsize;
			let b_byte_end = b_byte_start + rowsize;
			tmp_a.copy_from_slice(&bytes[a_byte_start..a_byte_end]);
			bytes.copy_within(b_byte_start..b_byte_end, a_byte_start);
			bytes[b_byte_start..b_byte_end].copy_from_slice(&tmp_a);
		}

		ImageRgba { width: image.width, height: image.height, bytes: bytes.into() }
	}

	/// Converts the ARGB (u32) pixel data into the RGBA (u8) format in-place
	///
	/// Safety: the `bytes` slice must have a length that's a multiple of 4
	#[allow(clippy::identity_op, clippy::erasing_op)]
	#[must_use]
	pub(super) unsafe fn win_to_rgba(bytes: &mut [u8]) -> Vec<u8> {
		// Check safety invariants to catch obvious bugs.
		debug_assert_eq!(bytes.len() % 4, 0);

		let mut u32pixels_buffer = convert_bytes_to_u32s(bytes);
		let u32pixels = match u32pixels_buffer {
			ImageDataCow::Borrowed(ref mut b) => b,
			ImageDataCow::Owned(ref mut b) => b.as_mut_slice(),
		};

		for p in u32pixels {
			let mut bytes = p.to_ne_bytes();
			bytes[0] = (*p >> (2 * 8)) as u8;
			bytes[1] = (*p >> (1 * 8)) as u8;
			bytes[2] = (*p >> (0 * 8)) as u8;
			bytes[3] = (*p >> (3 * 8)) as u8;
			*p = u32::from_ne_bytes(bytes);
		}

		match u32pixels_buffer {
			ImageDataCow::Borrowed(_) => bytes.to_vec(),
			ImageDataCow::Owned(bytes) => bytes.into_iter().flat_map(|b| b.to_ne_bytes()).collect(),
		}
	}

	// XXX: std's Cow is not usable here because it does not allow mutably
	// borrowing data.
	enum ImageDataCow<'a> {
		Borrowed(&'a mut [u32]),
		Owned(Vec<u32>),
	}

	/// Safety: the `bytes` slice must have a length that's a multiple of 4
	unsafe fn convert_bytes_to_u32s(bytes: &mut [u8]) -> ImageDataCow<'_> {
		// When the correct conditions are upheld, `std` should return everything in the well-aligned slice.
		let (prefix, _, suffix) = bytes.align_to::<u32>();

		// Check if `align_to` gave us the optimal result.
		//
		// If it didn't, use the slow path with more allocations
		if prefix.is_empty() && suffix.is_empty() {
			// We know that the newly-aligned slice will contain all the values
			ImageDataCow::Borrowed(bytes.align_to_mut::<u32>().1)
		} else {
			// XXX: Use `as_chunks` when it stabilizes.
			let u32pixels_buffer = bytes
				.chunks(4)
				.map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
				.collect();
			ImageDataCow::Owned(u32pixels_buffer)
		}
	}
}

/// A shim clipboard type that can have operations performed with it, but
/// does not represent an open clipboard itself.
///
/// Windows only allows one thread on the entire system to have the clipboard
/// open at once, so we have to open it very sparingly or risk causing the rest
/// of the system to be unresponsive. Instead, the clipboard is opened for
/// every operation and then closed afterwards.
pub(crate) struct Clipboard(());

// The other platforms have `Drop` implementation on their
// clipboard, so Windows should too for consistently.
impl Drop for Clipboard {
	fn drop(&mut self) {}
}

struct OpenClipboard<'clipboard> {
	_inner: clipboard_win::Clipboard,
	// The Windows clipboard can not be sent between threads once
	// open.
	_marker: PhantomData<*const ()>,
	_for_shim: &'clipboard mut Clipboard,
}

impl Clipboard {
	const DEFAULT_OPEN_ATTEMPTS: usize = 5;

	pub(crate) fn new() -> Result<Self, Error> {
		Ok(Self(()))
	}

	fn open(&mut self) -> Result<OpenClipboard, Error> {
		// Attempt to open the clipboard multiple times. On Windows, its common for something else to temporarily
		// be using it during attempts.
		//
		// For past work/evidence, see Firefox(https://searchfox.org/mozilla-central/source/widget/windows/nsClipboard.cpp#421) and
		// Chromium(https://source.chromium.org/chromium/chromium/src/+/main:ui/base/clipboard/clipboard_win.cc;l=86).
		//
		// Note: This does not use `Clipboard::new_attempts` because its implementation sleeps for `0ms`, which can
		// cause race conditions between closing/opening the clipboard in single-threaded apps.
		let mut attempts = Self::DEFAULT_OPEN_ATTEMPTS;
		let clipboard = loop {
			match clipboard_win::Clipboard::new() {
				Ok(this) => break Ok(this),
				Err(err) => match attempts {
					0 => break Err(err),
					_ => attempts -= 1,
				},
			}

			// The default value matches Chromium's implementation, but could be tweaked later.
			thread::sleep(Duration::from_millis(5));
		}
		.map_err(|_| Error::ClipboardOccupied)?;

		Ok(OpenClipboard { _inner: clipboard, _marker: PhantomData, _for_shim: self })
	}
}

// Note: In all of the builders, a clipboard opening result is stored.
// This is done for a few reasons:
// 1. consistently with the other platforms which can have an occupied clipboard.
// 	It is better if the operation fails at the most similar place on all platforms.
// 2. `{Get, Set, Clear}::new()` don't return a `Result`. Windows is the only case that
// 	needs this kind of handling, so it doesn't need to affect the other APIs.
// 3. Due to how the clipboard works on Windows, we need to open it for every operation
// and keep it open until its finished. This approach allows RAII to still be applicable.

pub(crate) struct Get<'clipboard> {
	clipboard: Result<OpenClipboard<'clipboard>, Error>,
}

impl<'clipboard> Get<'clipboard> {
	pub(crate) fn new(clipboard: &'clipboard mut Clipboard) -> Self {
		Self { clipboard: clipboard.open() }
	}

	pub(crate) fn text(self) -> Result<String, Error> {
		let _clipboard_assertion = self.clipboard?;
		Self::text_()
	}

	fn text_() -> Result<String, Error> {
		const FORMAT: u32 = clipboard_win::formats::CF_UNICODETEXT;

		// XXX: ToC/ToU race conditions are not possible because we are the sole owners of the clipboard currently.
		if !clipboard_win::is_format_avail(FORMAT) {
			return Err(Error::ContentNotAvailable);
		}

		let Some(text_size) = clipboard_win::raw::size(FORMAT) else {
			return Err(last_error("failed to read clipboard text size"));
		};

		// Allocate the specific number of WTF-16 characters we need to receive.
		// This division is always accurate because Windows uses 16-bit characters.
		let mut out: Vec<u16> = vec![0u16; text_size.get() / 2];

		let bytes_read = {
			// SAFETY: The source slice has a greater alignment than the resulting one.
			let out: &mut [u8] =
				unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr().cast(), out.len() * 2) };

			let mut bytes_read = clipboard_win::raw::get(FORMAT, out)
				.map_err(|e| map_error_code("failed to read clipboard string", e))?;

			// Convert the number of bytes read to the number of `u16`s
			bytes_read /= 2;

			// Remove the NUL terminator, if it existed.
			if let Some(last) = out.last().copied() {
				if last == 0 {
					bytes_read -= 1;
				}
			}

			bytes_read
		};

		// Create a UTF-8 string from WTF-16 data, if it was valid.
		String::from_utf16(&out[..bytes_read])
			.map_err(|e| into_unknown("failed to convert string from utf16", e))
	}

	pub(crate) fn rtf(self) -> Result<String, Error> {
		let _clipboard_assertion = self.clipboard?;
		Self::rtf_()
	}

	#[inline]
	fn rtf_() -> Result<String, Error> {
		let format = register_format_(CFSTR_MIME_RICHTEXT)?;

		// XXX: ToC/ToU race conditions are not possible because we are the sole owners of the clipboard currently.
		if !clipboard_win::is_format_avail(format) {
			return Err(Error::ContentNotAvailable);
		}

		let mut data = Vec::new();
		clipboard_win::raw::get_vec(format, &mut data)
			.map_err(|e| map_error_code("failed to read clipboard image data", e))?;
		Ok(String::from_utf8_lossy(&data).into_owned())
	}

	pub(crate) fn html(self) -> Result<String, Error> {
		let _clipboard_assertion = self.clipboard?;
		Self::html_()
	}

	fn html_() -> Result<String, Error> {
		match Html::new() {
			Some(h) => {
				let mut out = Vec::new();
				match h.read_clipboard(&mut out) {
					Ok(_s) => {}
					Err(e) => {
						return Err(map_error_code("failed to read clipboard html", e));
					}
				}
				String::from_utf8(out)
					.map_err(|e| into_unknown("failed to write html from utf8", e))
			}
			None => Err(Error::ContentNotAvailable),
		}
	}

	pub(crate) fn image(self) -> Result<ImageData<'static>, Error> {
		let _clipboard_assertion = self.clipboard?;
		Self::image_()
	}

	fn image_() -> Result<ImageData<'static>, Error> {
		match Self::image_svg() {
			Err(Error::ContentNotAvailable) => match Self::image_png() {
				Err(Error::ContentNotAvailable) => Self::image_dibv5(),
				result => result,
			},
			result => result,
		}
	}

	fn image_dibv5() -> Result<ImageData<'static>, Error> {
		const FORMAT: u32 = clipboard_win::formats::CF_DIBV5;

		if !clipboard_win::is_format_avail(FORMAT) {
			return Err(Error::ContentNotAvailable);
		}

		let mut data = Vec::new();

		clipboard_win::raw::get_vec(FORMAT, &mut data)
			.map_err(|e| map_error_code("failed to read clipboard image data", e))?;

		image_data::read_cf_dibv5(&data)
	}

	fn image_png() -> Result<ImageData<'static>, Error> {
		let format = register_format_(CFSTR_MIME_PNG)?;
		if !clipboard_win::is_format_avail(format) {
			return Err(Error::ContentNotAvailable);
		}

		let mut data = Vec::new();
		clipboard_win::raw::get_vec(format, &mut data)
			.map_err(|e| map_error_code("failed to read clipboard image data", e))?;
		Ok(ImageData::png(data.into()))
	}

	fn image_svg() -> Result<ImageData<'static>, Error> {
		let format = register_format_(CFSTR_MIME_SVG_XML)?;
		if !clipboard_win::is_format_avail(format) {
			return Err(Error::ContentNotAvailable);
		}

		let mut data = Vec::new();
		clipboard_win::raw::get_vec(format, &mut data)
			.map_err(|e| map_error_code("failed to read clipboard image data", e))?;
		Ok(ImageData::Svg(String::from_utf8_lossy(&data).into_owned()))
	}

	pub(crate) fn special(self, format_name: &str) -> Result<Vec<u8>, Error> {
		let _clipboard_assertion = self.clipboard?;
		Self::special_(format_name)
	}

	fn special_(format_name: &str) -> Result<Vec<u8>, Error> {
		let format = register_format_(format_name)?;
		if !clipboard_win::is_format_avail(format) {
			return Err(Error::ContentNotAvailable);
		}

		let mut data = Vec::new();
		clipboard_win::raw::get_vec(format, &mut data)
			.map_err(|e| map_error_code("failed to read clipboard data", e))?;
		Ok(data)
	}

	pub(crate) fn formats(self, formats: &[ClipboardFormat]) -> Result<Vec<ClipboardData>, Error> {
		let _clipboard_assertion = self.clipboard?;

		let mut results = Vec::new();
		let mut err = None;
		let mut err_count = 0;
		for format in formats.iter() {
			let mut cur_err = None;
			match format {
				ClipboardFormat::Text => match Self::text_() {
					Ok(text) => {
						results.push(ClipboardData::Text(text));
					}
					Err(Error::ContentNotAvailable) => {
						results.push(ClipboardData::None);
					}
					Err(e) => {
						log::debug!("Error reading text from clipboard: {}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::Rtf => match Self::rtf_() {
					Ok(rtf) => results.push(ClipboardData::Rtf(rtf)),
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading RTF from clipboard: {}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::Html => match Self::html_() {
					Ok(html) => results.push(ClipboardData::Html(html)),
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading HTML from clipboard, {:?}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::ImageRgba => match Self::image_dibv5() {
					Ok(image) => results.push(ClipboardData::Image(image)),
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading image from clipboard, {:?}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::ImagePng => match Self::image_png() {
					Ok(image) => results.push(ClipboardData::Image(image)),
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading PNG from clipboard, {:?}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::ImageSvg => match Self::image_svg() {
					Ok(image) => results.push(ClipboardData::Image(image)),
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading SVG from clipboard, {:?}", e);
						cur_err = Some(e);
					}
				},
				ClipboardFormat::Special(format_name) => match Self::special_(format_name) {
					Ok(data) => {
						results.push(ClipboardData::Special((format_name.to_string(), data)))
					}
					Err(Error::ContentNotAvailable) => results.push(ClipboardData::None),
					Err(e) => {
						log::debug!("Error reading special format from clipboard, {:?}", e);
						cur_err = Some(e);
					}
				},
			}
			if let Some(e) = cur_err {
				match e {
					Error::ClipboardOccupied => {
						// If the clipboard is occupied, we need to stop trying to read from it.
						return Err(e);
					}
					_ => {
						results.push(ClipboardData::None);
						err = Some(e);
						err_count += 1
					}
				}
			}
		}

		if err_count == formats.len() {
			if let Some(e) = err {
				Err(e)
			} else {
				// unreachable!() because `err_count == formats.len()`
				Ok(results)
			}
		} else {
			Ok(results)
		}
	}
}

pub(crate) struct Set<'clipboard> {
	clipboard: Result<OpenClipboard<'clipboard>, Error>,
	exclude_from_monitoring: bool,
	exclude_from_cloud: bool,
	exclude_from_history: bool,
}

impl<'clipboard> Set<'clipboard> {
	pub(crate) fn new(clipboard: &'clipboard mut Clipboard) -> Self {
		Self {
			clipboard: clipboard.open(),
			exclude_from_monitoring: false,
			exclude_from_cloud: false,
			exclude_from_history: false,
		}
	}

	pub(crate) fn text(self, data: Cow<'_, str>) -> Result<(), Error> {
		let open_clipboard = self.clipboard?;
		Self::text_(data, true)?;
		add_clipboard_exclusions(
			open_clipboard,
			self.exclude_from_monitoring,
			self.exclude_from_cloud,
			self.exclude_from_history,
		)
	}

	fn text_(data: Cow<'_, str>, clear: bool) -> Result<(), Error> {
		if clear {
			clipboard_win::raw::set_string(&data)
		} else {
			clipboard_win::raw::set_string_with(&data, options::NoClear)
		}
		.map_err(|e| map_error_code("Could not place the specified text to the clipboard", e))
	}

	pub(crate) fn rtf(self, data: Cow<'_, str>) -> Result<(), Error> {
		let open_clipboard = self.clipboard?;
		if let Err(e) = clipboard_win::raw::empty() {
			return Err(map_error_code("Failed to empty the clipboard. Got error code: {:?}", e));
		};

		Self::rtf_(data)?;
		add_clipboard_exclusions(
			open_clipboard,
			self.exclude_from_monitoring,
			self.exclude_from_cloud,
			self.exclude_from_history,
		)
	}

	#[inline]
	fn rtf_(data: Cow<'_, str>) -> Result<(), Error> {
		let format = register_format_(CFSTR_MIME_RICHTEXT)?;
		clipboard_win::raw::set_without_clear(format, data.as_bytes())
			.map_err(|e| map_error_code("failed to set without clear", e))
	}

	pub(crate) fn html(self, html: Cow<'_, str>, alt: Option<Cow<'_, str>>) -> Result<(), Error> {
		let open_clipboard = self.clipboard?;
		Self::html_(html, alt, true)?;
		add_clipboard_exclusions(
			open_clipboard,
			self.exclude_from_monitoring,
			self.exclude_from_cloud,
			self.exclude_from_history,
		)
	}

	fn html_(html: Cow<'_, str>, alt: Option<Cow<'_, str>>, clear: bool) -> Result<(), Error> {
		if clear {
			if let Err(e) = clipboard_win::raw::empty() {
				return Err(map_error_code("Failed to empty the clipboard, {:?}", e));
			};
		}

		let alt = match alt {
			Some(s) => s.into(),
			None => String::new(),
		};
		clipboard_win::raw::set_string_with(&alt, options::NoClear).map_err(|e| {
			map_error_code("Could not place the specified text to the clipboard", e)
		})?;

		Self::html_without_alt_(html)
	}

	#[inline]
	fn html_without_alt_(html: Cow<'_, str>) -> Result<(), Error> {
		let format = register_format_("HTML Format")?;
		let html = wrap_html(&html);
		clipboard_win::raw::set_without_clear(format, html.as_bytes())
			.map_err(|e| map_error_code("failed to set without clear", e))
	}

	pub(crate) fn image(self, image: ImageData) -> Result<(), Error> {
		let _open_clipboard = self.clipboard?;
		if let Err(e) = clipboard_win::raw::empty() {
			return Err(map_error_code("Failed to empty the clipboard", e));
		};
		Self::image_(image)
	}

	fn image_(image: ImageData) -> Result<(), Error> {
		match image {
			ImageData::Rgba(image) => Self::image_rgba(image),
			ImageData::Png(png) => image_data::add_png_file(&png),
			ImageData::Svg(svg) => Self::image_svg(svg),
		}
	}

	#[inline]
	fn image_rgba(image: ImageRgba) -> Result<(), Error> {
		// XXX: The ordering of these functions is important, as some programs will grab the
		// first format available. PNGs tend to have better compatibility on Windows, so it is set first.
		image_data::add_png_file_from_rgba(&image)?;
		image_data::add_cf_dibv5(image)
	}

	#[inline]
	fn image_svg(svg: String) -> Result<(), Error> {
		let format = register_format_(CFSTR_MIME_SVG_XML)?;
		clipboard_win::raw::set_without_clear(format, svg.as_bytes())
			.map_err(|e| map_error_code("Failed to set SVG data to clipboard", e))
	}

	pub(crate) fn special(self, format_name: &str, data: &[u8]) -> Result<(), Error> {
		let open_clipboard = self.clipboard?;
		if let Err(e) = clipboard_win::raw::empty() {
			return Err(map_error_code("Failed to empty the clipboard", e));
		};
		Self::special_(format_name, data)?;
		add_clipboard_exclusions(
			open_clipboard,
			self.exclude_from_monitoring,
			self.exclude_from_cloud,
			self.exclude_from_history,
		)
	}

	#[inline]
	fn special_(format_name: &str, data: &[u8]) -> Result<(), Error> {
		let format = register_format_(format_name)?;
		clipboard_win::raw::set_without_clear(format, data)
			.map_err(|e| map_error_code("failed to set clipboard data", e))
	}

	// No need to implement checking `ClipboardOccupied` as the `Get` does.
	// Because it's a common case that `Get`s are called consecutively in multiple threads or processes.
	pub(crate) fn formats(self, data: &[ClipboardData]) -> Result<(), Error> {
		let open_clipboard = self.clipboard?;

		if let Err(e) = clipboard_win::raw::empty() {
			return Err(map_error_code("Failed to empty the clipboard", e));
		};

		for item in data.iter() {
			match item {
				ClipboardData::Text(text) => {
					Self::text_(text.clone().into(), false)?;
				}
				ClipboardData::Rtf(rtf) => {
					Self::rtf_(rtf.clone().into())?;
				}
				ClipboardData::Html(html) => {
					Self::html_without_alt_(html.clone().into())?;
				}
				ClipboardData::Image(image) => {
					Self::image_(image.clone())?;
				}
				ClipboardData::Special((format_name, data)) => {
					let format_name = format_name.as_str();
					Self::special_(format_name, data)?;
				}
				_ => {}
			}
		}

		add_clipboard_exclusions(
			open_clipboard,
			self.exclude_from_monitoring,
			self.exclude_from_cloud,
			self.exclude_from_history,
		)
	}
}

fn add_clipboard_exclusions(
	_open_clipboard: OpenClipboard<'_>,
	exclude_from_monitoring: bool,
	exclude_from_cloud: bool,
	exclude_from_history: bool,
) -> Result<(), Error> {
	/// `set` should be called with the registered format and a DWORD value of 0.
	///
	/// See https://docs.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats#cloud-clipboard-and-clipboard-history-formats
	const CLIPBOARD_EXCLUSION_DATA: &[u8] = &0u32.to_ne_bytes();

	// Clipboard exclusions are applied retroactively (we still have the clipboard lock) to the item that is currently in the clipboard.
	// See the MS docs on `CLIPBOARD_EXCLUSION_DATA` for specifics. Once the item is added to the clipboard,
	// tell Windows to remove it from cloud syncing and history.

	if exclude_from_monitoring {
		if let Some(format) =
			clipboard_win::register_format("ExcludeClipboardContentFromMonitorProcessing")
		{
			// The documentation states "place any data on the clipboard in this format to prevent...", and using the zero bytes
			// like the others for consistency works.
			clipboard_win::raw::set_without_clear(format.get(), CLIPBOARD_EXCLUSION_DATA).map_err(
				|e| map_error_code("failed to exclude data from clipboard monitoring", e),
			)?;
		}
	}

	if exclude_from_cloud {
		if let Some(format) = clipboard_win::register_format("CanUploadToCloudClipboard") {
			// We believe that it would be a logic error if this call failed, since we've validated the format is supported,
			// we still have full ownership of the clipboard and aren't moving it to another thread, and this is a well-documented operation.
			// Due to these reasons, `Error::Unknown` is used because we never expect the error path to be taken.
			clipboard_win::raw::set_without_clear(format.get(), CLIPBOARD_EXCLUSION_DATA)
				.map_err(|e| map_error_code("failed to exclude data from cloud clipboard", e))?;
		}
	}

	if exclude_from_history {
		if let Some(format) = clipboard_win::register_format("CanIncludeInClipboardHistory") {
			// See above for reasoning about using `Error::Unknown`.
			clipboard_win::raw::set_without_clear(format.get(), CLIPBOARD_EXCLUSION_DATA)
				.map_err(|e| map_error_code("failed to exclude data from clipboard history", e))?;
		}
	}

	Ok(())
}

/// Windows-specific extensions to the [`Set`](crate::Set) builder.
pub trait SetExtWindows: private::Sealed {
	/// Exclude the data which will be set on the clipboard from being processed
	/// at all, either in the local clipboard history or getting uploaded to the cloud.
	///
	/// If this is set, it is not recommended to call [exclude_from_cloud](SetExtWindows::exclude_from_cloud) or [exclude_from_history](SetExtWindows::exclude_from_history).
	fn exclude_from_monitoring(self) -> Self;

	/// Excludes the data which will be set on the clipboard from being uploaded to
	/// the Windows 10/11 [cloud clipboard].
	///
	/// [cloud clipboard]: https://support.microsoft.com/en-us/windows/clipboard-in-windows-c436501e-985d-1c8d-97ea-fe46ddf338c6
	fn exclude_from_cloud(self) -> Self;

	/// Excludes the data which will be set on the clipboard from being added to
	/// the system's [clipboard history] list.
	///
	/// [clipboard history]: https://support.microsoft.com/en-us/windows/get-help-with-clipboard-30375039-ce71-9fe4-5b30-21b7aab6b13f
	fn exclude_from_history(self) -> Self;
}

impl SetExtWindows for crate::Set<'_> {
	fn exclude_from_monitoring(mut self) -> Self {
		self.platform.exclude_from_monitoring = true;
		self
	}

	fn exclude_from_cloud(mut self) -> Self {
		self.platform.exclude_from_cloud = true;
		self
	}

	fn exclude_from_history(mut self) -> Self {
		self.platform.exclude_from_history = true;
		self
	}
}

pub(crate) struct Clear<'clipboard> {
	clipboard: Result<OpenClipboard<'clipboard>, Error>,
}

impl<'clipboard> Clear<'clipboard> {
	pub(crate) fn new(clipboard: &'clipboard mut Clipboard) -> Self {
		Self { clipboard: clipboard.open() }
	}

	pub(crate) fn clear(self) -> Result<(), Error> {
		let _clipboard_assertion = self.clipboard?;
		clipboard_win::empty().map_err(|e| map_error_code("failed to clear clipboard", e))
	}
}

#[inline]
fn register_format_(name: &str) -> Result<u32, Error> {
	Ok(clipboard_win::register_format(name)
		.ok_or_else(|| Error::unknown(format!("failed to register clipboard format \"{}\"", name)))?
		.get())
}

fn wrap_html(ctn: &str) -> String {
	let h_version = "Version:0.9";
	let h_start_html = "\r\nStartHTML:";
	let h_end_html = "\r\nEndHTML:";
	let h_start_frag = "\r\nStartFragment:";
	let h_end_frag = "\r\nEndFragment:";
	let c_start_frag = "\r\n<html>\r\n<body>\r\n<!--StartFragment-->\r\n";
	let c_end_frag = "\r\n<!--EndFragment-->\r\n</body>\r\n</html>";
	let h_len = h_version.len()
		+ h_start_html.len()
		+ 10 + h_end_html.len()
		+ 10 + h_start_frag.len()
		+ 10 + h_end_frag.len()
		+ 10;
	let n_start_html = h_len + 2;
	let n_start_frag = h_len + c_start_frag.len();
	let n_end_frag = n_start_frag + ctn.len();
	let n_end_html = n_end_frag + c_end_frag.len();
	format!(
		"{}{}{:010}{}{:010}{}{:010}{}{:010}{}{}{}",
		h_version,
		h_start_html,
		n_start_html,
		h_end_html,
		n_end_html,
		h_start_frag,
		n_start_frag,
		h_end_frag,
		n_end_frag,
		c_start_frag,
		ctn,
		c_end_frag,
	)
}

#[cfg(test)]
mod tests {
	use super::image_data::{read_cf_dibv5, rgba_to_win, win_to_rgba};
	use crate::common::ImageData;
	use std::mem::size_of;
	use windows_sys::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_BITFIELDS, BI_RGB, LCS_GM_IMAGES};

	#[test]
	fn conversion_between_win_and_rgba() {
		const DATA: [u8; 16] =
			[100, 100, 255, 100, 0, 0, 0, 255, 255, 100, 100, 255, 100, 255, 100, 100];

		let mut data = DATA;
		let _converted = unsafe { win_to_rgba(&mut data) };

		let mut data = DATA;
		let _converted = unsafe { rgba_to_win(&mut data) };

		let mut data = DATA;
		let _converted = unsafe { win_to_rgba(&mut data) };
		let _converted = unsafe { rgba_to_win(&mut data) };
		assert_eq!(data, DATA);

		let mut data = DATA;
		let _converted = unsafe { rgba_to_win(&mut data) };
		let _converted = unsafe { win_to_rgba(&mut data) };
		assert_eq!(data, DATA);
	}

	fn dibv5_test_data(
		width: usize,
		height: usize,
		compression: i32,
		alpha_mask: u32,
		pixels: &[u8],
	) -> Vec<u8> {
		let header = BITMAPV5HEADER {
			bV5Size: size_of::<BITMAPV5HEADER>() as u32,
			bV5Width: width as i32,
			bV5Height: height as i32,
			bV5Planes: 1,
			bV5BitCount: 32,
			bV5Compression: compression,
			bV5SizeImage: pixels.len() as u32,
			bV5XPelsPerMeter: 0,
			bV5YPelsPerMeter: 0,
			bV5ClrUsed: 0,
			bV5ClrImportant: 0,
			bV5RedMask: if compression == BI_BITFIELDS { 0x00ff0000 } else { 0 },
			bV5GreenMask: if compression == BI_BITFIELDS { 0x0000ff00 } else { 0 },
			bV5BlueMask: if compression == BI_BITFIELDS { 0x000000ff } else { 0 },
			bV5AlphaMask: alpha_mask,
			bV5CSType: 0,
			// SAFETY: Windows ignores this field because `bV5CSType` is not set to `LCS_CALIBRATED_RGB`.
			bV5Endpoints: unsafe { std::mem::zeroed() },
			bV5GammaRed: 0,
			bV5GammaGreen: 0,
			bV5GammaBlue: 0,
			bV5Intent: LCS_GM_IMAGES as u32,
			bV5ProfileData: 0,
			bV5ProfileSize: 0,
			bV5Reserved: 0,
		};

		let header_bytes = unsafe {
			std::slice::from_raw_parts(
				(&header as *const BITMAPV5HEADER) as *const u8,
				size_of::<BITMAPV5HEADER>(),
			)
		};

		let mut data = Vec::with_capacity(header_bytes.len() + pixels.len());
		data.extend_from_slice(header_bytes);
		data.extend_from_slice(pixels);
		data
	}

	#[test]
	fn read_cf_dibv5_repairs_all_black_missing_alpha() {
		let data = dibv5_test_data(2, 1, BI_RGB, 0, &[0, 0, 0, 0, 0, 0, 0, 0]);
		let ImageData::Rgba(image) = read_cf_dibv5(&data).unwrap() else {
			panic!("expected RGBA image");
		};

		assert_eq!(image.width, 2);
		assert_eq!(image.height, 1);
		assert_eq!(image.bytes.as_ref(), &[0, 0, 0, 255, 0, 0, 0, 255]);
	}

	#[test]
	fn read_cf_dibv5_preserves_declared_transparency() {
		let data = dibv5_test_data(2, 1, BI_BITFIELDS, 0xff000000, &[0, 0, 255, 0, 0, 255, 0, 0]);
		let ImageData::Rgba(image) = read_cf_dibv5(&data).unwrap() else {
			panic!("expected RGBA image");
		};

		assert_eq!(image.width, 2);
		assert_eq!(image.height, 1);
		assert_eq!(image.bytes.as_ref(), &[255, 0, 0, 0, 0, 255, 0, 0]);
	}
}
