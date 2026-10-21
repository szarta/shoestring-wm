//! Minimal PAM client: verify one user's password through one service.
//!
//! This is the only PAM the locker needs — `pam_start`, `pam_authenticate`,
//! `pam_acct_mgmt`, `pam_end`, and a conversation that answers the password
//! prompt — so it talks to `libpam` directly instead of through a wrapper
//! crate. The same code builds against Linux-PAM and OpenPAM (FreeBSD, NetBSD,
//! DragonFly, macOS): the functions and the `pam_message` / `pam_response` /
//! `pam_conv` structs have the same layout in both. The `const` on
//! `pam_message.msg` differs, but only in C, not in the ABI.
//!
//! Return codes are the trap. Only `PAM_SUCCESS` (0) and `PAM_BUF_ERR` (5) are
//! the same number in both; `PAM_CONV_ERR` is 19 on Linux-PAM and 6 on OpenPAM
//! (`_pam_types.h` vs `pam_constants.h`). So the conversation's failure code is
//! picked per platform below, nothing else compares against a non-zero code,
//! and every error message comes from `pam_strerror`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};

/// Opaque `pam_handle_t`.
#[repr(C)]
struct PamHandle {
    _private: [u8; 0],
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut PamHandle, errnum: c_int) -> *const c_char;
}

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
#[cfg(target_os = "linux")]
const PAM_CONV_ERR: c_int = 19;
#[cfg(any(
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "ios"
))]
const PAM_CONV_ERR: c_int = 6;
#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "macos",
    target_os = "ios"
)))]
compile_error!("unknown PAM implementation: look up PAM_CONV_ERR in this platform's headers");

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
/// Both implementations cap a single conversation call at this many messages.
const PAM_MAX_NUM_MSG: c_int = 32;

/// A password that is overwritten when dropped.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Take ownership of `password` without copying it.
    pub fn new(password: String) -> Self {
        Secret(password.into_bytes())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // The whole capacity: backspace pops characters but leaves their bytes
        // in the spare capacity.
        wipe(self.0.as_mut_ptr(), self.0.capacity());
    }
}

