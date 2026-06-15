//! PAM environment list

/***********************************************************************
 * (c) 2021 Christoph Grenz <christophg+gitorious @ grenz-bonn.de>     *
 *                                                                     *
 * This Source Code Form is subject to the terms of the Mozilla Public *
 * License, v. 2.0. If a copy of the MPL was not distributed with this *
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.            *
 ***********************************************************************/

use crate::c_box::CBox;
use libc::c_char;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::iter::FusedIterator;
use std::ops::Index;
use std::os::unix::ffi::OsStrExt;
use std::{fmt, slice};

/// Item in a PAM environment list.
///
/// A key-value pair representing an environment variable in a [`EnvList`],
/// convertible to `&CStr` and to a pair (key, value) of `&OsStr`'s.
///
/// As this struct is tightly coupled to the memory management of [`EnvList`]
/// no constructor for custom instances is provided.
#[repr(transparent)]
#[derive(Debug)]
pub struct EnvItem(CBox<c_char>);

impl EnvItem {
	/// Returns a [`CStr`] reference to the `"key=value"` representation.
	#[must_use]
	pub fn as_cstr(&self) -> &CStr {
		unsafe { CStr::from_ptr(self.0.as_ref()) }
	}

	/// Returns a pair of references to a `("key", "value")` representation.
	#[must_use]
	pub fn key_value(&self) -> (&OsStr, &OsStr) {
		let element = <&CStr>::from(self).to_bytes();
		let sep = element
			.iter()
			.position(|b| *b == b'=')
			.unwrap_or(element.len());
		(
			OsStr::from_bytes(&element[..sep]),
			OsStr::from_bytes(&element[sep + 1..]),
		)
	}
}

/// Display and string conversion of the environment variable.
///
/// Also causes `.to_string()` to be implemented.
impl fmt::Display for EnvItem {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", <&CStr>::from(self).to_string_lossy())
	}
}

impl<'a> From<&'a EnvItem> for &'a CStr {
	#[inline]
	fn from(item: &'a EnvItem) -> Self {
		item.as_cstr()
	}
}

impl<'a> From<&'a EnvItem> for (&'a OsStr, &'a OsStr) {
	fn from(item: &'a EnvItem) -> Self {
		item.key_value()
	}
}

impl AsRef<CStr> for EnvItem {
	#[inline]
	fn as_ref(&self) -> &CStr {
		self.as_cstr()
	}
}

impl PartialEq for EnvItem {
	#[inline]
	fn eq(&self, other: &Self) -> bool {
		PartialEq::eq(self.as_cstr(), other.as_cstr())
	}
}

impl Eq for EnvItem {}

impl PartialOrd for EnvItem {
	#[inline]
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		PartialOrd::partial_cmp(self.as_cstr(), other.as_cstr())
	}
}

impl Ord for EnvItem {
	#[inline]
	fn cmp(&self, other: &Self) -> Ordering {
		Ord::cmp(self.as_cstr(), other.as_cstr())
	}
}

/// Serializes as a (key, value) OsStr tuple.
#[cfg(feature = "serde")]
impl serde::Serialize for EnvItem {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.key_value().serialize(serializer)
	}
}

/// Helper function: determine the length of the array
unsafe fn count_items<T: ?Sized>(mut ptr: *const *const T) -> usize {
	let mut result: usize = 0;
	while !(*ptr).is_null() {
		ptr = ptr.add(1);
		result += 1;
	}
	result
}

