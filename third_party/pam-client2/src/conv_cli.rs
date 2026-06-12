/*!
 * Interactive command line conversation handler
 *
 * *This module is unavailable if pam-client is built without the `"cli"` feature.*
 */

/***********************************************************************
 * (c) 2021 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>     *
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

#![forbid(unsafe_code)]

use super::ConversationHandler;
use crate::error::ErrorCode;
use std::ffi::{CStr, CString};
use std::io::{self, BufRead, Write};

/// Newline trimming helper function
fn trim_newline(s: &mut String) {
	if s.ends_with('\n') {
		s.pop();
		if s.ends_with('\r') {
			s.pop();
		}
	}
}

/// Command-line implementation of `ConversationHandler`
///
/// *This struct is unavailable if pam-client is built without the `"cli"` feature.*
///
/// Prompts, info and error messages will be written to STDERR, non-secret
/// input in read from STDIN and [rpassword][`rpassword::prompt_password`]
/// is used to prompt the user for passwords.
///
/// # Limitations
///
/// Please note that UTF-8 encoding is assumed for terminal I/O, so this
/// handler may fail to authenticate on legacy non-UTF-8 systems when the user
/// input contains non-ASCII characters.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Conversation {
	info_prefix: String,
	error_prefix: String,
}

impl Conversation {
	/// Creates a new CLI conversation handler.
	#[must_use]
	pub fn new() -> Self {
		Self {
			info_prefix: "[PAM INFO] ".to_string(),
			error_prefix: "[PAM ERROR] ".to_string(),
		}
	}

	/// The prefix text written before info text
	#[inline]
	#[must_use]
	pub fn info_prefix(&self) -> &str {
		&self.info_prefix
	}

	/// Updates the prefix put before info text
	pub fn set_info_prefix(&mut self, prefix: impl Into<String>) {
		self.info_prefix = prefix.into();
	}

	/// The prefix text written before error messages
	#[inline]
	#[must_use]
	pub fn error_prefix(&self) -> &str {
		&self.error_prefix
	}

	/// Updates the prefix put before error messages
	pub fn set_error_prefix(&mut self, prefix: impl Into<String>) {
		self.error_prefix = prefix.into();
	}
}

impl Default for Conversation {
	fn default() -> Self {
		Self::new()
	}
}

impl ConversationHandler for Conversation {
	fn prompt_echo_on(&mut self, msg: &CStr) -> Result<CString, ErrorCode> {
		let mut line = String::new();
		if io::stderr().lock().write_all(msg.to_bytes()).is_err() {
			return Err(ErrorCode::CONV_ERR);
		}
		let result = io::stdin().lock().read_line(&mut line);
		match result {
			Err(_) | Ok(0) => Err(ErrorCode::CONV_ERR),
			Ok(_) => {
				trim_newline(&mut line);
				CString::new(line).map_err(|_| ErrorCode::CONV_ERR)
			}
		}
	}

	fn prompt_echo_off(&mut self, msg: &CStr) -> Result<CString, ErrorCode> {
		let prompt = msg.to_string_lossy();
		match rpassword::prompt_password(prompt) {
			Err(_) => Err(ErrorCode::CONV_ERR),
			Ok(password) => CString::new(password).map_err(|_| ErrorCode::CONV_ERR),
		}
	}

	fn text_info(&mut self, msg: &CStr) {
		eprintln!("{}{}", &self.info_prefix, msg.to_string_lossy());
	}

	fn error_msg(&mut self, msg: &CStr) {
		eprintln!("{}{}", &self.error_prefix, msg.to_string_lossy());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_trim() {
		let mut value = "Test\r\n".to_string();
		trim_newline(&mut value);
		assert_eq!(value, "Test");

		let mut value = "Test\n".to_string();
		trim_newline(&mut value);
		assert_eq!(value, "Test");
	}

	#[test]
	fn test_output() {
		let mut c = Conversation::default();
		let _ = c.clone();
		c.set_info_prefix("INFO: ");
		c.set_error_prefix("ERROR: ");
		assert_eq!(c.info_prefix(), "INFO: ");
		assert_eq!(c.error_prefix(), "ERROR: ");
		c.text_info(&CString::new("test").unwrap());
		c.error_msg(&CString::new("test2").unwrap());

		assert!(format!("{:?}", &c).contains("ERROR: "));
	}
}
