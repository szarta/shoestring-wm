/*!
 * Client application interface for Pluggable Authentication Modules (PAM)
 *
 * PAM is the user authentication system used in many Unix-like operating
 * systems (Linux, NetBSD, MacOS, Solaris, etc.) and handles authentication,
 * account management, session management and passwort changing via pluggable
 * modules. This allows programs (such as `login(1)` and `su(1)`) to
 * authenticate users e.g. in a Kerberos domain as well as locally in
 * `/etc/passwd`.
 *
 * This library provides a safe API to the application-faced parts of PAM,
 * covering multiple use-cases with a high grade of flexibility.
 *
 * It is currently only tested on Linux, but should also support OpenPAM-based
 * platforms like NetBSD. Solaris should also be supported, but the support
 * is untested.
 *
 * # Examples
 *
 * Sample workflow for command line interaction like `su -c`, running a
 * single program as the user after they successfully authenticated:
 *
 * ```no_run
 * use pam_client2::{Context, Flag};
 * use pam_client2::conv_cli::Conversation; // CLI implementation
 * use std::process::Command;
 * use std::os::unix::process::CommandExt;
 *
 * let mut context = Context::new(
 *    "my-service",       // Service name, decides which policy is used (see `/etc/pam.d`)
 *    None,               // Optional preset user name
 *    Conversation::new() // Handler for user interaction
 * ).expect("Failed to initialize PAM context");
 *
 * // Optionally set some settings
 * context.set_user_prompt(Some("Who art thou? "));
 *
 * // Authenticate the user (ask for password, 2nd-factor token, fingerprint, etc.)
 * context.authenticate(Flag::NONE).expect("Authentication failed");
 *
 * // Validate the account (is not locked, expired, etc.)
 * context.acct_mgmt(Flag::NONE).expect("Account validation failed");
 *
 * // Get resulting user name and map to a user id
 * let username = context.user();
 * let uid = 65535; // Left as an exercise to the reader
 *
 * // Open session and initialize credentials
 * let mut session = context.open_session(Flag::NONE).expect("Session opening failed");
 *
 * // Run a process in the PAM environment
 * let result = Command::new("/usr/bin/some_program")
 *                      .env_clear()
 *                      .envs(session.envlist().iter_tuples())
 *                      .uid(uid)
 *                   // .gid(...)
 *                      .status();
 *
 * // The session is automatically closed when it goes out of scope.
 * ```
 *
 * Sample workflow for non-interactive authentication
 * ```no_run
 * use pam_client2::{Context, Flag};
 * use pam_client2::conv_mock::Conversation; // Non-interactive implementation
 *
 * let mut context = Context::new(
 *    "my-service",  // Service name
 *    None,
 *    Conversation::with_credentials("username", "password")
 * ).expect("Failed to initialize PAM context");
 *
 * // Authenticate the user
 * context.authenticate(Flag::NONE).expect("Authentication failed");
 *
 * // Validate the account
 * context.acct_mgmt(Flag::NONE).expect("Account validation failed");
 *
 * // ...
 * ```
 *
 * Sample workflow for session management without PAM authentication:
 * ```no_run
 * use pam_client2::{Context, Flag};
 * use pam_client2::conv_null::Conversation; // Null implementation
 *
 * let mut context = Context::new(
 *    "my-service",     // Service name
 *    Some("username"), // Preset username
 *    Conversation::new()
 * ).expect("Failed to initialize PAM context");
 *
 * // We already authenticated the user by other means (e.g. SSH key) so we
 * // skip `context.authenticate()`.
 *
 * // Validate the account
 * context.acct_mgmt(Flag::NONE).expect("Account validation failed");
 *
 * // Open session and initialize credentials
 * let mut session = context.open_session(Flag::NONE).expect("Session opening failed");
 *
 * // ...
 *
 * // The session is automatically closed when it goes out of scope.
 * ```
 */