/// A PAM environment list
///
/// The PAM environment represents the contents of the regular environment
/// variables of the authenticated user when service is granted and can be
/// used to prepare the environment of child processes run as the authenticated
/// user.
///
/// See [`Context::envlist()`][`crate::Context::envlist()`].
///
/// # Examples
/// ```rust
/// # use pam_client2::Context;
/// # let handler = pam_client2::conv_mock::Conversation::with_credentials("test".to_string(), "test".to_string());
/// # let mut context = Context::new("dummy", None, handler).unwrap();
/// // Print the PAM environment
/// for item in &context.envlist() {
///     println!("VAR: {}", item);
/// }
/// ```
///
/// The environment can be passed to [`std::process::Command`] by using [`EnvList::iter_tuples()`]:
/// ```no_run
/// use std::process::Command;
/// # use pam_client2::Context;
/// # let handler = pam_client2::conv_mock::Conversation::with_credentials("test".to_string(), "test".to_string());
/// # let mut context = Context::new("dummy", None, handler).unwrap();
///
/// // Spawn a process in the PAM environment
/// let command = Command::new("/usr/bin/some_program")
///                       .env_clear()
///                       .envs(context.envlist().iter_tuples());
/// ```
///
/// The environment can be passed to NIX's `execve` by using [`EnvList::as_ref()`]:
/// ```no_run
/// # // mock execve so that the doctest compiles
/// # pub mod nix { pub mod unistd {
/// #     use std::convert::Infallible;
/// #     use std::ffi::{CString, CStr};
/// #     pub fn execve<SA: AsRef<CStr>, SE: AsRef<CStr>>(path: &CStr, args: &[SA], env: &[SE]) -> Result<Infallible, ()> { panic!() }
/// # } }
/// # use std::ffi::CString;
/// # use pam_client2::Context;
/// # let handler = pam_client2::conv_mock::Conversation::with_credentials("test".to_string(), "test".to_string());
/// # let mut context = Context::new("dummy", None, handler).unwrap();
/// use nix::unistd::execve;
///
/// // Replace this process with another program in the PAM environment
/// execve(
///     &CString::new("/usr/bin/some_program").unwrap(),
///     &[CString::new("some_program").unwrap()],
///     context.envlist().as_ref()
/// ).expect("replacing the current process failed");
/// ```
#[derive(Debug)]
pub struct EnvList(CBox<[EnvItem]>);

impl EnvList {
	/// Creates an `EnvList` from a pointer as returned by
	/// `pam_getenvlist()`.
	///
	/// # Panics
	/// Panics if `data` is null.
	#[must_use]
	pub(crate) unsafe fn new(data: *mut *mut c_char) -> Self {
		assert!(!data.is_null());
		let len = count_items(data as *const *const c_char);
		Self(CBox::from_raw_slice(data.cast(), len))
	}

	/// Returns a reference to the value of the named environment variable.
	///
	/// Returns `None` if the variable doesn't exist in this list.
	#[must_use]
	pub fn get<T: AsRef<OsStr>>(&self, name: T) -> Option<&OsStr> {
		#[inline]
		fn get<'a>(list: &'a EnvList, name: &'_ OsStr) -> Option<&'a OsStr> {
			list.iter_tuples()
				.find_map(|(k, v)| if k == name { Some(v) } else { None })
		}
		get(self, name.as_ref())
	}

	/// Returns an iterator over all contained variables as [`EnvItem`]s.
	///
	/// The iteration happens in deterministic, but unspecified order.
	#[inline]
	pub fn iter(&self) -> Iter<'_> {
		self.0.iter()
	}

	/// Returns the count of environment variables in the list.
	#[inline]
	#[must_use]
	pub fn len(&self) -> usize {
		self.0.len()
	}

	/// Returns `true` if the environment list is empty.
	#[inline]
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	/// Returns an iterator over all contained variables as
	/// `(key: &OsStr, value: &OsStr)` tuples.
	///
	/// The iteration happens in deterministic, but unspecified order.
	///
	/// Provides compatibility with [`std::process::Command::envs()`].
	#[inline]
	pub fn iter_tuples(&self) -> TupleIter<'_> {
		TupleIter(self.0.iter())
	}
}

/// Display and string conversion of the environment list.
///
/// Also causes `.to_string()` to be implemented.
impl fmt::Display for EnvList {
	/// Formats the environment list as a multi-line string
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		for item in self.0.iter() {
			writeln!(f, "{}", item.as_cstr().to_string_lossy())?;
		}
		Ok(())
	}
}

