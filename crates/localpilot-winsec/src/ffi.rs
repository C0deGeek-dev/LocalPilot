//! The Win32 calls behind the safe API: the only unsafe code in the
//! workspace (ADR-0189). Every block states what makes it sound, and every
//! system allocation is freed by an owning type's `Drop`.

#![allow(
    unsafe_code,
    reason = "tokio's security-attributes API and Win32 security calls are unsafe-only (ADR-0189)"
)]

use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A UTF-16 string the system allocated with `LocalAlloc`.
struct LocalWide(*mut u16);

impl LocalWide {
    fn to_string_lossy(&self) -> String {
        if self.0.is_null() {
            return String::new();
        }
        // SAFETY: the system wrote a NUL-terminated UTF-16 string at `self.0`
        // and it stays allocated until `drop`; we read up to the NUL only.
        unsafe {
            let mut n = 0;
            while *self.0.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(self.0, n))
        }
    }
}

impl Drop for LocalWide {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a system LocalAlloc and is freed once.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
}

/// A self-relative security descriptor the system allocated.
pub(crate) struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `w` is NUL-terminated and outlives the call; on success
        // `sd` receives a LocalAlloc'd descriptor, owned from here by `Self`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the descriptor came from a system LocalAlloc and is
            // freed once.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
}

/// A kernel handle, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a real handle this type owns, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
    // closing; on success `token` is a real handle, owned by `Handle`.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut len = 0u32;
    // SAFETY: a null buffer of length 0 asks only for the size; the call
    // fails with ERROR_INSUFFICIENT_BUFFER and writes `len`.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements: TOKEN_USER holds pointers, so the buffer must be
    // pointer-aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is at least `len` bytes and 8-byte aligned.
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `buf` starts with a TOKEN_USER whose SID points into
    // `buf`, which lives until the end of this function.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut s: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid (above); on success `s` receives a LocalAlloc'd
    // string, owned from here by `LocalWide`.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalWide(s).to_string_lossy())
}

pub(crate) fn create_pipe(
    options: &ServerOptions,
    name: &str,
    sd: &SecurityDescriptor,
) -> io::Result<NamedPipeServer> {
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose descriptor `sd`
    // outlives the call; CreateNamedPipeW copies what it keeps.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            ptr::addr_of_mut!(attrs).cast::<std::ffi::c_void>(),
        )
    }
}

pub(crate) fn dacl_sddl(server: &NamedPipeServer) -> io::Result<String> {
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is the live pipe's and outlives the call; only the
    // descriptor out-parameter is requested, which the call LocalAllocs.
    let rc = unsafe {
        GetSecurityInfo(
            server.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(rc).unwrap_or(i32::MAX),
        ));
    }
    let sd = SecurityDescriptor(sd);
    let mut s: *mut u16 = ptr::null_mut();
    // SAFETY: `sd` is the descriptor just returned; on success `s` receives
    // a LocalAlloc'd string, owned from here by `LocalWide`.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut s,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalWide(s).to_string_lossy())
}