/// Overwrite `len` bytes at `ptr` in a way the optimizer can't drop. The
/// caller must own `len` writable bytes at `ptr`.
fn wipe(ptr: *mut u8, len: usize) {
    for i in 0..len {
        // SAFETY: guaranteed by the caller.
        unsafe { std::ptr::write_volatile(ptr.add(i), 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Why verification failed. Each carries PAM's own description.
#[derive(Debug)]
pub enum Error {
    /// The password holds a NUL byte, which PAM can't receive.
    NulInPassword,
    /// The service or user name holds a NUL byte.
    NulInName,
    /// `pam_start` failed (no such service, libpam trouble).
    Start(String),
    /// The password was wrong, or the conversation failed.
    Auth(String),
    /// The password was right but the account may not log in now.
    Account(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NulInPassword => f.write_str("password contains NUL"),
            Error::NulInName => f.write_str("service or user name contains NUL"),
            Error::Start(m) => write!(f, "pam_start: {m}"),
            Error::Auth(m) => write!(f, "pam_authenticate: {m}"),
            Error::Account(m) => write!(f, "pam_acct_mgmt: {m}"),
        }
    }
}

/// The conversation's view of the password, reached through `appdata_ptr`.
struct ConvData {
    password: Secret,
}

/// Owns a started PAM transaction and ends it on drop, with the status of the
/// last call as `pam_end` expects. `_conv` and `_data` back the pointers PAM
/// holds, so they live exactly as long as the handle.
struct Transaction {
    handle: *mut PamHandle,
    last_status: c_int,
    _conv: Box<PamConv>,
    _data: Box<ConvData>,
}

impl Transaction {
    fn strerror(&self, code: c_int) -> String {
        // SAFETY: `handle` is live; pam_strerror returns a static or
        // handle-owned string, copied out before the handle can end.
        let s = unsafe { pam_strerror(self.handle, code) };
        if s.is_null() {
            format!("PAM error {code}")
        } else {
            // SAFETY: non-null, NUL-terminated, owned by libpam.
            unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful pam_start and is ended once.
        unsafe { pam_end(self.handle, self.last_status) };
    }
}

/// Check `password` for `user` through PAM `service`: authenticate, then make
/// sure the account may log in. The password is wiped when this returns.
pub fn verify(service: &str, user: &str, password: Secret) -> Result<(), Error> {
    if password.0.contains(&0) {
        return Err(Error::NulInPassword);
    }
    let service = CString::new(service).map_err(|_| Error::NulInName)?;
    let user = CString::new(user).map_err(|_| Error::NulInName)?;

    let mut data = Box::new(ConvData { password });
    let conv = Box::new(PamConv {
        conv: converse,
        appdata_ptr: (&mut *data as *mut ConvData).cast(),
    });
    let mut handle: *mut PamHandle = std::ptr::null_mut();
    // SAFETY: valid C strings; `conv` (and the data it points to) outlive the
    // handle, as `Transaction` owns them.
    let rc = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &*conv, &mut handle) };
    if handle.is_null() {
        return Err(Error::Start(format!("PAM error {rc}")));
    }
    let mut tx = Transaction {
        handle,
        last_status: rc,
        _conv: conv,
        _data: data,
    };
    if rc != PAM_SUCCESS {
        return Err(Error::Start(tx.strerror(rc)));
    }

    // SAFETY: live handle.
    tx.last_status = unsafe { pam_authenticate(tx.handle, 0) };
    if tx.last_status != PAM_SUCCESS {
        return Err(Error::Auth(tx.strerror(tx.last_status)));
    }
    // SAFETY: live handle.
    tx.last_status = unsafe { pam_acct_mgmt(tx.handle, 0) };
    if tx.last_status != PAM_SUCCESS {
        return Err(Error::Account(tx.strerror(tx.last_status)));
    }
    Ok(())
}

/// The conversation callback. Answers every echo-off prompt (the password)
/// with the password, logs error messages, ignores informational text, and
/// refuses anything else (echo-on prompts such as a username or OTP).
///
/// PAM takes ownership of the response array and every `resp` string and frees
/// them with `free`, so both are allocated with libc. On any failure every
/// allocation made so far is wiped and freed and `*resp` is left untouched.
unsafe extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if num_msg <= 0 || num_msg > PAM_MAX_NUM_MSG || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    if appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: `appdata_ptr` is the `ConvData` that `verify` registered, alive
    // for the whole transaction.
    let data = unsafe { &*(appdata_ptr as *const ConvData) };
    let count = num_msg as usize;

    // SAFETY: calloc of `count` zeroed responses; null checked below.
    let replies =
        unsafe { libc::calloc(count, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }

    for i in 0..count {
        // Both Linux-PAM and OpenPAM pass an array of `count` message pointers.
        // SAFETY: `msg` has `count` entries.
        let m = unsafe { *msg.add(i) };
        if m.is_null() {
            // SAFETY: `replies` holds `count` entries, the first `i` filled.
            unsafe { free_replies(replies, count) };
            return PAM_CONV_ERR;
        }
        // SAFETY: non-null message from PAM.
        let m = unsafe { &*m };
        match m.msg_style {
            PAM_PROMPT_ECHO_OFF => {
                let Some(copy) = c_copy(&data.password.0) else {
                    // SAFETY: as above.
                    unsafe { free_replies(replies, count) };
                    return PAM_BUF_ERR;
                };
                // SAFETY: `i < count`.
                unsafe { (*replies.add(i)).resp = copy };
            }
            PAM_ERROR_MSG => {
                if !m.msg.is_null() {
                    // SAFETY: NUL-terminated message from PAM.
                    let text = unsafe { CStr::from_ptr(m.msg) };
                    tracing::debug!(msg = ?text, "pam error_msg");
                }
            }
            PAM_TEXT_INFO => {}
            PAM_PROMPT_ECHO_ON => {
                // A username or OTP prompt: the locker only has a password.
                // SAFETY: as above.
                unsafe { free_replies(replies, count) };
                return PAM_CONV_ERR;
            }
            _ => {
                // SAFETY: as above.
                unsafe { free_replies(replies, count) };
                return PAM_CONV_ERR;
            }
        }
    }

    // SAFETY: `resp` is a valid out-pointer from PAM.
    unsafe { *resp = replies };
    PAM_SUCCESS
}

/// A malloc'd, NUL-terminated copy of `bytes`, for PAM to free.
fn c_copy(bytes: &[u8]) -> Option<*mut c_char> {
    // SAFETY: malloc of len + 1 bytes; null checked.
    let p = unsafe { libc::malloc(bytes.len() + 1) } as *mut u8;
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` has room for `bytes` plus the terminator.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
    }
    Some(p.cast())
}

/// Wipe and free every response string in `replies`, then the array itself.
///
/// # Safety
/// `replies` is a calloc'd array of `count` responses whose `resp` fields are
/// null or strings from [`c_copy`].
unsafe fn free_replies(replies: *mut PamResponse, count: usize) {
    for i in 0..count {
        // SAFETY: `i < count`.
        let r = unsafe { &mut *replies.add(i) };
        if !r.resp.is_null() {
            // SAFETY: a NUL-terminated string from `c_copy`.
            let len = unsafe { libc::strlen(r.resp) };
            wipe(r.resp.cast(), len);
            // SAFETY: allocated by malloc in `c_copy`.
            unsafe { libc::free(r.resp.cast()) };
            r.resp = std::ptr::null_mut();
        }
    }
    // SAFETY: allocated by calloc in `converse`.
    unsafe { libc::free(replies.cast()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(style: c_int, text: &CStr) -> PamMessage {
        PamMessage {
            msg_style: style,
            msg: text.as_ptr(),
        }
    }

    /// Run `converse` over `messages` with `password` as the app data.
    fn run(messages: &[PamMessage], password: &str) -> (c_int, Vec<Option<Vec<u8>>>) {
        let data = ConvData {
            password: Secret::new(password.to_owned()),
        };
        let mut ptrs: Vec<*const PamMessage> = messages.iter().map(|m| m as *const _).collect();
        let mut out: *mut PamResponse = std::ptr::null_mut();
        let rc = unsafe {
            converse(
                ptrs.len() as c_int,
                ptrs.as_mut_ptr(),
                &mut out,
                (&data as *const ConvData).cast_mut().cast(),
            )
        };
        let mut replies = Vec::new();
        if rc == PAM_SUCCESS {
            for i in 0..messages.len() {
                let r = unsafe { &*out.add(i) };
                replies.push(
                    (!r.resp.is_null())
                        .then(|| unsafe { CStr::from_ptr(r.resp) }.to_bytes().to_vec()),
                );
            }
            unsafe { free_replies(out, messages.len()) };
        } else {
            assert!(
                out.is_null(),
                "a failed conversation must not hand back responses"
            );
        }
        (rc, replies)
    }

    #[test]
    fn answers_the_password_prompt() {
        let (rc, replies) = run(&[message(PAM_PROMPT_ECHO_OFF, c"Password: ")], "hunter2");
        assert_eq!(rc, PAM_SUCCESS);
        assert_eq!(replies, vec![Some(b"hunter2".to_vec())]);
    }

    #[test]
    fn info_and_error_messages_get_no_response() {
        let (rc, replies) = run(
            &[
                message(PAM_TEXT_INFO, c"hello"),
                message(PAM_ERROR_MSG, c"warning"),
                message(PAM_PROMPT_ECHO_OFF, c"Password: "),
            ],
            "pw",
        );
        assert_eq!(rc, PAM_SUCCESS);
        assert_eq!(replies, vec![None, None, Some(b"pw".to_vec())]);
    }

    #[test]
    fn echo_on_prompt_fails_the_whole_conversation() {
        let (rc, _) = run(
            &[
                message(PAM_PROMPT_ECHO_OFF, c"Password: "),
                message(PAM_PROMPT_ECHO_ON, c"OTP: "),
            ],
            "pw",
        );
        assert_eq!(rc, PAM_CONV_ERR);
    }

    #[test]
    fn unknown_style_and_bad_counts_are_refused() {
        assert_eq!(run(&[message(99, c"?")], "pw").0, PAM_CONV_ERR);
        let mut out: *mut PamResponse = std::ptr::null_mut();
        let data = ConvData {
            password: Secret::new(String::new()),
        };
        let appdata = (&data as *const ConvData).cast_mut().cast();
        for n in [0, -1, PAM_MAX_NUM_MSG + 1] {
            let mut none: [*const PamMessage; 0] = [];
            let rc = unsafe { converse(n, none.as_mut_ptr(), &mut out, appdata) };
            assert_eq!(rc, PAM_CONV_ERR, "num_msg = {n}");
        }
    }

    /// Through the real libpam and the installed `shoestring-lock` policy: a
    /// wrong password for `$USER` must fail authentication, after a full
    /// conversation with `pam_unix`. Needs the policy file installed, and
    /// `pam_unix` may add a fail delay, so it is `#[ignore]`.
    #[test]
    #[ignore]
    fn wrong_password_fails_through_real_pam() {
        let user = std::env::var("USER").expect("$USER");
        let err = verify(
            "shoestring-lock",
            &user,
            Secret::new("definitely-not-the-password-\u{1F512}".into()),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Auth(_)), "got {err}");
    }

    #[test]
    fn nul_in_password_is_rejected_before_pam() {
        let err = verify("shoestring-lock", "nobody", Secret::new("a\0b".into())).unwrap_err();
        assert!(matches!(err, Error::NulInPassword));
    }
}
