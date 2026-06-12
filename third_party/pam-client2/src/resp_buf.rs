//! Helper module for safe building of PAM responses

/***********************************************************************
 * (c) 2021-2022 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>*
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

use crate::error::ErrorCode;
use crate::Result;

use crate::c_box::CBox;
use libc::{free, strdup};
use pam_sys::pam_response as PamResponse;
use std::ffi::CString;
use std::{mem, ptr, slice};

/// Reasonably safe fixed-length buffer for PAM conversation responses.
///
/// Allocates memory that can be freed by C code and, if the responsibility
/// isn't passed (by using `Into<*mut PamResponse>`), deallocates it when it
/// is dropped.
#[derive(Debug)]
pub(crate) struct ResponseBuffer {
	items: CBox<[PamResponse]>,
}

impl ResponseBuffer {
	/// Creates a new buffer for `len` PAM conversation responses.
	///
	/// Fails with `Err(BUF_ERR)` if
	/// 1. `len` is non-positive,
	/// 2. `len` is so large, the total allocated memory would exceed [`isize::MAX`],
	/// 3. the memory could not be allocated.
	pub fn new(len: isize) -> Result<Self> {
		match len {
			1..=isize::MAX => {
				#[allow(clippy::cast_sign_loss)]
				let buffer = CBox::<PamResponse>::try_new_zeroed_slice(len as usize)?;
				Ok(Self {
					items: unsafe { buffer.assume_all_init() },
				})
			}
			_ => Err(ErrorCode::BUF_ERR.into()),
		}
	}

	/// Returns the number of elements in the buffer.
	#[allow(unused)]
	pub fn len(&self) -> usize {
		self.items.len()
	}

	/// Returns `false` if the buffer was constructed with a `len` > 0.
	#[allow(unused)]
	pub fn is_empty(&self) -> bool {
		self.items.len() == 0
	}

	/// Iterates over all contained `PamResponse` items.
	#[inline]
	#[allow(unused)]
	pub fn iter(&self) -> slice::Iter<'_, PamResponse> {
		self.into_iter()
	}

	/// Puts a response at the specified index slot.
	///
	/// If the slot was already filled, the previous response will be lost.
	///
	/// # Panics
	/// Panics if the index is out of range.
	#[inline]
	pub fn put(&mut self, index: usize, response: Option<CString>) {
		assert!(index < self.items.len());
		// Sound because of the bounds check above and because zeroed memory
		// is a valid representation for the contained structs.
		let dest = &mut self.items[index];

		// Free the old string if there was already one in this slot
		if !dest.resp.is_null() {
			unsafe { free(dest.resp.cast()) };
		}

		// Copy the string into the struct, so that the resulting pointer can
		// be deallocated with `free()`. The use of `strdup` should be sound
		// here, because `CString::as_ptr()` guarantees to point to a valid
		// NULL-terminated string.
		*dest = match response {
			Some(text) => PamResponse {
				resp: unsafe { strdup(text.as_ptr()) },
				resp_retcode: 0,
			},
			None => PamResponse {
				resp: ptr::null_mut(),
				resp_retcode: 0,
			},
		}
	}

	/// Puts a binary response at the specified index slot. (Linux specific, experimental)
	///
	/// If the slot was already filled, the previous response will be lost.
	///
	/// The data is kept in a pseudo-struct `{length: u32, type: u8, data: [u8]}`
	/// in network byte order.
	///
	/// # Panics
	/// Panics if the index is out of range, memory could not be allocated
	/// or the length of the response exceeds `u32::MAX - 5`.
	#[cfg(target_os = "linux")]
	#[inline]
	#[allow(clippy::cast_possible_truncation)]
	pub fn put_binary(&mut self, index: usize, response_type: u8, response: &[u8]) {
		assert!(index < self.items.len());
		let len = response.len() + 5;
		assert!(len <= u32::MAX as usize);
		// Sound because of the bounds check above and because zeroed memory
		// is a valid representation for the contained structs.
		let dest = &mut self.items[index];

		// Free the old string if there was already one in this slot
		if !dest.resp.is_null() {
			unsafe { free(dest.resp.cast()) };
		}

		// Copy the data into a buffer that can be deallocated with `free()`.
		// Sound because zeroed memory is a valid representation for `u8`.
		let mut buffer = unsafe { CBox::<u8>::new_zeroed_slice(len).assume_all_init() };
		buffer[0..4].copy_from_slice(&(len as u32).to_be_bytes());
		buffer[4] = response_type;
		buffer[5..].copy_from_slice(response);
		*dest = PamResponse {
			resp: CBox::into_raw_unsized(buffer).cast(),
			resp_retcode: 0,
		};
	}
}

/// Provides read access using `buffer[index]` for convenience and debugging
impl std::ops::Index<usize> for ResponseBuffer {
	type Output = PamResponse;

	#[inline]
	fn index(&self, index: usize) -> &Self::Output {
		assert!(index < self.items.len());
		&self.items[index]
	}
}

/// Provides conversion to slice with `buffer[..]` for convenience
impl std::ops::Index<std::ops::RangeFull> for ResponseBuffer {
	type Output = [PamResponse];

	#[inline]
	fn index(&self, _index: std::ops::RangeFull) -> &Self::Output {
		&self.items[..]
	}
}

/// Provides `for`-loop support for convenience
impl<'a> IntoIterator for &'a ResponseBuffer {
	type Item = &'a PamResponse;
	type IntoIter = std::slice::Iter<'a, PamResponse>;
	fn into_iter(self) -> Self::IntoIter {
		self[..].iter()
	}
}

/// Convert a `ResponseBuffer` into a mutable `PamResponse` array pointer.
///
/// This is mainly used for easy low-level interaction with the PAM
/// framework and the responsibility to call [`libc::free()`] on the
/// array pointer is moved to the caller!
impl From<ResponseBuffer> for *mut PamResponse {
	fn from(mut buf: ResponseBuffer) -> Self {
		let result = (buf.items.as_mut() as *mut [PamResponse]).cast();
		mem::forget(buf);
		result
	}
}

/// Destructor freeing the string memory if not moved into a raw pointer
///
/// This prevents memory leaks if the responsibility to free the buffer
/// was not moved to a PAM module.
impl Drop for ResponseBuffer {
	fn drop(&mut self) {
		for item in self.items.iter_mut() {
			if !item.resp.is_null() {
				unsafe { free(item.resp.cast()) };
				item.resp = ptr::null_mut();
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn prepare_test_buffer() -> ResponseBuffer {
		let mut buffer = ResponseBuffer::new(4).unwrap();
		buffer.put(0, Some(CString::new("some response").unwrap()));
		buffer.put(1, None);
		buffer.put(2, Some(CString::new("some response").unwrap()));
		buffer.put(2, Some(CString::new("another response").unwrap()));
		buffer.put_binary(3, 1, &[]);
		buffer.put_binary(3, 1, &[0, 1, 2]);
		buffer
	}

	#[test]
	fn test_len() {
		assert_eq!(ResponseBuffer::new(1).unwrap().len(), 1);
		assert_eq!(ResponseBuffer::new(3).unwrap().len(), 3);
		assert!(!ResponseBuffer::new(3).unwrap().is_empty());
		assert_eq!(ResponseBuffer::new(65535).unwrap().len(), 65535);
		assert_eq!(ResponseBuffer::new(65535).unwrap()[..].len(), 65535);

		assert!(ResponseBuffer::new(0).is_err());
		assert!(ResponseBuffer::new(-1).is_err());
		assert!(ResponseBuffer::new(isize::MAX).is_err());
	}

	#[test]
	fn test_iter() {
		let buffer = prepare_test_buffer();
		for (i, item) in buffer.iter().enumerate() {
			assert_eq!(item.resp_retcode, 0);
			if i == 1 {
				assert!(item.resp.is_null())
			} else {
				assert!(!item.resp.is_null())
			}
		}
	}

	#[test]
	fn test_index() {
		let buffer = prepare_test_buffer();
		assert_eq!(buffer[0].resp_retcode, 0);
		assert!(buffer[1].resp.is_null());
		assert!(!buffer[2].resp.is_null());
	}
}
