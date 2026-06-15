//! Error structs and related helpers

/***********************************************************************
 * (c) 2021-2022 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>*
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

use crate::char_ptr_to_str;
use crate::context::PamHandle;
#[doc(no_inline)]
pub use crate::ErrorCode;
use pam_sys::pam_strerror;

use std::any::type_name;
use std::cmp::{Eq, PartialEq};
use std::error;
use std::fmt::{Debug, Display, Formatter, Result as FmtResult};
use std::hash::{Hash, Hasher};
use std::io;
use std::marker::PhantomData;

/// The error payload type for errors that never have payloads.
///
/// Like `std::convert::Infallible` but with a less confusing name, given
/// the context it's used in here. Might become a type alias to `!` when
/// the [`!` never type](https://doc.rust-lang.org/std/primitive.never.html)
/// is stabilized.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NoPayload {}

impl Display for NoPayload {
	fn fmt(&self, _: &mut Formatter<'_>) -> FmtResult {
		match *self {}
	}
}

impl PartialEq for NoPayload {
	fn eq(&self, _: &NoPayload) -> bool {
		match *self {}
	}
}

impl Eq for NoPayload {}

impl Hash for NoPayload {
	fn hash<H: Hasher>(&self, _: &mut H) {
		match *self {}
	}
}

/// Helper to implement `Debug` on `ErrorWith` with `T` not implementing `Debug`
enum DisplayHelper<T> {
	Some(PhantomData<T>),
	None,
}

impl<T> DisplayHelper<T> {
	#[inline]
	fn new(option: &Option<T>) -> Self {
		match option {
			None => Self::None,
			Some(_) => Self::Some(PhantomData),
		}
	}
}

impl<T> Debug for DisplayHelper<T> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		match *self {
			Self::None => write!(f, "None"),
			Self::Some(_) => write!(f, "<{}>", type_name::<T>()),
		}
	}
}

/// Base error type for PAM operations (possibly with a payload)
///
/// Errors originate from the PAM library, PAM modules or helper structs
/// in this crate. Currently no custom instances are supported.
#[must_use]
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ErrorWith<T> {
	code: ErrorCode,
	msg: String,
	payload: Option<T>,
}

impl<T> ErrorWith<T> {
	/// Creates a new [`Error`] that takes a payload.
	///
	/// Functions that consume a struct can use the payload to transfer back
	/// ownership in error cases.
	pub(crate) fn with_payload(
		handle: PamHandle,
		code: ErrorCode,
		payload: Option<T>,
	) -> ErrorWith<T> {
		Self {
			code,
			msg: char_ptr_to_str(unsafe { pam_strerror(handle.into(), code.repr()) })
				.unwrap_or("")
				.into(),
			payload,
		}
	}

	/// The error code.
	pub const fn code(&self) -> ErrorCode {
		self.code
	}

	/// Text representation of the error code, if available.
	pub fn message(&self) -> Option<&str> {
		if self.msg.is_empty() {
			None
		} else {
			Some(&self.msg)
		}
	}

	/// Returns a reference to an optional payload.
	#[rustversion::attr(since(1.48), const)]
	pub fn payload(&self) -> Option<&T> {
		self.payload.as_ref()
	}

	/// Takes the payload out of the error message.
	///
	/// If a payload exists in this error, it will be moved into the returned
	/// [`Option`]. All further calls to [`payload()`][`Self::payload()`] and
	/// [`take_payload()`][`Self::take_payload()`] will return [`None`].
	pub fn take_payload(&mut self) -> Option<T> {
		match self.payload {
			Some(_) => self.payload.take(),
			None => None,
		}
	}

	/// Maps the error payload to another type
	pub fn map<U>(self, func: impl FnOnce(T) -> U) -> ErrorWith<U> {
		ErrorWith::<U> {
			code: self.code,
			msg: self.msg,
			payload: self.payload.map(func),
		}
	}

	/// Removes the payload and converts to [`Error`]
	#[inline]
	pub fn into_without_payload(self) -> Error {
		Error {
			code: self.code,
			msg: self.msg,
			payload: None,
		}
	}
}

impl<T> Debug for ErrorWith<T> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		// Hacky and not always correct, but the best we can do for now
		// without specialization
		if type_name::<T>() == type_name::<NoPayload>() {
			f.debug_struct("pam_client2::Error")
				.field("code", &self.code)
				.field("msg", &self.msg)
				.finish()
		} else {
			f.debug_struct("pam_client2::ErrorWith")
				.field("code", &self.code)
				.field("msg", &self.msg)
				.field("payload", &DisplayHelper::new(&self.payload))
				.finish()
		}
	}
}

/// Error type for PAM operations without error payload.
///
/// This variant never contains a payload.
pub type Error = ErrorWith<NoPayload>;

impl Error {
	/// Creates a new [`Error`].
	pub(crate) fn new(handle: PamHandle, code: ErrorCode) -> Error {
		Self::with_payload(handle, code, None)
	}

	/// Adds the payload to the error message and returns a corresponding
	/// [`ErrorWith<T>`] instance.
	pub fn into_with_payload<T>(self, payload: T) -> ErrorWith<T> {
		ErrorWith::<T> {
			code: self.code,
			msg: self.msg,
			payload: Some(payload),
		}
	}

	/// Converts the error message into a [`ErrorWith<T>`] instance without
	/// a payload.
	pub fn into<T>(self) -> ErrorWith<T> {
		ErrorWith::<T> {
			code: self.code,
			msg: self.msg,
			payload: None,
		}
	}
}

impl<T> Display for ErrorWith<T> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		if self.msg.is_empty() {
			write!(f, "<{}>", self.code as i32)
		} else {
			f.write_str(&self.msg)
		}
	}
}

impl<T> error::Error for ErrorWith<T> {}

impl<T> PartialEq for ErrorWith<T>
where
	T: PartialEq,
{
	fn eq(&self, other: &Self) -> bool {
		self.code == other.code && self.payload == other.payload
	}
}

impl<T> Eq for ErrorWith<T> where T: Eq {}

impl<T> Hash for ErrorWith<T>
where
	T: Hash,
{
	fn hash<H: Hasher>(&self, state: &mut H) {
		(self.code as i32).hash(state);
		self.payload.hash(state);
	}
}

/// Wrapping of a [`ErrorCode`] in a [`Error`] without a PAM context.
///
/// This is used internally to construct [`Error`] instances when no PAM
/// context is available. These instances won't have a message string, only
/// a code.
///
/// Examples:
/// ```rust
/// # use pam_client2::{Error, ErrorCode};
///
/// let error = Error::from(ErrorCode::ABORT);
/// println!("{:?}", error);
/// ```
/// ```rust
/// # use pam_client2::{Error, ErrorCode};
///
/// let error: Error = ErrorCode::ABORT.into();
/// println!("{:?}", error);
/// ```
impl From<ErrorCode> for Error {
	#[inline]
	fn from(code: ErrorCode) -> Self {
		Error {
			code,
			msg: String::new(),
			payload: None,
		}
	}
}

/// Automatic wrapping in [`std::io::Error`] (if payload type is compatible).
///
/// ```rust
/// # use std::convert::TryInto;
/// # use pam_client2::{Result, Error, ErrorCode};
/// # fn some_succeeding_pam_function() -> Result<()> { Ok(()) }
/// fn main() -> std::result::Result<(), std::io::Error> {
///     some_succeeding_pam_function()?;
///     Ok(())
/// }
/// ```
/// ```rust,should_panic
/// # use std::convert::{Infallible, TryInto};
/// # use pam_client2::{Result, Error, ErrorCode};
/// # fn some_failing_pam_function() -> Result<Infallible> {
/// #     Err(ErrorCode::ABORT.into())
/// # }
/// fn main() -> std::result::Result<(), std::io::Error> {
///     some_failing_pam_function()?;
///     Ok(())
/// }
/// ```
impl<T: Send + Sync + Debug + 'static> From<ErrorWith<T>> for io::Error {
	fn from(error: ErrorWith<T>) -> Self {
		io::Error::new(
			match error.code {
				#[cfg(not(openpam))]
				ErrorCode::INCOMPLETE => io::ErrorKind::Interrupted,
				ErrorCode::BAD_ITEM | ErrorCode::USER_UNKNOWN => io::ErrorKind::NotFound,
				ErrorCode::CRED_INSUFFICIENT | ErrorCode::PERM_DENIED => {
					io::ErrorKind::PermissionDenied
				}
				_ => io::ErrorKind::Other,
			},
			Box::new(error),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::conv_null::Conversation;
	use crate::Context;

	#[test]
	fn test_basic() {
		let context = Context::new("test", None, Conversation::default()).unwrap();
		let error = Error::new(context.handle(), ErrorCode::CONV_ERR).into_with_payload("foo");
		assert_eq!(error.payload(), Some(&"foo"));
		assert!(error.message().is_some());
		assert!(format!("{:?}", error).len() > 1);
		let mut error = error.map(|_| usize::MIN);
		assert_eq!(error.payload(), Some(&usize::MIN));
		let _ = error.take_payload();
		assert_eq!(error.take_payload(), None);
		assert!(format!("{:?}", error).contains("None"));
		let error = error.map(|_| usize::MIN);
		assert_eq!(error.payload(), None);
		let error = error.into_without_payload();
		assert_eq!(error.payload(), None);
		assert!(format!("{:?} {}", error, error).len() > 4);
		assert_eq!(io::Error::from(error).kind(), io::ErrorKind::Other);
	}

	#[test]
	fn test_no_msg() {
		let error = Error::from(ErrorCode::BAD_ITEM);
		assert_eq!(
			format!("{}", error),
			format!("<{}>", (ErrorCode::BAD_ITEM as i32))
		);
		assert_eq!(format!("{:?}", &error), format!("{:?}", error.clone()));
		assert!(error.message().is_none());
		let error: ErrorWith<()> = error.into();
		assert_eq!(io::Error::from(error).kind(), io::ErrorKind::NotFound);
	}

	/// Check if a hash can be calculated and equality works
	#[test]
	fn test_traits() {
		use std::collections::hash_map::DefaultHasher;
		use std::hash::{Hash, Hasher};

		fn calc_hash<T: Hash>(t: &T) -> u64 {
			let mut s = DefaultHasher::new();
			t.hash(&mut s);
			s.finish()
		}

		let error = Error::from(ErrorCode::BUF_ERR);

		assert_eq!(calc_hash(&error), calc_hash(&error.clone()));
		assert_eq!(&error, &error.clone());
	}
}