/// Provide direct `for`-loop support.
///
/// See [`EnvList::iter()`].
impl<'a> IntoIterator for &'a EnvList {
	type Item = &'a EnvItem;
	type IntoIter = Iter<'a>;

	fn into_iter(self) -> Self::IntoIter {
		self.iter()
	}
}

/// Provide compatibility with the 3rd parameter of `nix::unistd::execve`.
impl AsRef<[EnvItem]> for EnvList {
	#[inline]
	fn as_ref(&self) -> &[EnvItem] {
		&self.0
	}
}

/// Conversion to a vector of (key, value) tuples.
impl From<EnvList> for Vec<(OsString, OsString)> {
	fn from(list: EnvList) -> Self {
		let mut vec = Vec::with_capacity(list.len());
		for (key, value) in list.iter_tuples() {
			vec.push((key.to_owned(), value.to_owned()));
		}
		vec
	}
}

/// Reference conversion to a vector of (&key, &value) tuples.
impl<'a> From<&'a EnvList> for Vec<(&'a OsStr, &'a OsStr)> {
	fn from(list: &'a EnvList) -> Self {
		list.iter_tuples().collect()
	}
}

/// Conversion to a vector of "key=value" `CString`s.
impl From<EnvList> for Vec<CString> {
	fn from(list: EnvList) -> Self {
		let mut vec = Vec::with_capacity(list.len());
		for item in list.0.iter() {
			vec.push(item.as_cstr().to_owned());
		}
		vec
	}
}

/// Reference conversion to a vector of "key=value" `&CStr`s.
impl<'a> From<&'a EnvList> for Vec<&'a CStr> {
	fn from(list: &'a EnvList) -> Self {
		let mut vec = Vec::with_capacity(list.len());
		for item in list.0.iter() {
			vec.push(item.as_cstr());
		}
		vec
	}
}

/// Conversion to a hash map
impl<S> From<EnvList> for HashMap<OsString, OsString, S>
where
	S: ::std::hash::BuildHasher + Default,
{
	fn from(list: EnvList) -> Self {
		let mut map = HashMap::<_, _, S>::with_capacity_and_hasher(list.len(), S::default());
		for (key, value) in list.iter_tuples() {
			map.insert(key.to_owned(), value.to_owned());
		}
		map
	}
}

/// Reference conversion to a referencing hash map
impl<'a, S> From<&'a EnvList> for HashMap<&'a OsStr, &'a OsStr, S>
where
	S: ::std::hash::BuildHasher + Default,
{
	fn from(list: &'a EnvList) -> Self {
		list.iter_tuples().collect()
	}
}

/// Indexing with `list[key]`
impl<T: AsRef<OsStr>> Index<T> for EnvList {
	type Output = OsStr;

	/// Returns a reference to the value of the named environment variable.
	///
	/// # Panics
	/// Panics if the environment variable is not present in the `EnvList`.
	fn index(&self, name: T) -> &Self::Output {
		self.get(name).expect("environment variable not found")
	}
}

/// Serializes as a list of (key, value) OsStr tuples.
#[cfg(feature = "serde")]
impl serde::Serialize for EnvList {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.0.serialize(serializer)
	}
}

/// Iterator over [`EnvItem`]s in an [`EnvList`].
pub type Iter<'a> = slice::Iter<'a, EnvItem>;

/// Iterator over [`EnvItem`]s converted to `(key: &OsStr, value: &OsStr)`
/// tuples.
///
/// Returned by [`EnvList::iter_tuples()`].
#[must_use]
#[derive(Debug)]
pub struct TupleIter<'a>(slice::Iter<'a, EnvItem>);

impl<'a> Iterator for TupleIter<'a> {
	type Item = (&'a OsStr, &'a OsStr);

	fn next(&mut self) -> Option<Self::Item> {
		self.0.next().map(EnvItem::key_value)
	}

	#[inline]
	fn size_hint(&self) -> (usize, Option<usize>) {
		self.0.size_hint()
	}
}

impl FusedIterator for TupleIter<'_> {}
impl ExactSizeIterator for TupleIter<'_> {}
