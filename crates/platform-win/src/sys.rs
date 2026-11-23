//! Process and file-permission primitives the CLI and desktop shell need on
//! Windows but `std` does not expose.
//!
//! They live here, next to the other Win32 calls, so the Windows code is
//! compiled by `cargo check --target x86_64-pc-windows-msvc` instead of hiding
//! behind the application crate's feature graph.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_SUCCESS, STILL_ACTIVE};
use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION,
    GetLengthSid, GetTokenInformation, InitializeAcl, OBJECT_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::{HRESULT, PCWSTR};

/// `FILE_ALL_ACCESS`: the full set of file access rights, used for the owner's
/// ACE. The windows crate exposes it only with the file-system feature, and one
/// constant does not justify a feature that no other call needs.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// Whether a process id names a live process.
///
/// A process owned by another user cannot be opened with the query right; that
/// is "alive but not ours", which is exactly the answer the single-instance
/// lock needs. Only a missing process reports false.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess takes plain values and returns an owned handle or an
    // error; no pointer is dereferenced here.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        // Access denied means the process exists but belongs to another user;
        // any other error means the pid is gone.
        Err(error) => return error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
    };
    let mut exit_code = 0u32;
    // SAFETY: the handle is live and `exit_code` is a writable local.
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: the handle came from OpenProcess and is closed exactly once.
    unsafe {
        let _ = CloseHandle(handle);
    }
    queried.is_ok() && exit_code == STILL_ACTIVE.0 as u32
}

/// Replace the file's inherited DACL with one ACE for the current user.
///
/// This is the Windows counterpart of `chmod 0600`: only the account that runs
/// printkit can read the administrator token. The DACL is protected, so a
/// permissive parent directory cannot widen it again.
pub fn restrict_to_current_user(path: &Path) -> io::Result<()> {
    let token = process_token()?;
    let mut needed = 0u32;
    // SAFETY: a null buffer with length 0 asks for the required size; the call
    // is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::other(
            "cannot size the process token user information",
        ));
    }
    let mut information = vec![0u8; needed as usize];
    // SAFETY: the buffer is `needed` bytes, the size the previous call asked
    // for, and the token is live.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(information.as_mut_ptr() as *mut core::ffi::c_void),
            needed,
            &mut needed,
        )
    }
    .map_err(io_error)?;
    if information.len() < std::mem::size_of::<TOKEN_USER>() {
        return Err(io::Error::other("process token user information is short"));
    }
    // SAFETY: `read_unaligned` accepts a byte buffer; the SID it points to is
    // owned by the same buffer and stays valid until this function returns.
    let user = unsafe { (information.as_ptr() as *const TOKEN_USER).read_unaligned() };
    if user.User.Sid.is_invalid() {
        return Err(io::Error::other("process token has no user SID"));
    }
    // SAFETY: the SID belongs to the live token information buffer.
    let sid_length = unsafe { GetLengthSid(user.User.Sid) } as usize;
    let acl_length = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        - std::mem::size_of::<u32>()
        + sid_length;
    // An ACL stores 16-bit sizes and ACEs are DWORD-aligned; `u64` storage
    // gives the buffer the alignment `ACL` expects.
    let mut storage = vec![0u64; acl_length.div_ceil(8)];
    let acl = storage.as_mut_ptr() as *mut ACL;
    // SAFETY: `acl` points at `storage`, which is at least `acl_length` bytes.
    unsafe { InitializeAcl(acl, acl_length as u32, ACL_REVISION) }.map_err(io_error)?;
    // SAFETY: the ACL has room for one allow ACE plus the user SID (sized
    // above), and both pointers are live for the duration of the call.
    unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, user.User.Sid) }
        .map_err(io_error)?;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let security_information = OBJECT_SECURITY_INFORMATION(
        DACL_SECURITY_INFORMATION.0 | PROTECTED_DACL_SECURITY_INFORMATION.0,
    );
    // SAFETY: the path is null-terminated, the ACL and SID outlive the call,
    // and the owner/group/SACL arguments are deliberately left unchanged.
    let status = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            security_information,
            None,
            None,
            Some(acl as *const ACL),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(())
}

/// A process token; the handle is closed on every exit path.
struct Token(windows::Win32::Foundation::HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_token() -> io::Result<Token> {
    let mut handle = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: the pseudo-handle is always valid and `handle` is writable.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) }.map_err(io_error)?;
    Ok(Token(handle))
}

fn io_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}