/***********************************************************************
 * (c) 2021-2022 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>*
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

mod c_box;
mod context;
#[cfg(feature = "cli")]
pub mod conv_cli;
pub mod conv_mock;
pub mod conv_null;
mod conversation;
pub mod env_list;
mod error;
mod ffi;
mod resp_buf;
mod session;

#[macro_use]
extern crate bitflags;
use libc::{c_char, c_int};
use std::ffi::CStr;

pub use context::Context;
pub use conversation::ConversationHandler;
pub use error::{Error, ErrorWith};
pub use session::{Session, SessionToken};

use pam_sys::*;
pub extern crate pam_sys2 as pam_sys;

fn char_ptr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
	if ptr.is_null() {
		None
	} else {
		let cstr = unsafe { CStr::from_ptr(ptr) };
		match cstr.to_str() {
			Err(_) => None,
			Ok(s) => Some(s),
		}
	}
}

bitflags! {
	/// Flags for most PAM functions
	#[allow(clippy::upper_case_acronyms)]
	#[repr(transparent)]
	#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(transparent))]
	#[derive(Copy, Clone)]
	pub struct Flag: c_int {
		/// Don't generate any messages
		const SILENT = PAM_SILENT as c_int;
		/// Fail with `AUTH_ERROR` if the user has a null authentication token.
		const DISALLOW_NULL_AUTHTOK = PAM_DISALLOW_NULL_AUTHTOK as c_int;
		/// Only update passwords that have aged.
		const CHANGE_EXPIRED_AUTHTOK = PAM_CHANGE_EXPIRED_AUTHTOK as c_int;
		/// Set user credentials.
		#[doc(hidden)]
		const ESTABLISH_CRED = PAM_ESTABLISH_CRED as c_int;
		/// Delete user credentials.
		#[doc(hidden)]
		const DELETE_CRED = PAM_DELETE_CRED as c_int;
		/// Reinitialize user credentials.
		#[doc(hidden)]
		const REINITIALIZE_CRED = PAM_REINITIALIZE_CRED as c_int;
		/// Extend lifetime of user credentials.
		#[doc(hidden)]
		const REFRESH_CRED = PAM_REFRESH_CRED as c_int;
	}
}

#[allow(clippy::upper_case_acronyms)]
impl Flag {
	/// No flags; use default behaviour.
	pub const NONE: Flag = Flag::empty();
}

#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[repr(isize)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
/// PAM error codes
pub enum ErrorCode {
	OPEN_ERR = PAM_OPEN_ERR as isize,
	SYMBOL_ERR = PAM_SYMBOL_ERR as isize,
	SERVICE_ERR = PAM_SERVICE_ERR as isize,
	SYSTEM_ERR = PAM_SYSTEM_ERR as isize,
	BUF_ERR = PAM_BUF_ERR as isize,
	PERM_DENIED = PAM_PERM_DENIED as isize,
	AUTH_ERR = PAM_AUTH_ERR as isize,
	CRED_INSUFFICIENT = PAM_CRED_INSUFFICIENT as isize,
	AUTHINFO_UNAVAIL = PAM_AUTHINFO_UNAVAIL as isize,
	USER_UNKNOWN = PAM_USER_UNKNOWN as isize,
	MAXTRIES = PAM_MAXTRIES as isize,
	NEW_AUTHTOK_REQD = PAM_NEW_AUTHTOK_REQD as isize,
	ACCT_EXPIRED = PAM_ACCT_EXPIRED as isize,
	SESSION_ERR = PAM_SESSION_ERR as isize,
	CRED_UNAVAIL = PAM_CRED_UNAVAIL as isize,
	CRED_EXPIRED = PAM_CRED_EXPIRED as isize,
	CRED_ERR = PAM_CRED_ERR as isize,
	CONV_ERR = PAM_CONV_ERR as isize,
	AUTHTOK_ERR = PAM_AUTHTOK_ERR as isize,
	AUTHTOK_RECOVERY_ERR = PAM_AUTHTOK_RECOVERY_ERR as isize,
	AUTHTOK_LOCK_BUSY = PAM_AUTHTOK_LOCK_BUSY as isize,
	AUTHTOK_DISABLE_AGING = PAM_AUTHTOK_DISABLE_AGING as isize,
	ABORT = PAM_ABORT as isize,
	AUTHTOK_EXPIRED = PAM_AUTHTOK_EXPIRED as isize,
	MODULE_UNKNOWN = PAM_MODULE_UNKNOWN as isize,
	BAD_ITEM = PAM_BAD_ITEM as isize,
	// `PAM_CONV_AGAIN` / `PAM_INCOMPLETE` are Linux-PAM extensions; OpenPAM
	// has no such return codes and never produces them, so these variants
	// only exist on Linux-PAM.
	#[cfg(not(openpam))]
	CONV_AGAIN = PAM_CONV_AGAIN as isize,
	#[cfg(not(openpam))]
	INCOMPLETE = PAM_INCOMPLETE as isize,
}

impl ErrorCode {
    pub fn repr(&self) -> c_int {
        match self {
            ErrorCode::OPEN_ERR => PAM_OPEN_ERR as c_int,
            ErrorCode::SYMBOL_ERR => PAM_SYMBOL_ERR as c_int,
            ErrorCode::SERVICE_ERR => PAM_SERVICE_ERR as c_int,
            ErrorCode::SYSTEM_ERR => PAM_SYSTEM_ERR as c_int,
            ErrorCode::BUF_ERR => PAM_BUF_ERR as c_int,
            ErrorCode::PERM_DENIED => PAM_PERM_DENIED as c_int,
            ErrorCode::AUTH_ERR => PAM_AUTH_ERR as c_int,
            ErrorCode::CRED_INSUFFICIENT => PAM_CRED_INSUFFICIENT as c_int,
            ErrorCode::AUTHINFO_UNAVAIL => PAM_AUTHINFO_UNAVAIL as c_int,
            ErrorCode::USER_UNKNOWN => PAM_USER_UNKNOWN as c_int,
            ErrorCode::MAXTRIES => PAM_MAXTRIES as c_int,
            ErrorCode::NEW_AUTHTOK_REQD => PAM_NEW_AUTHTOK_REQD as c_int,
            ErrorCode::ACCT_EXPIRED => PAM_ACCT_EXPIRED as c_int,
            ErrorCode::SESSION_ERR => PAM_SESSION_ERR as c_int,
            ErrorCode::CRED_UNAVAIL => PAM_CRED_UNAVAIL as c_int,
            ErrorCode::CRED_EXPIRED => PAM_CRED_EXPIRED as c_int,
            ErrorCode::CRED_ERR => PAM_CRED_ERR as c_int,
            ErrorCode::CONV_ERR => PAM_CONV_ERR as c_int,
            ErrorCode::AUTHTOK_ERR => PAM_AUTHTOK_ERR as c_int,
            ErrorCode::AUTHTOK_RECOVERY_ERR => PAM_AUTHTOK_RECOVERY_ERR as c_int,
            ErrorCode::AUTHTOK_LOCK_BUSY => PAM_AUTHTOK_LOCK_BUSY as c_int,
            ErrorCode::AUTHTOK_DISABLE_AGING => {
                PAM_AUTHTOK_DISABLE_AGING as c_int
            }
            ErrorCode::ABORT => PAM_ABORT as c_int,
            ErrorCode::AUTHTOK_EXPIRED => PAM_AUTHTOK_EXPIRED as c_int,
            ErrorCode::MODULE_UNKNOWN => PAM_MODULE_UNKNOWN as c_int,
            ErrorCode::BAD_ITEM => PAM_BAD_ITEM as c_int,
            #[cfg(not(openpam))]
            ErrorCode::CONV_AGAIN => PAM_CONV_AGAIN as c_int,
            #[cfg(not(openpam))]
            ErrorCode::INCOMPLETE => PAM_INCOMPLETE as c_int,
        }
    }
    pub fn from_repr(x: c_int) -> Option<ErrorCode> {
		match x {
			PAM_OPEN_ERR => Some(ErrorCode::OPEN_ERR),
			PAM_SYMBOL_ERR => Some(ErrorCode::SYMBOL_ERR),
			PAM_SERVICE_ERR => Some(ErrorCode::SERVICE_ERR),
			PAM_SYSTEM_ERR => Some(ErrorCode::SYSTEM_ERR),
			PAM_BUF_ERR => Some(ErrorCode::BUF_ERR),
			PAM_PERM_DENIED => Some(ErrorCode::PERM_DENIED),
			PAM_AUTH_ERR => Some(ErrorCode::AUTH_ERR),
			PAM_CRED_INSUFFICIENT => Some(ErrorCode::CRED_INSUFFICIENT),
			PAM_AUTHINFO_UNAVAIL => Some(ErrorCode::AUTHINFO_UNAVAIL),
			PAM_USER_UNKNOWN => Some(ErrorCode::USER_UNKNOWN),
			PAM_MAXTRIES => Some(ErrorCode::MAXTRIES),
			PAM_NEW_AUTHTOK_REQD => Some(ErrorCode::NEW_AUTHTOK_REQD),
			PAM_ACCT_EXPIRED => Some(ErrorCode::ACCT_EXPIRED),
			PAM_SESSION_ERR => Some(ErrorCode::SESSION_ERR),
			PAM_CRED_UNAVAIL => Some(ErrorCode::CRED_UNAVAIL),
			PAM_CRED_EXPIRED => Some(ErrorCode::CRED_EXPIRED),
			PAM_CRED_ERR => Some(ErrorCode::CRED_ERR),
			PAM_CONV_ERR => Some(ErrorCode::CONV_ERR),
			PAM_AUTHTOK_ERR => Some(ErrorCode::AUTHTOK_ERR),
			PAM_AUTHTOK_RECOVERY_ERR => Some(ErrorCode::AUTHTOK_RECOVERY_ERR),
			PAM_AUTHTOK_LOCK_BUSY => Some(ErrorCode::AUTHTOK_LOCK_BUSY),
			PAM_AUTHTOK_DISABLE_AGING => Some(ErrorCode::AUTHTOK_DISABLE_AGING),
			PAM_ABORT => Some(ErrorCode::ABORT),
			PAM_AUTHTOK_EXPIRED => Some(ErrorCode::AUTHTOK_EXPIRED),
			PAM_MODULE_UNKNOWN => Some(ErrorCode::MODULE_UNKNOWN),
			PAM_BAD_ITEM => Some(ErrorCode::BAD_ITEM),
			#[cfg(not(openpam))]
			PAM_CONV_AGAIN => Some(ErrorCode::CONV_AGAIN),
			#[cfg(not(openpam))]
			PAM_INCOMPLETE => Some(ErrorCode::INCOMPLETE),
			_ => None,
		}
    }
}

/// Type alias for the result of most PAM methods.
pub type Result<T> = std::result::Result<T, Error>;
/// Type alias for the result of PAM methods that pass back a consumed struct
/// on error.
pub type ExtResult<T, P> = std::result::Result<T, ErrorWith<P>>;

const PAM_SUCCESS: c_int = pam_sys::PAM_SUCCESS as c_int;
