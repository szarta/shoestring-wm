//! Null conversation handler

/***********************************************************************
 * Author: Christoph Grenz <christophg+gitorious @ grenz-bonn.de>      *
 *                                                                     *
 * Due to the lack of originality the Null conversation handler        *
 * implementation is given to the public domain. To the extent         *
 * possible under law, the author has waived all copyright and related *
 * or neighboring rights to this Source Code Form.                     *
 * https://creativecommons.org/publicdomain/zero/1.0/legalcode         *
 ***********************************************************************/

#![forbid(unsafe_code)]

use super::ConversationHandler;
use crate::error::ErrorCode;
use std::ffi::{CStr, CString};

/// Null implementation of `ConversationHandler`
///
/// When a PAM module asks for any user interaction an error is returned.
/// Error and info messages are ignored.
///
/// This handler may be used for testing and for environments where no user
/// interaction is possible, no credentials can be stored beforehand and
/// failing is the only answer if some PAM module needs input.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Conversation {}

impl Conversation {
	/// Creates a new null conversation handler
	#[must_use]
	pub const fn new() -> Self {
		Self {}
	}
}

impl Default for Conversation {
	fn default() -> Self {
		Self::new()
	}
}

impl ConversationHandler for Conversation {
	fn prompt_echo_on(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
		Err(ErrorCode::CONV_ERR)
	}

	fn prompt_echo_off(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
		Err(ErrorCode::CONV_ERR)
	}

	fn text_info(&mut self, _msg: &CStr) {}

	fn error_msg(&mut self, _msg: &CStr) {}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test() {
		let text = CString::new("test").unwrap();
		let mut c = Conversation::default();
		assert_eq!(format!("{:?}", c), format!("{:?}", c.clone()));

		assert!(c.prompt_echo_on(&text).is_err());
		assert!(c.prompt_echo_off(&text).is_err());
		assert!(c.radio_prompt(&text).is_err());
		assert!(c.binary_prompt(0, &[]).is_err());
		c.text_info(&text);
		c.error_msg(&text);
	}
}
