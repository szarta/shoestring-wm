//! PAM context and related helpers

/***********************************************************************
 * (c) 2021-2022 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>*
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

use crate::env_list::EnvList;
use crate::error::{Error, ErrorCode};
use crate::ffi::{from_pam_conv, into_pam_conv};
use crate::session::{Session, SessionToken};
use crate::{char_ptr_to_str, ConversationHandler};
extern crate libc;
extern crate pam_sys2 as pam_sys;

use crate::{ExtResult, Flag, Result, PAM_SUCCESS};

use libc::{c_int, c_void};
// `c_char` / `slice` are only referenced by the Linux-only `PAM_XAUTHDATA`
// support below.
#[cfg(any(target_os = "linux", doc))]
use libc::c_char;
use pam_sys::pam_conv as PamConversation;
use pam_sys::pam_handle_t as RawPamHandle;
use pam_sys::{
	pam_acct_mgmt, pam_authenticate, pam_chauthtok, pam_close_session, pam_end, pam_get_item,
	pam_getenv, pam_getenvlist, pam_open_session, pam_putenv, pam_set_item, pam_setcred, pam_start,
};
use std::cell::Cell;
use std::ffi::{CStr, CString, OsStr};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::os::unix::ffi::OsStrExt;
use std::ptr::NonNull;
use std::ptr;
#[cfg(any(target_os = "linux", doc))]
use std::slice;

/// Internal: Builds getters/setters for string-typed PAM items.
macro_rules! impl_pam_str_item {
	($name:ident, $set_name:ident, $item_type:expr$(, $doc:literal$(, $extdoc:literal)?)?$(,)?) => {
		$(#[doc = "Returns "]#[doc = $doc]$(#[doc = "\n\n"]#[doc = $extdoc])?)?
		pub fn $name(&self) -> Result<String> {
			let ptr = self.get_item($item_type as c_int)?;
			if ptr.is_null() {
				return Err(Error::new(self.handle(), ErrorCode::PERM_DENIED));
			}
			let string = unsafe { CStr::from_ptr(ptr.cast()) }.to_string_lossy().into_owned();
			return Ok(string);
		}

		$(#[doc = "Sets "]#[doc = $doc])?
		pub fn $set_name(&mut self, value: Option<&str>) -> Result<()> {
			match value {
				None => unsafe { self.set_item($item_type as c_int, ptr::null()) },
				Some(string) => {
					let cstring = CString::new(string).map_err(|_| Error::new(self.handle(), ErrorCode::BUF_ERR))?;
					unsafe { self.set_item($item_type as c_int, cstring.as_ptr().cast()) }
				}
			}
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PamHandle(NonNull<RawPamHandle>);

/// PAM handle wrapper
impl PamHandle {
	/// Create a PAM handle from a raw handle pointer.
	///
	/// # Safety
	/// The argument `ptr` must be NULL or a valid non-dangling PAM handle created by `pam_start`.
	#[inline]
	pub unsafe fn new(ptr: *mut RawPamHandle) -> Option<Self> {
		NonNull::new(ptr).map(Self)
	}

	#[inline]
	pub const fn as_ptr(self) -> *mut RawPamHandle {
		self.0.as_ptr()
	}
}

impl From<PamHandle> for *mut RawPamHandle {
	#[inline]
	fn from(handle: PamHandle) -> Self {
		handle.as_ptr()
	}
}

impl From<PamHandle> for *const RawPamHandle {
	#[inline]
	fn from(handle: PamHandle) -> Self {
		handle.as_ptr()
	}
}

/// Special struct for the `PAM_XAUTHDATA` pam item
///
/// Uses const pointers as `pam_set_item` makes a copy of the data and
/// never mutates through the pointers and `pam_get_item` by API contract
/// states that returned data should not be modified.
#[cfg(any(target_os = "linux", doc))]
#[repr(C)]
#[derive(Debug)]
struct XAuthData {
	/// Length of `name` in bytes excluding the trailing NUL
	pub namelen: c_int,
	/// Name of the authentication method as a null terminated string
	pub name: *const c_char,
	/// Length of `data` in bytes
	pub datalen: c_int,
	/// Authentication method-specific data
	pub data: *const c_char,
}

/// Main struct for PAM interaction
///
/// Manages a PAM context holding the transaction state.
///
/// See the [crate documentation][`crate`] for examples.
pub struct Context<ConvT> {
	handle: PamHandle,
	last_status: Cell<c_int>,
	_conversation: PhantomData<ConvT>,
}

impl<ConvT> Context<ConvT>
where
	ConvT: ConversationHandler,
{
	/// Creates a PAM context and starts a PAM transaction.
	///
	/// # Parameters
	/// - `service` – Name of the service. The policy for the service will be
	///   read from the file /etc/pam.d/*service_name*, falling back to
	///   /etc/pam.conf.
	/// - `username` – Name of the target user. If `None`, the user will be
	///   asked through the conversation handler if neccessary.
	/// - `conversation` – A conversation handler through which the user can be
	///   asked for his username, password, etc. Use
	///   [`conv_cli::Conversation`][`crate::conv_cli::Conversation`] for a command line
	///   default implementation, [`conv_mock::Conversation`][`crate::conv_mock::Conversation`]
	///   for fixed credentials or implement the [`ConversationHandler`] trait
	///   for custom behaviour.
	///
	/// # Errors
	/// Expected error codes include:
	/// - `ErrorCode::ABORT` – General failure
	/// - `ErrorCode::BUF_ERR` – Memory allocation failure or null byte in
	///   parameter.
	/// - `ErrorCode::SYSTEM_ERR` – Other system error
	#[rustversion::attr(since(1.48), doc(alias = "pam_start"))]
	pub fn new(service: &str, username: Option<&str>, conversation: ConvT) -> Result<Self> {
		// Wrap `conversation` in a box and delegate to `from_boxed_conv`
		Self::from_boxed_conv(service, username, Box::new(conversation))
	}

	/// Creates a PAM context and starts a PAM transaction taking a boxed
	/// conversation handler.
	///
	/// See [`new()`][`Self::new()`] for details.
	pub fn from_boxed_conv(
		service: &str,
		username: Option<&str>,
		boxed_conv: Box<ConvT>,
	) -> Result<Self> {
		let mut handle: *mut RawPamHandle = ptr::null_mut();

		let c_service = CString::new(service).map_err(|_| Error::from(ErrorCode::BUF_ERR))?;
		let c_username = match username {
			None => None,
			Some(name) => Some(CString::new(name).map_err(|_| Error::from(ErrorCode::BUF_ERR))?),
		};

		// Create callback struct for C code
		let pam_conv = into_pam_conv(boxed_conv);

		// Start the PAM context
		match unsafe {
			pam_start(
				c_service.as_ptr(),
				c_username.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
				&pam_conv,
				&mut handle,
			)
		} {
			PAM_SUCCESS => {
				// A null pointer should never happen on PAM_SUCCESS, but we need to check to make sure.
				// Safety: The handle came from `pam_start` so it must be valid.
				let handle = unsafe { PamHandle::new(handle) }
					.ok_or_else(|| Error::from(ErrorCode::ABORT))?;
				let mut result = Self {
					handle,
					last_status: Cell::new(PAM_SUCCESS),
					_conversation: PhantomData,
				};
				// Initialize the conversation handler
				result.conversation_mut().init(username);
				Ok(result)
			}
			code => Err(ErrorCode::from_repr(code)
				.unwrap_or(ErrorCode::ABORT)
				.into()),
		}
	}

	/// Authenticates a user.
	///
	/// The conversation handler may be called to ask the user for their name
	/// (especially if no initial username was provided), their password and
	/// possibly other tokens if e.g. two-factor authentication is required.
	/// Conversely the conversation handler may not be called if authentication
	/// is handled by other means, e.g. a fingerprint scanner.
	///
	/// Relevant `flags` are [`Flag::NONE`], [`Flag::SILENT`] and
	/// [`Flag::DISALLOW_NULL_AUTHTOK`] (don't authenticate empty
	/// passwords).
	///
	/// # Errors
	/// Expected error codes include:
	/// - `ABORT` – Serious failure; the application should exit.
	/// - `AUTH_ERR` – The user was not authenticated.
	/// - `CRED_INSUFFICIENT` – The application does not have sufficient
	///   credentials to authenticate the user.
	/// - `AUTHINFO_UNAVAIL` – Could not retrieve authentication information
	///   due to e.g. network failure.
	/// - `MAXTRIES` – At least one module reached its retry limit. Do not
	/// - try again.
	/// - `USER_UNKNOWN` – User not known.
	/// - `INCOMPLETE` – The conversation handler returned `CONV_AGAIN`. Call
	///   again after the asynchronous conversation finished.
	#[rustversion::attr(since(1.48), doc(alias = "pam_authenticate"))]
	pub fn authenticate(&mut self, flags: Flag) -> Result<()> {
		self.wrap_pam_return(unsafe { pam_authenticate(self.handle().into(), flags.bits()) })
	}

	/// Validates user account authorization.
	///
	/// Determines if the account is valid, not expired, and verifies other
	/// access restrictions. Usually used directly after authentication.
	/// The conversation handler may be called by some PAM module.
	///
	/// Relevant `flags` are [`Flag::NONE`], [`Flag::SILENT`] and
	/// [`Flag::DISALLOW_NULL_AUTHTOK`] (demand password change on empty
	/// passwords).
	///
	/// # Errors
	/// Expected error codes include:
	/// - `ACCT_EXPIRED` – Account has expired.
	/// - `AUTH_ERR` – Authentication failure.
	/// - `NEW_AUTHTOK_REQD` – Password has expired. Use [`chauthtok()`] to let
	///   the user change their password or abort.
	/// - `PERM_DENIED` – Permission denied
	/// - `USER_UNKNOWN` – User not known
	/// - `INCOMPLETE` – The conversation handler returned `CONV_AGAIN`. Call
	///   again after the asynchronous conversation finished.
	///
	/// [`chauthtok()`]: `Self::chauthtok`
	#[rustversion::attr(since(1.48), doc(alias = "pam_acct_mgmt"))]
	pub fn acct_mgmt(&mut self, flags: Flag) -> Result<()> {
		self.wrap_pam_return(unsafe { pam_acct_mgmt(self.handle().into(), flags.bits()) })
	}

	/// Fully reinitializes the user's credentials (if established).
	///
	/// Reinitializes credentials like Kerberos tokens for when a session
	/// is already managed by another process. This is e.g. used in
	/// lockscreen applications to refresh the credentials of the desktop
	/// session.
	///
	/// Relevant `flags` are [`Flag::NONE`] and [`Flag::SILENT`].
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BUF_ERR` – Memory allocation error
	/// - `CRED_ERR` – Setting credentials failed
	/// - `CRED_UNAVAIL` – Failed to retrieve credentials
	/// - `SYSTEM_ERR` – Other system error
	/// - `USER_UNKNOWN` – User not known
	pub fn reinitialize_credentials(&mut self, flags: Flag) -> Result<()> {
		self.wrap_pam_return(unsafe {
			pam_setcred(
				self.handle().into(),
				(Flag::REINITIALIZE_CRED | flags).bits(),
			)
		})
	}

	/// Changes a users password.
	///
	/// The conversation handler will be used to request the new password
	/// and might query for the old one.
	///
	/// Relevant `flags` are [`Flag::NONE`], [`Flag::SILENT`] and
	/// [`Flag::CHANGE_EXPIRED_AUTHTOK`] (only initiate change for
	/// expired passwords).
	///
	/// # Errors
	/// Expected error codes include:
	/// - `AUTHTOK_ERR` – Unable to obtain the new password
	/// - `AUTHTOK_RECOVERY_ERR` – Unable to obtain the old password
	/// - `AUTHTOK_LOCK_BUSY` – Authentication token is currently locked
	/// - `AUTHTOK_DISABLE_AGING` – Password aging is disabled (password may
	///   be unchangeable in at least one module)
	/// - `PERM_DENIED` – Permission denied
	/// - `TRY_AGAIN` – Not all modules were able to prepare an authentication
	///   token update. Nothing was changed.
	/// - `USER_UNKNOWN` – User not known
	/// - `INCOMPLETE` – The conversation handler returned `CONV_AGAIN`. Call
	///   again after the asynchronous conversation finished.
	#[rustversion::attr(since(1.48), doc(alias = "pam_chauthtok"))]
	pub fn chauthtok(&mut self, flags: Flag) -> Result<()> {
		self.wrap_pam_return(unsafe { pam_chauthtok(self.handle().into(), flags.bits()) })
	}

	/// Sets up a user session.
	///
	/// Establishes user credentials and performs various tasks to prepare
	/// a login session, may create the home directory on first login, mount
	/// user-specific directories, log access times, etc. The application
	/// must usually have sufficient privileges to perform this task (e.g.
	/// have EUID 0). The returned [`Session`] object closes the session and
	/// deletes the established credentials on drop.
	///
	/// The user should already be [authenticated] and [authorized] at this
	/// point, but this isn't enforced or strictly neccessary if the user
	/// was authenticated by other means. In that case the conversation
	/// handler might be called by e.g. crypt-mount modules to get a password.
	///
	/// Relevant `flags` are [`Flag::NONE`] and [`Flag::SILENT`].
	///
	/// # Errors
	/// Expected error codes include:
	/// - `ABORT` – Serious failure; the application should exit
	/// - `BUF_ERR` – Memory allocation error
	/// - `SESSION_ERR` – Some session initialization failed
	/// - `CRED_ERR` – Setting credentials failed
	/// - `CRED_UNAVAIL` – Failed to retrieve credentials
	/// - `SYSTEM_ERR` – Other system error
	/// - `USER_UNKNOWN` – User not known
	/// - `INCOMPLETE` – The conversation handler returned `CONV_AGAIN`. Call
	///   again after the asynchronous conversation finished.
	///
	/// [authenticated]: `Self::authenticate()`
	/// [authorized]: `Self::acct_mgmt()`
	#[rustversion::attr(since(1.48), doc(alias = "pam_open_session"))]
	pub fn open_session(&mut self, flags: Flag) -> Result<Session<'_, ConvT>> {
		let bits = flags.bits();
		let handle = self.handle().as_ptr();
		self.wrap_pam_return(unsafe {
			pam_setcred(handle, (Flag::ESTABLISH_CRED | flags).bits())
		})?;

		if let Err(e) = self.wrap_pam_return(unsafe { pam_open_session(handle, bits) }) {
			let _ = self.wrap_pam_return(unsafe {
				pam_setcred(handle, (Flag::DELETE_CRED | flags).bits())
			});
			return Err(e);
		}

		// Reinitialize credentials after session opening. With this we try
		// to circumvent different assumptions of PAM modules about when
		// `setcred` is called, as the documentations of different PAM
		// implementations differ. (OpenSSH does something similar too).
		if let Err(e) = self.wrap_pam_return(unsafe {
			pam_setcred(handle, (Flag::REINITIALIZE_CRED | flags).bits())
		}) {
			let _ = self.wrap_pam_return(unsafe { pam_close_session(handle, bits) });
			let _ = self.wrap_pam_return(unsafe {
				pam_setcred(handle, (Flag::DELETE_CRED | flags).bits())
			});
			return Err(e);
		}

		Ok(Session::new(self, true))
	}

	/// Maintains user credentials but don't set up a full user session.
	///
	/// Establishes user credentials and returns a [`Session`] object that
	/// deletes the credentials on drop. It doesn't open a PAM session.
	///
	/// The user should already be [authenticated] and [authorized] at this
	/// point, but this isn't enforced or strictly neccessary.
	///
	/// Depending on the platform this use case may not be fully supported.
	///
	/// Relevant `flags` are [`Flag::NONE`] and [`Flag::SILENT`].
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BUF_ERR` – Memory allocation error
	/// - `CRED_ERR` – Setting credentials failed
	/// - `CRED_UNAVAIL` – Failed to retrieve credentials
	/// - `SYSTEM_ERR` – Other system error
	/// - `USER_UNKNOWN` – User not known
	///
	/// [authenticated]: Self::authenticate()
	/// [authorized]: Self::acct_mgmt()
	pub fn open_pseudo_session(&mut self, flags: Flag) -> Result<Session<'_, ConvT>> {
		self.wrap_pam_return(unsafe {
			pam_setcred(self.handle().into(), (Flag::ESTABLISH_CRED | flags).bits())
		})?;

		Ok(Session::new(self, false))
	}

	/// Resume a session from a [`SessionToken`].
	pub fn unleak_session(&mut self, token: SessionToken) -> Session<'_, ConvT> {
		Session::new(self, matches!(token, SessionToken::FullSession))
	}
}

impl<ConvT> Context<ConvT> {
	/// Internal: Gets the PAM handle.
	#[inline]
	pub(crate) fn handle(&self) -> PamHandle {
		self.handle
	}

	/// Internal: Wraps a `ErrorCode` into a `Result` and sets `last_status`.
	#[inline]
	pub(crate) fn wrap_pam_return(&self, status: c_int) -> Result<()> {
		self.last_status.set(status);
		match status {
			PAM_SUCCESS => Ok(()),
			code => Err(Error::new(
				self.handle(),
				ErrorCode::from_repr(code).unwrap_or(ErrorCode::ABORT),
			)),
		}
	}

	/// Returns raw PAM information.
	///
	/// If possible, use the convenience wrappers [`service()`][`Self::service()`],
	/// [`user()`][`Self::user()`], … instead.
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BAD_ITEM` – Unsupported, undefined or inaccessible item
	/// - `BUF_ERR` – Memory buffer error
	/// - `PERM_DENIED` – The value was NULL/None
	#[rustversion::attr(since(1.48), doc(alias = "pam_get_item"))]
	pub fn get_item(&self, item_type: c_int) -> Result<*const c_void> {
		let mut result: *const c_void = ptr::null();
		self.wrap_pam_return(unsafe {
			pam_get_item(self.handle().into(), item_type, &mut result)
		})?;
		Ok(result)
	}

	/// Updates raw PAM information.
	///
	/// If possible, use the convenience wrappers
	/// [`set_service()`][`Self::set_service()`],
	/// [`set_user()`][`Self::set_user()`], … instead.
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BAD_ITEM` – Unsupported, undefined or inaccessible item
	/// - `BUF_ERR` – Memory buffer error
	///
	/// # Safety
	/// This method is unsafe. You must guarantee that the data pointed to by
	/// `value` matches the type the PAM library expects. E.g. a null terminated
	/// `*const c_char` for `PAM_SERVICE` or `*const PamXAuthData` for
	/// `PAM_XAUTHDATA`.
	#[rustversion::attr(since(1.48), doc(alias = "pam_set_item"))]
	pub unsafe fn set_item(&mut self, item_type: c_int, value: *const c_void) -> Result<()> {
		self.wrap_pam_return(pam_set_item(self.handle().into(), item_type, value))
	}

	/// Returns a pointer to the raw conversation handler
	///
	/// # Panics
	/// May panic if the type of the handler isn't `ConvT` or if somehow
	/// extracting the handler from the PAM handle fails.
	#[inline]
	fn conversation_raw(&self) -> *mut ConvT {
		let ptr = self
			.get_item(pam_sys::PAM_CONV as c_int)
			.expect("Extracting the conversation handler should never fail")
			.cast::<PamConversation>();
		unsafe {
			from_pam_conv(
				ptr.as_ref()
					.expect("Invalid state: conversation handler should never be null"),
			)
		}
	}

	/// Returns a reference to the conversation handler.
	pub fn conversation(&self) -> &ConvT {
		let ptr: *const ConvT = self.conversation_raw();
		// Safety: the conversation handler is only set by `from_boxed_conv()` or `replace_handler()`
		// and these maintain that the installed handler is valid and of the correct type.
		unsafe { &*ptr }
	}

	/// Returns a mutable reference to the conversation handler.
	pub fn conversation_mut(&mut self) -> &mut ConvT {
		let ptr = self.conversation_raw();
		// Safety: the conversation handler is only set by `from_boxed_conv()` or `replace_handler()`
		// and these maintain that the installed handler is valid and of the correct type.
		unsafe { &mut *ptr }
	}

	impl_pam_str_item!(
		service,
		set_service,
		pam_sys::PAM_SERVICE,
		"the service name"
	);
	impl_pam_str_item!(user, set_user, pam_sys::PAM_USER, "the username of the entity under whose identity service will be given",
		"This value can be mapped by any module in the PAM stack, so don't assume it stays unchanged after calling other methods on `Self`.");
	impl_pam_str_item!(
		user_prompt,
		set_user_prompt,
		pam_sys::PAM_USER_PROMPT,
		"the string used when prompting for a user's name"
	);
	impl_pam_str_item!(tty, set_tty, pam_sys::PAM_TTY, "the terminal name");
	impl_pam_str_item!(
		ruser,
		set_ruser,
		pam_sys::PAM_RUSER,
		"the requesting user name"
	);
	impl_pam_str_item!(
		rhost,
		set_rhost,
		pam_sys::PAM_RHOST,
		"the requesting hostname"
	);
	#[cfg(any(target_os = "linux", doc))]
	impl_pam_str_item!(
		authtok_type,
		set_authtok_type,
		pam_sys::PAM_AUTHTOK_TYPE,
		"the default password type in the prompt (Linux specific)",
		"E.g. \"UNIX\" for \"Enter UNIX password:\""
	);
	#[cfg(any(target_os = "linux", doc))]
	impl_pam_str_item!(
		xdisplay,
		set_xdisplay,
		pam_sys::PAM_XDISPLAY,
		"the name of the X display (Linux specific)"
	);

	/// Returns X authentication data as (name, value) pair (Linux specific).
	#[cfg(any(target_os = "linux", doc))]
	pub fn xauthdata(&self) -> Result<(&CStr, &[u8])> {
		let handle = self.handle();
		let ptr = self
			.get_item(pam_sys::PAM_XAUTHDATA as c_int)?
			.cast::<XAuthData>();
		if ptr.is_null() {
			return Err(Error::new(handle, ErrorCode::PERM_DENIED));
		}
		let data = unsafe { &*ptr };

		// Safety checks: validate the length are non-negative and that
		// the pointers are non-null
		if data.namelen < 0 || data.datalen < 0 || data.name.is_null() || data.data.is_null() {
			return Err(Error::new(handle, ErrorCode::BUF_ERR));
		}

		#[allow(clippy::cast_sign_loss)]
		Ok((
			CStr::from_bytes_with_nul(unsafe {
				slice::from_raw_parts(data.name.cast(), data.namelen as usize + 1)
			})
			.map_err(|_| Error::new(handle, ErrorCode::BUF_ERR))?,
			unsafe { slice::from_raw_parts(data.data.cast(), data.datalen as usize) },
		))
	}

	/// Sets X authentication data (Linux specific).
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BAD_ITEM` – Unsupported item
	/// - `BUF_ERR` – Memory buffer error
	#[cfg(any(target_os = "linux", doc))]
	pub fn set_xauthdata(&mut self, value: Option<(&CStr, &[u8])>) -> Result<()> {
		match value {
			None => unsafe { self.set_item(pam_sys::PAM_XAUTHDATA as c_int, ptr::null()) },
			Some((name, data)) => {
				let name_bytes = name.to_bytes_with_nul();

				if name_bytes.len() > i32::MAX as usize || data.len() > i32::MAX as usize {
					return Err(Error::new(self.handle(), ErrorCode::BUF_ERR));
				}

				#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
				let xauthdata = XAuthData {
					namelen: name_bytes.len() as i32 - 1,
					name: name_bytes.as_ptr().cast(),
					datalen: data.len() as i32,
					data: data.as_ptr().cast(),
				};
				unsafe {
					self.set_item(
						pam_sys::PAM_XAUTHDATA as c_int,
						&xauthdata as *const _ as *const c_void,
					)
				}
			}
		}
	}

	/// Returns the value of a PAM environment variable.
	///
	/// Searches the environment list in this PAM context for an
	/// item with key `name` and returns the value, if it exists.
	#[must_use]
	#[rustversion::attr(since(1.48), doc(alias = "pam_getenv"))]
	pub fn getenv(&self, name: impl AsRef<OsStr>) -> Option<&str> {
		let c_name = match CString::new(name.as_ref().as_bytes()) {
			Err(_) => return None,
			Ok(s) => s,
		};
		char_ptr_to_str(unsafe { pam_getenv(self.handle().into(), c_name.as_ptr()) })
	}

	/// Sets or unsets a PAM environment variable.
	///
	/// Modifies the environment list in this PAM context. The `name_value`
	/// argument can take one of the following forms:
	/// - *NAME*=*value* – Set a variable to a given value. If it was already
	///   set it is overwritten.
	/// - *NAME*= – Set a variable to the empty value. If it was already set
	///   it is overwritten.
	/// - *NAME* – Delete a variable, if it exists.
	#[rustversion::attr(since(1.48), doc(alias = "pam_putenv"))]
	pub fn putenv(&mut self, name_value: impl AsRef<OsStr>) -> Result<()> {
		let c_name_value = CString::new(name_value.as_ref().as_bytes())
			.map_err(|_| Error::from(ErrorCode::BUF_ERR))?;
		self.wrap_pam_return(unsafe { pam_putenv(self.handle().into(), c_name_value.as_ptr()) })
	}

	/// Returns a copy of the PAM environment in this context.
	///
	/// The contained variables represent the contents of the regular
	/// environment variables of the authenticated user when service is
	/// granted.
	///
	/// The returned [`EnvList`] type is designed to ease handing the
	/// environment to [`std::process::Command::envs()`] and
	/// `nix::unistd::execve()`.
	#[must_use]
	#[rustversion::attr(since(1.48), doc(alias = "pam_getenvlist"))]
	pub fn envlist(&self) -> EnvList {
		unsafe { EnvList::new(pam_getenvlist(self.handle().into()).cast()) }
	}

	/// Swap the conversation handler.
	///
	/// Consumes the context, returns the new context and the old conversation
	/// handler.
	///
	/// # Errors
	/// Expected error codes include:
	/// - `BAD_ITEM` – Swapping the conversation handler is unsupported
	/// - `BUF_ERR` – Memory buffer error
	///
	/// The error payload contains the old context and the new handler.
	pub fn replace_conversation<T: ConversationHandler>(
		self,
		new_handler: T,
	) -> ExtResult<(Context<T>, ConvT), (Self, T)> {
		match self.replace_conversation_boxed(new_handler.into()) {
			Ok((context, boxed_old_conv)) => Ok((context, *boxed_old_conv)),
			Err(error) => Err(error.map(|(ctx, b_conv)| (ctx, *b_conv))),
		}
	}
	/// Swap the conversation handler (boxed variant).
	///
	/// See [`replace_conversation()`][`Self::replace_conversation()`].
	pub fn replace_conversation_boxed<T: ConversationHandler>(
		mut self,
		new_handler: Box<T>,
	) -> ExtResult<(Context<T>, Box<ConvT>), (Self, Box<T>)> {
		// Get current username for handler initialization
		let username = match self.user() {
			Ok(u) => Some(u),
			Err(e) => {
				if e.code() != ErrorCode::PERM_DENIED {
					return Err(e.into_with_payload((self, new_handler)));
				}
				None
			}
		};
		// Get pointer to old handler
		let old_handler_ptr = self.conversation_raw();

		// Swap handler
		let pam_conv = into_pam_conv(new_handler);
		if let Err(e) = unsafe {
			self.set_item(
				pam_sys::PAM_CONV as c_int,
				&pam_conv as *const _ as *const _,
			)
		} {
			let new_handler = unsafe { Box::from_raw(from_pam_conv(&pam_conv)) };
			Err(e.into_with_payload((self, new_handler)))
		} else {
			// Prevent dropping of the old context
			let old = ManuallyDrop::new(self);

			// Reconstruct old handler from saved pointer
			// Safety: The handler was replaced in the context, so there
			// is no other way to access the old handler anymore and
			// the pointer was originally constructed with `Box::into_raw()`.
			let old_handler = unsafe { Box::from_raw(old_handler_ptr) };

			// Create new context
			let mut context = Context::<T> {
				handle: old.handle,
				last_status: Cell::new(old.last_status.replace(PAM_SUCCESS)),
				_conversation: PhantomData,
			};

			// Initialize handler
			context.conversation_mut().init(username.as_deref());

			// Return context and old handler
			Ok((context, old_handler))
		}
	}
}

/// Destructor ending the PAM transaction and releasing the PAM context
impl<ConvT> Drop for Context<ConvT> {
	#[rustversion::attr(since(1.48), doc(alias = "pam_end"))]
	fn drop(&mut self) {
		let conv = self.conversation_raw();
		unsafe { pam_end(self.handle.into(), self.last_status.get()) };
		drop(unsafe { Box::from_raw(conv) });
	}
}

// `Send` should be possible, as long as `ConvT` is `Send` too, as all memory
// access is bound to an unique instance of `Context` (no copy/clone) and we
// keep interior mutability bound to having a reference to the instance.
unsafe impl<ConvT> Send for Context<ConvT> where ConvT: Send {}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::{OsStr, OsString};

	#[test]
	fn test_basic() {
		let mut context =
			Context::new("test", Some("user"), crate::conv_null::Conversation::new()).unwrap();
		// Check if user name and service name are correctly saved
		assert_eq!(context.service().unwrap(), "test");
		assert_eq!(context.user().unwrap(), "user");
		// Check basic properties of PamHandle
		let h = context.handle();
		assert_eq!(&h.0, &h.0);
		assert!(format!("{:?}", h).contains(&format!("{:?}", h.as_ptr())));
		// Check setting/getting of string items.
		context.set_user_prompt(Some("Who art thou? ")).unwrap();
		assert_eq!(context.user_prompt().unwrap(), "Who art thou? ");
		context.set_tty(Some("/dev/tty")).unwrap();
		assert_eq!(context.tty().unwrap(), "/dev/tty");
		context.set_ruser(Some("nobody")).unwrap();
		assert_eq!(context.ruser().unwrap(), "nobody");
		context.set_rhost(Some("nowhere")).unwrap();
		assert_eq!(context.rhost().unwrap(), "nowhere");
		// Check linux specific items
		#[cfg(target_os = "linux")]
		{
			context.set_authtok_type(Some("TEST")).unwrap();
			assert_eq!(context.authtok_type().unwrap(), "TEST");
			context.set_xdisplay(Some(":0")).unwrap();
			assert_eq!(context.xdisplay().unwrap(), ":0");
			let xauthname = CString::new("TEST_DATA").unwrap();
			let xauthdata = [];
			let _ = context.xauthdata();
			context
				.set_xauthdata(Some((&xauthname, &xauthdata)))
				.unwrap();
			let (resultname, resultdata) = context.xauthdata().unwrap();
			assert_eq!(resultname, xauthname.as_c_str());
			assert_eq!(resultdata, &xauthdata);
		};
		// Check accessing the conversation handler
		assert_eq!(
			context.conversation_mut() as *mut _ as *const _,
			context.conversation() as *const _
		);
		context
			.conversation_mut()
			.text_info(&CString::new("").unwrap());
		// Check getting an unaccessible item
		assert!(context.get_item(pam_sys::PAM_AUTHTOK as c_int).is_err());
		// Check environment setting/getting
		context.putenv("TEST=1").unwrap();
		context.putenv("TEST2=2").unwrap();
		let _ = context.putenv("\0=\0").unwrap_err();
		assert_eq!(context.getenv("TEST").unwrap(), "1");
		assert!(context.getenv("TESTNONEXIST").is_none());
		let env = context.envlist();
		assert!(!env.is_empty());
		let _ = env.get("TEST").unwrap();
		let _ = env.get("TESTNONEXIST").is_none();
		for (key, value) in env.iter_tuples() {
			if key.to_string_lossy() == "TEST" {
				assert_eq!(value.to_string_lossy(), "1");
			}
		}
		assert!(format!("{:?}", &env.iter_tuples()).contains("EnvItem"));
		for item in &env {
			let string = item.to_string();
			if string.starts_with("TEST=") {
				assert_eq!(string, "TEST=1");
				assert!(format!("{:?}", &item).contains("EnvItem"));
			} else if string.starts_with("TEST2=") {
				let (_, v): (&OsStr, &OsStr) = item.into();
				assert_eq!(v.to_string_lossy(), "2");
			}
			let _ = item.as_ref();
		}
		let _ = format!("{:?}", &env);
		assert!(!env.is_empty());
		assert_eq!(env.len(), env.as_ref().len());
		assert_eq!(env.as_ref(), context.envlist().as_ref());
		assert_eq!(
			env.as_ref().partial_cmp(context.envlist().as_ref()),
			Some(std::cmp::Ordering::Equal)
		);
		assert_eq!(
			env.as_ref().cmp(context.envlist().as_ref()),
			std::cmp::Ordering::Equal
		);
		assert_eq!(&env["TEST"], "1");
		assert_eq!(env.len(), env.iter_tuples().size_hint().0);
		let list: std::vec::Vec<&CStr> = (&env).into();
		assert_eq!(list.len(), env.len());
		let list: std::vec::Vec<(&OsStr, _)> = (&env).into();
		assert_eq!(list.len(), env.len());
		let map: std::collections::HashMap<&OsStr, _> = (&env).into();
		assert_eq!(map.len(), map.len());
		assert_eq!(
			map.get(&OsString::from("TEST".to_string()).as_ref()),
			Some(&OsString::from("1".to_string()).as_ref())
		);
		assert!(env.to_string().contains("TEST=1"));
		let list: std::vec::Vec<(std::ffi::OsString, _)> = context.envlist().into();
		assert_eq!(list.len(), env.len());
		let list: std::vec::Vec<CString> = context.envlist().into();
		assert_eq!(list.len(), env.len());
		let map: std::collections::HashMap<_, _> = context.envlist().into();
		assert_eq!(map.len(), env.len());
		assert_eq!(
			map.get(&OsString::from("TEST".to_string())),
			Some(&OsString::from("1".to_string()))
		);
		drop(context)
	}

	#[test]
	fn test_conv_replace() {
		let mut context =
			Context::new("test", Some("user"), crate::conv_null::Conversation::new()).unwrap();
		// Set username
		context.set_user(Some("anybody")).unwrap();
		// Replace conversation handler
		let (mut context, old_conv) = context
			.replace_conversation(crate::conv_mock::Conversation::default())
			.unwrap();
		// Check if set username was propagated to the new handler
		assert_eq!(context.conversation().username, "anybody");
		context.set_user(None).unwrap();
		let (context, _) = context.replace_conversation(old_conv).unwrap();
		// Check if username stays None after being set througout a replace.
		assert!(context.user().is_err());
	}

	#[test]
	fn test_dyn_ref() {
		let mut handler_a = crate::conv_null::Conversation::new();
		let mut handler_b = crate::conv_mock::Conversation::new();

		let mut context = Context::new(
			"test",
			Some("user"),
			&mut handler_a as &mut dyn ConversationHandler,
		)
		.unwrap();
		// Set username
		context.set_user(Some("anybody")).unwrap();
		// Replace conversation handler and drop reference to `handler_a`
		let (context, _) = context
			.replace_conversation(
				&mut handler_b as &mut dyn ConversationHandler
			)
			.unwrap();

		// Assert that handler_a is accessible again
		drop(handler_a);
		
		// drop context
		drop(context);

		// Check if set username was propagated to `handler_b`
		assert_eq!(handler_b.username, "anybody");
	}

	#[test]
	fn test_dyn() {
		let mut context = Context::new(
			"test",
			Some("user"),
			Box::new(crate::conv_null::Conversation::new()) as Box<dyn ConversationHandler>,
		)
		.unwrap();
		// Set username
		context.set_user(Some("anybody")).unwrap();
		// Replace conversation handler
		let (context, _) = context
			.replace_conversation(
				Box::new(crate::conv_mock::Conversation::new()) as Box<dyn ConversationHandler>
			)
			.unwrap();

		// Check if set username was propagated to the new handler
		// Safety: we know the type from four lines above, so this unchecked downcast is sound.
		let mock_handler: &crate::conv_mock::Conversation =
			unsafe { &*(&**context.conversation() as *const _ as *const _) };
		assert_eq!(mock_handler.username, "anybody");
	}

	/// Shallowly tests a full authentication + password change + session cycle.
	///
	/// This will fail if the environment is not appropriately
	/// prepared and the test process has no elevated rights.
	/// Currently it is only checked if some function crashes
	/// or panics, not if the authentication succeeds.
	#[test]
	#[cfg_attr(not(feature = "full_test"), ignore)]
	fn test_full() {
		let mut context = Context::new(
			"test_rust_pam_client",
			Some("nobody"),
			crate::conv_null::Conversation::new(),
		)
		.unwrap();
		let _ = context.authenticate(Flag::SILENT);
		let _ = context.acct_mgmt(Flag::SILENT);
		let _ = context.chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK);
		let _ = context.reinitialize_credentials(Flag::SILENT | Flag::NONE);
		drop(context.open_session(Flag::SILENT));
		drop(context.open_pseudo_session(Flag::SILENT));
	}

	/// Shallowly tests a full session cycle without authentication.
	///
	/// This will fail if the environment is not appropriately
	/// prepared and the test process has no elevated rights.
	/// Currently it is only checked if some function crashes
	/// or panics, not if anything really succeeds.
	///
	/// Some operating systems besides Linux don't support this mode of operation.
	#[test]
	#[cfg_attr(not(feature = "full_test"), ignore)]
	fn test_full_unauth() {
		let mut context = Context::new(
			"test_rust_pam_client",
			Some("nobody"),
			crate::conv_null::Conversation::new(),
		)
		.unwrap();
		let _ = context.acct_mgmt(Flag::SILENT);
		let _ = context.chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK);
		let _ = context.reinitialize_credentials(Flag::SILENT | Flag::NONE);
		if let Ok(mut session) = context.open_session(Flag::SILENT) {
			let _ = session.refresh_credentials(Flag::SILENT);
			let _ = session.reinitialize_credentials(Flag::SILENT);
			let _ = session.envlist();
			let _ = session.close(Flag::SILENT);
		};
		if let Ok(mut session) = context.open_pseudo_session(Flag::SILENT) {
			let _ = session.refresh_credentials(Flag::SILENT);
		};
	}
}
