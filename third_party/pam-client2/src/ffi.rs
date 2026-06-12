//! Functions interfacing with the conversation callback from C code

/***********************************************************************
 * (c) 2021-2022 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>*
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

use crate::error::ErrorCode;
use crate::resp_buf::ResponseBuffer;
use crate::ConversationHandler;
use crate::PAM_SUCCESS;

use libc::{c_char, c_int, c_void};
use pam_sys::PAM_BUF_ERR;
use pam_sys::{
	pam_conv as PamConversation, pam_message as PamMessage, pam_response as PamResponse,
};
use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::slice;

/// Pointer type of `pam_message.msg`.
///
/// Linux-PAM declares the field as `const char *`, whereas OpenPAM
/// declares it as `char *`. `pam-sys2` mirrors that per-implementation,
/// so the conversation helpers below must accept whichever mutability the
/// active platform uses or they fail to type-check against
/// `pam_message.msg`. The `openpam` cfg is set by `build.rs`.
#[cfg(openpam)]
type MsgContentPtr = *mut c_char;
#[cfg(not(openpam))]
type MsgContentPtr = *const c_char;

/// Wraps `callback` along with [`pam_converse<T>`] for handing to libpam.
pub(crate) fn into_pam_conv<T: ConversationHandler>(callback: Box<T>) -> PamConversation {
	PamConversation {
		conv: Some(pam_converse::<T>),
		appdata_ptr: Box::into_raw(callback).cast(),
	}
}

/// Extracts the pointer to the conversation handler of type `T`.
///
/// Performs the reverse of [`into_pam_conv()`] (except for not rebuilding a `Box`).
///
/// There is no check that `T` is the correct type. Unsafe code dereferencing
/// the returned pointer must make sure that the types match.
///
/// # Panics
/// Panics on null pointers in the `pam_conv` structure.
pub(crate) fn from_pam_conv<T>(conv: &PamConversation) -> *mut T {
	assert!(conv.conv.is_some());
	assert!(!conv.appdata_ptr.is_null());
	conv.appdata_ptr.cast()
}

/// Maps `prompt_*` success results to `Option<CString>`.
///
/// Might get more complex in case we change the function signatures in the
/// future.
#[allow(clippy::unnecessary_wraps)]
#[inline]
const fn map_conv_string(input: CString) -> Option<CString> {
	Some(input)
}

/// Converts the message pointer into a slice for easy iteration.
///
/// Version for Linux, NetBSD and similar platforms that interpret
/// `**PamMessage` as "array of pointers to `PamMessage` structs".
///
/// # Panics
/// Panics if `num_msg` is negative or `msg` is null
#[cfg(not(target_os = "solaris"))]
#[inline]
#[allow(clippy::cast_sign_loss)]
fn msg_to_slice(msg: &*mut *const PamMessage, num_msg: c_int) -> &[&PamMessage] {
	assert!(num_msg >= 0 || msg.is_null());
	// This is sound, as [&PamMessage] has the same layout as "array of ptrs
	// to PamMessage structs" and msgs lives at least until we return.
	unsafe { slice::from_raw_parts((*msg) as *const &PamMessage, num_msg as usize) }
}

/// Converts the message pointer into a slice for easy iteration.
///
/// Version for Solaris and similar platforms that interpret `**PamMessage`
/// as "pointer to array of PamMessage structs".
///
/// # Panics
/// Panics if `num_msg` is negative or `msg` is null
#[cfg(target_os = "solaris")]
#[inline]
#[allow(clippy::cast_sign_loss)]
fn msg_to_slice(msg: &*mut *const PamMessage, num_msg: c_int) -> &'static [PamMessage] {
	assert!(num_msg >= 0 || msg.is_null());
	// This is sound, as *[PamMessage] has the same layout as "ptr to array
	// of PamMessage structs" and msgs lives at least until we return.
	unsafe { slice::from_raw_parts((**msg), num_msg as usize) }
}

/// Maximum supported message number (for Linux and similar)
#[cfg(not(target_os = "solaris"))]
#[allow(clippy::cast_possible_wrap)]
const fn max_msg_num() -> isize {
	isize::MAX / size_of::<*const PamMessage>() as isize
}

/// Maximum supported message number (for Solaris)
#[cfg(target_os = "solaris")]
#[allow(clippy::cast_possible_wrap)]
const fn max_msg_num() -> isize {
	isize::MAX / size_of::<PamMessage>() as isize
}

/// Interprets the message content as a null-terminated string
///
/// NULL pointers are converted into empty string as a safety measure.
///
/// # Safety
/// This is sound as long as the message type implies a non-binary message
/// and the PAM modules play by the rules.
unsafe fn msg_content_as_cstr(msg: &MsgContentPtr) -> &CStr {
	if msg.is_null() {
		CStr::from_bytes_with_nul_unchecked(b"\0")
	} else {
		CStr::from_ptr(*msg)
	}
}

/// Interprets the message content as a null-terminated string
///
/// NULL pointers are converted into empty string as a safety measure.
///
/// # Safety
/// This is sound as long as the message type implies a non-binary message
/// and the PAM modules play by the rules.
// Only the Linux-PAM `PAM_RADIO_TYPE` path uses this.
#[cfg(target_os = "linux")]
unsafe fn msg_content_to_cstr(msg: &MsgContentPtr) -> &CStr {
	if msg.is_null() {
		CStr::from_bytes_with_nul_unchecked(b"\0")
	} else {
		CStr::from_ptr(*msg)
	}
}

/// Interprets the message content as a binary pseudo-struct
///
/// NULL pointers are converted into empty data as a safety measure.
///
/// # Safety
/// This is sound as long as the message type implies a binary message
/// and the PAM modules play by the rules.
// Only the Linux-PAM `PAM_BINARY_PROMPT` path uses this.
#[cfg(target_os = "linux")]
#[allow(clippy::cast_sign_loss)]
unsafe fn msg_content_to_bin(msg: &MsgContentPtr) -> (u8, &[u8]) {
	if msg.is_null() {
		(0, &[])
	} else {
		// Decode length and data
		// Sound as long as the PAM modules set the length correctly
		let len = u32::from_be_bytes(*(*msg).cast());
		let len = len.saturating_sub(5); // Subtract header length
		let type_ = *msg.add(4) as u8;
		let data = slice::from_raw_parts(msg.add(5).cast(), len as usize);
		(type_, data)
	}
}

/// Conversation function C library callback.
///
/// Will be called by C code when a conversation is requested. Does sanity
/// checks, prepares a response buffer and calls the conversation function
/// identified by `T` and `appdata_ptr` for each message.
pub(crate) unsafe extern "C" fn pam_converse<T: ConversationHandler>(
	num_msg: c_int,
	msg: *mut *const PamMessage,
	out_resp: *mut *mut PamResponse,
	appdata_ptr: *mut c_void,
) -> c_int {
	const MAX_MSG_NUM: isize = max_msg_num();

	// Check for null pointers
	if msg.is_null() || out_resp.is_null() || appdata_ptr.is_null() {
		return PAM_BUF_ERR as c_int;
	}

	// Extract conversation handler from `appdata_ptr`.
	// This is sound, as we did the reverse in `into_pam_conv`.
	let handler = &mut *(appdata_ptr.cast::<T>());

	// Prepare response buffer
	let mut responses = match ResponseBuffer::new(num_msg as isize) {
		Ok(buf) => buf,
		Err(e) => return e.code().repr(),
	};

	// Check preconditions for slice::from_raw_parts.
	// (the checks in `ResponseBuffer::new` are even stricter but better be
	// safe than sorry).
	if !(0..=MAX_MSG_NUM).contains(&(num_msg as isize)) {
		return PAM_BUF_ERR as c_int;
	}

	// Cast `msg` with `num_msg` to a slice for easy iteration.
	let messages = msg_to_slice(&msg, num_msg);

	// Call conversation handler for each message
	for (i, message) in messages.iter().enumerate() {
		match message.msg_style as c_int {
			// Special case: experimental binary messages (Linux)
			#[cfg(target_os = "linux")]
			pam_sys::PAM_BINARY_PROMPT => {
				let (type_, data) = msg_content_to_bin(&message.msg);
				let result = handler.binary_prompt(type_, data);
				match result {
					Ok(response) => responses.put_binary(i, response.0, &response.1),
					Err(code) => return code.repr(),
				}
			}
			// All other cases
			_ => {
				// Delegate to the correct handler method based on `msg_style`
				let result = match message.msg_style as c_int {
					pam_sys::PAM_PROMPT_ECHO_ON => {
						let text = msg_content_as_cstr(&message.msg);
						handler.prompt_echo_on(text).map(map_conv_string)
					}
					pam_sys::PAM_PROMPT_ECHO_OFF => {
						let text = msg_content_as_cstr(&message.msg);
						handler.prompt_echo_off(text).map(map_conv_string)
					}
					pam_sys::PAM_TEXT_INFO => {
						let text = msg_content_as_cstr(&message.msg);
						handler.text_info(text);
						Ok(None)
					}
					pam_sys::PAM_ERROR_MSG => {
						let text = msg_content_as_cstr(&message.msg);
						handler.error_msg(text);
						Ok(None)
					}
					#[cfg(target_os = "linux")]
					pam_sys::PAM_RADIO_TYPE => {
						let text = msg_content_to_cstr(&message.msg);
						handler.radio_prompt(text).map(|b| {
							if b {
								CString::new("yes").ok()
							} else {
								CString::new("no").ok()
							}
						})
					}
					_ => Err(ErrorCode::CONV_ERR),
				};

				// Process response and bail out on errors
				match result {
					Ok(response) => responses.put(i, response),
					Err(code) => return code.repr(),
				}
			}
		}
	}

	// Transfer responses to caller and return.
	// Sound as long as the PAM modules play by the rules..
	*out_resp = responses.into();
	PAM_SUCCESS
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::conv_mock::{Conversation, LogEntry};
	use crate::conv_null::Conversation as NullConversation;
	use libc::free;
	use std::ptr;

	fn make_handler() -> Box<Conversation> {
		Box::new(Conversation::with_credentials("test usër", "paßword"))
	}

	/// Check edge cases for invalid parameters to `pam_conv`
	#[test]
	fn test_edge_cases() {
		let pam_conv = into_pam_conv(make_handler());
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let text = CString::new("").unwrap();
		let mut msg = PamMessage {
			msg_style: pam_sys::PAM_PROMPT_ECHO_ON as c_int,
			msg: text.as_ptr(),
		};
		let mut msg_ptr = &mut msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		assert_eq!(
			unsafe { c_callback(1, ptr::null_mut(), &mut responses as *mut *mut _, appdata,) },
			ErrorCode::BUF_ERR.repr(),
			"pam_conv with null `msg` arg returned `left` instead of BUF_ERR"
		);

		assert_eq!(
			unsafe {
				c_callback(
					1,
					&mut msg_ptr as *mut *const PamMessage,
					ptr::null_mut(),
					appdata,
				)
			},
			ErrorCode::BUF_ERR.repr(),
			"pam_conv with null `out_resp` arg returned `left` instead of BUF_ERR"
		);

		assert_eq!(
			unsafe {
				c_callback(
					1,
					&mut msg_ptr as *mut *const _,
					&mut responses as *mut *mut _,
					ptr::null_mut(),
				)
			},
			ErrorCode::BUF_ERR.repr(),
			"pam_conv with null `appdata_ptr` arg returned `left` instead of BUF_ERR"
		);

		assert_eq!(
			unsafe {
				c_callback(
					-1,
					&mut msg_ptr as *mut *const _,
					&mut responses as *mut *mut _,
					appdata,
				)
			},
			ErrorCode::BUF_ERR.repr(),
			"pam_conv with negative `msg_num` arg returned `left` instead of BUF_ERR"
		);

		// There should be exactly zero message in the log
		let handler: &Conversation = unsafe { &*from_pam_conv(&pam_conv) };
		assert_eq!(
			handler.log.len(),
			0,
			"log contained `left` messages instead of `right`"
		);
	}

	/// Check if `pam_conv` rejects `0` as `num_msg` argument
	///
	/// This is not required by the standard, but our own policy, to keep this
	/// case defined.
	#[test]
	fn test_zero_num() {
		let pam_conv = into_pam_conv(make_handler());
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let mut msg = PamMessage {
			msg_style: pam_sys::PAM_PROMPT_ECHO_ON as c_int,
			msg: ptr::null_mut(),
		};
		let mut msg_ptr = &mut msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		// zero `msg_num` should fail
		assert_eq!(
			unsafe {
				c_callback(
					0,
					&mut msg_ptr as *mut *const _,
					&mut responses as *mut *mut _,
					appdata,
				)
			},
			ErrorCode::BUF_ERR.repr(),
			"pam_conv with zero `msg_num` arg returned `left` instead of BUF_ERR"
		);

		// There should be exactly zero message in the log
		let handler: &Conversation = unsafe { &*from_pam_conv(&pam_conv) };
		assert_eq!(
			handler.log.len(),
			0,
			"log contained `left` messages instead of `right`"
		);
	}

	/// Check if `pam_conv` correctly answers a prompt
	fn test_prompt(style: c_int, prompt: &str, expected: &str) {
		let pam_conv = into_pam_conv(make_handler());
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let text = CString::new(prompt).unwrap();
		let mut msg = PamMessage {
			msg_style: style as c_int,
			msg: text.as_ptr(),
		};
		let mut msg_ptr = &mut msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		let code = unsafe {
			c_callback(
				1,
				&mut msg_ptr as *mut *const _,
				&mut responses as *mut *mut _,
				appdata,
			)
		};
		assert_eq!(
			code,
			pam_sys::PAM_SUCCESS as c_int,
			"pam_conv failed with error code `left`"
		);

		assert!(
			!responses.is_null(),
			"response is still null after conversation"
		);

		assert_eq!(
			unsafe { (*responses).resp_retcode },
			0,
			"retcode in response is `left` instead of 0"
		);

		let response = unsafe { CStr::from_ptr((*responses).resp) };

		assert_eq!(
			response,
			CString::new(expected).unwrap().as_c_str(),
			"response contained `left` instead of expected `right`"
		);

		unsafe {
			free((*responses).resp as *mut _);
			free(responses as *mut _);
		}

		// There should be exactly zero message in the log
		let handler: &Conversation = unsafe { &*from_pam_conv(&pam_conv) };
		assert_eq!(
			handler.log.len(),
			0,
			"log contained `left` messages instead of `right`"
		);
	}

	/// Check if `pam_conv` correctly handles an error from the conversation handler
	#[test]
	fn test_prompt_err() {
		let pam_conv = into_pam_conv(Box::new(NullConversation::new()));
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let text = CString::new("").unwrap();
		let mut msg = PamMessage {
			msg_style: pam_sys::PAM_PROMPT_ECHO_OFF as c_int,
			msg: text.as_ptr(),
		};
		let mut msg_ptr = &mut msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		let code = unsafe {
			c_callback(
				1,
				&mut msg_ptr as *mut *const _,
				&mut responses as *mut *mut _,
				appdata,
			)
		};
		assert_eq!(
			code,
			ErrorCode::CONV_ERR.repr(),
			"pam_conv failed with error code `left`"
		);

		assert!(
			responses.is_null(),
			"response is not null after conversation with error"
		);
	}

	/// Check if `pam_conv` correctly answers an echoing prompt
	#[test]
	fn test_prompt_echo_on() {
		test_prompt(pam_sys::PAM_PROMPT_ECHO_ON, "username? ", "test usër")
	}

	/// Check if `pam_conv` correctly answers a secret prompt
	#[test]
	fn test_prompt_echo_off() {
		test_prompt(pam_sys::PAM_PROMPT_ECHO_OFF, "password? ", "paßword")
	}

	/// Check if `pam_conv` correctly answers a radio prompt (Linxu specific)
	#[test]
	#[cfg(target_os = "linux")]
	fn test_prompt_radio() {
		test_prompt(pam_sys::PAM_RADIO_TYPE, "no? ", "no")
	}

	/// Check if `pam_conv` correctly handles a info/error message
	fn test_output_msg(style: c_int, text: Option<&str>) -> LogEntry {
		let pam_conv = into_pam_conv(make_handler());
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let c_text = CString::new(text.unwrap_or("")).unwrap();
		let mut msg = PamMessage {
			msg_style: style as c_int,
			msg: match text {
				Some(_) => c_text.as_ptr(),
				None => ptr::null(),
			},
		};
		let mut msg_ptr = &mut msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		let code = unsafe {
			c_callback(
				1,
				&mut msg_ptr as *mut *const _,
				&mut responses as *mut *mut _,
				appdata,
			)
		};
		assert_eq!(
			code,
			pam_sys::PAM_SUCCESS as c_int,
			"pam_conv failed with error code `left`"
		);

		assert!(
			!responses.is_null(),
			"response is still null after conversation"
		);

		assert_eq!(
			unsafe { (*responses).resp_retcode },
			0,
			"retcode in response is `left` instead of 0"
		);

		assert_eq!(
			unsafe { (*responses).resp },
			ptr::null_mut(),
			"response text is non-null after output-only conversation"
		);

		unsafe {
			free(responses as *mut _);
		}

		let handler: &mut Conversation = unsafe { &mut *from_pam_conv(&pam_conv) };
		// There should be exactly one message in the log
		assert_eq!(
			handler.log.len(),
			1,
			"log contained `left` messages instead of `right`"
		);

		let result = handler.log[0].clone();
		// Check if clearing works
		handler.clear_log();
		assert_eq!(handler.log.len(), 0, "log could not be emptied");
		result
	}

	/// Check if `pam_conv` correctly transmits an error message
	#[test]
	fn test_error_msg() {
		const MSG: &str = "test error öäüß";
		let logentry = test_output_msg(pam_sys::PAM_ERROR_MSG, Some(MSG));
		if let LogEntry::Error(msg) = logentry {
			assert_eq!(
				msg.to_string_lossy(),
				MSG,
				"log contained unexpected message `left`"
			);
		} else {
			assert!(false, "log contained unexpected message type");
		}
	}

	/// Check if `pam_conv` correctly transmits an info message
	#[test]
	fn test_info_msg() {
		const MSG: &str = "test info äöüß";
		let logentry = test_output_msg(pam_sys::PAM_TEXT_INFO, Some(MSG));
		if let LogEntry::Info(msg) = logentry {
			assert_eq!(
				msg.to_string_lossy(),
				MSG,
				"log contained unexpected message `left`"
			);
		} else {
			assert!(false, "log contained unexpected message type");
		}
	}

	/// Check if a null pointer is safely converted into an empty string
	#[test]
	fn test_null_msg() {
		let logentry = test_output_msg(pam_sys::PAM_TEXT_INFO, None);
		if let LogEntry::Info(msg) = logentry {
			assert_eq!(
				msg.to_string_lossy(),
				"",
				"log contained unexpected message `left`"
			);
		} else {
			assert!(false, "log contained unexpected message type");
		}
	}

	/// Check if a unknown message type produces a conversation error
	#[test]
	#[should_panic(expected = "pam_conv failed with error code `left`")]
	fn test_inval_msg() {
		test_output_msg(65535, None);
	}

	/// Check if `pam_conv` correctly handles a binary message
	#[test]
	#[cfg(target_os = "linux")]
	fn test_binary() {
		let pam_conv = into_pam_conv(make_handler());
		let c_callback = pam_conv.conv.unwrap();
		let appdata = pam_conv.appdata_ptr;

		let buffer: Vec<u8> = vec![0, 0, 0, 6, 0xFF, 0x42];

		let msg = PamMessage {
			msg_style: pam_sys::PAM_BINARY_PROMPT as c_int,
			msg: buffer.as_ptr() as *const _,
		};
		let mut msg_ptr = &msg as *const _;

		let mut responses: *mut PamResponse = ptr::null_mut();

		let code = unsafe {
			c_callback(
				1,
				&mut msg_ptr as *mut *const _,
				&mut responses as *mut *mut _,
				appdata,
			)
		};

		assert_eq!(
			code,
			pam_sys::PAM_CONV_ERR as c_int,
			"pam_conv returned unexpected error code `left`"
		);
	}
}
