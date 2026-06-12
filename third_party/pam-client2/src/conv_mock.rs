//! Simple non-interactive conversation handler

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
use std::iter::FusedIterator;
use std::vec;

/// Elements in [`Conversation::log`]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LogEntry {
	Info(CString),
	Error(CString),
}

/// Non-interactive implementation of `ConversationHandler`
///
/// When a PAM module asks for a non-secret string, [`username`][`Self::username`]
/// will be returned and when a secret string is asked for,
/// [`password`][`Self::password`] will be returned.
///
/// All info and error messages will be recorded in [`log`][`Self::log`].
///
/// # Limitations
///
/// This is enough to handle many authentication flows non-interactively, but
/// flows with two-factor-authentication and things like
/// [`chauthok()`][`crate::Context::chauthtok()`] will most definitely fail.
///
/// Please also note that UTF-8 encoding is assumed for both username and
/// password, so this handler may fail to authenticate on legacy non-UTF-8
/// systems when one of the strings contains non-ASCII characters.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Conversation {
	/// The username to use
	pub username: String,
	/// The password to use
	pub password: String,
	/// All received info/error messages
	pub log: vec::Vec<LogEntry>,
}

impl Conversation {
	/// Creates a new CLI conversation handler
	///
	/// If [`username`][`Self::username`] isn't manually set to a non-empty
	/// string, it will be automatically set to the `Context`s default
	/// username on context initialization.
	#[must_use]
	pub const fn new() -> Self {
		Self {
			username: String::new(),
			password: String::new(),
			log: vec::Vec::new(),
		}
	}

	/// Creatse a new CLI conversation handler with preset credentials
	#[must_use]
	pub fn with_credentials(username: impl Into<String>, password: impl Into<String>) -> Self {
		Self {
			username: username.into(),
			password: password.into(),
			log: vec::Vec::new(),
		}
	}

	/// Clears the error/info log
	pub fn clear_log(&mut self) {
		self.log.clear();
	}

	/// Lists only errors from the log
	pub fn errors(&self) -> impl Iterator<Item = &CString> + FusedIterator {
		self.log.iter().filter_map(|x| match x {
			LogEntry::Info(_) => None,
			LogEntry::Error(msg) => Some(msg),
		})
	}

	/// Lists only info messages from the log
	pub fn infos(&self) -> impl Iterator<Item = &CString> + FusedIterator {
		self.log.iter().filter_map(|x| match x {
			LogEntry::Info(msg) => Some(msg),
			LogEntry::Error(_) => None,
		})
	}
}

impl Default for Conversation {
	fn default() -> Self {
		Self::new()
	}
}

impl ConversationHandler for Conversation {
	fn init(&mut self, default_user: Option<&str>) {
		if let Some(user) = default_user {
			if self.username.is_empty() {
				self.username = user.to_string();
			}
		}
	}

	fn prompt_echo_on(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
		CString::new(self.username.clone()).map_err(|_| ErrorCode::CONV_ERR)
	}

	fn prompt_echo_off(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
		CString::new(self.password.clone()).map_err(|_| ErrorCode::CONV_ERR)
	}

	fn text_info(&mut self, msg: &CStr) {
		self.log.push(LogEntry::Info(msg.to_owned()));
	}

	fn error_msg(&mut self, msg: &CStr) {
		self.log.push(LogEntry::Error(msg.to_owned()));
	}

	fn radio_prompt(&mut self, _msg: &CStr) -> Result<bool, ErrorCode> {
		Ok(false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test() {
		let text = CString::new("test").unwrap();
		let mut c = Conversation::default();
		let _ = c.clone();
		assert!(c.prompt_echo_on(&text).is_ok());
		assert!(c.prompt_echo_off(&text).is_ok());
		assert!(c.radio_prompt(&text).ok() == Some(false));
		assert!(c.binary_prompt(0, &[]).is_err());
		c.text_info(&text);
		c.error_msg(&text);
		assert_eq!(c.log.len(), 2);
		let v: std::vec::Vec<&CString> = c.errors().collect();
		assert_eq!(v.len(), 1);
		let v: std::vec::Vec<&CString> = c.infos().collect();
		assert_eq!(v.len(), 1);
		assert!(format!("{:?}", &c).contains("test"));
	}

	#[test]
	fn test_boxed() {
		let text = CString::new("test").unwrap();
		let mut c = Box::<Conversation>::default();
		assert!(c.prompt_echo_on(&text).is_ok());
		assert!(c.prompt_echo_off(&text).is_ok());
		assert!(c.radio_prompt(&text).ok() == Some(false));
		assert!(c.binary_prompt(0, &[]).is_err());
		c.text_info(&text);
		c.error_msg(&text);
		assert_eq!(c.log.len(), 2);
		let v: std::vec::Vec<&CString> = c.errors().collect();
		assert_eq!(v.len(), 1);
		let v: std::vec::Vec<&CString> = c.infos().collect();
		assert_eq!(v.len(), 1);
		assert!(format!("{:?}", &c).contains("test"));
	}
}
