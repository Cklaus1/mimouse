//! Control pipe security on Windows: `\\.\pipe\mimouse-<user SID>`, owned by
//! and accessible to the current user only, local clients only. Clients
//! refuse a pipe owned by anyone else (name squatting).

use std::{ffi::c_void, io, os::windows::io::AsRawHandle, ptr};

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
            },
            GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
            SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::GetNamedPipeClientProcessId,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
    },
    core::{HSTRING, PWSTR},
};

use super::{ConnectionError, Peer};

/// name of the control pipe of the current user
pub(super) fn pipe_name() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\mimouse-{}", current_user_sid()?))
}

/// security descriptor for control pipe instances: owner and only grantee is
/// the current user; protected, so nothing is inherited. The medium label lets
/// a non-elevated CLI reach a service started elevated (scheduled task).
pub(super) struct PipeSecurity {
    name: String,
    sd: PSECURITY_DESCRIPTOR,
}

impl PipeSecurity {
    pub(super) fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl = HSTRING::from(format!("O:{sid}G:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?
        };
        Ok(Self {
            name: format!(r"\\.\pipe\mimouse-{sid}"),
            sd,
        })
    }

    /// create a pipe instance; `first` fails with access denied if the pipe
    /// already exists
    pub(super) fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: false.into(),
        };
        // Safety: `sa` and the descriptor it points to outlive the call
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &self.name,
                    &mut sa as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        }
    }

    /// the process connected to `pipe`
    pub(super) fn client_peer(&self, pipe: &NamedPipeServer) -> Peer {
        let mut pid = 0;
        let pipe = HANDLE(pipe.as_raw_handle());
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.is_err() {
            return Peer::default();
        }
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return Peer {
                pid: Some(pid),
                ..Default::default()
            };
        };
        let peer = Peer {
            pid: Some(pid),
            uid: process_user_sid(process).ok(),
            exe: process_image(process).ok(),
        };
        let _ = unsafe { CloseHandle(process) };
        peer
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.sd.0))) };
    }
}

/// refuse a pipe whose owner is not the current user
pub(super) fn verify_owner(pipe: &NamedPipeClient) -> Result<(), ConnectionError> {
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let err = unsafe {
        GetSecurityInfo(
            HANDLE(pipe.as_raw_handle()),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    if err.is_err() {
        return Err(io::Error::from_raw_os_error(err.0 as i32).into());
    }
    let owner = sid_to_string(owner);
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    let owner = owner?;
    if owner != current_user_sid()? {
        return Err(ConnectionError::NotOwner(owner));
    }
    Ok(())
}

/// string SID of the user running this process
pub(crate) fn current_user_sid() -> io::Result<String> {
    process_user_sid(unsafe { GetCurrentProcess() })
}

fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token)? };
    let sid = token_user_sid(token);
    let _ = unsafe { CloseHandle(token) };
    sid
}

fn token_user_sid(token: HANDLE) -> io::Result<String> {
    let mut len = 0;
    // first call reports the required size
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    // u64 storage keeps TOKEN_USER aligned
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        )?
    };
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    sid_to_string(user.User.Sid)
}

fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut s = PWSTR(ptr::null_mut());
    unsafe { ConvertSidToStringSidW(sid, &mut s)? };
    let r = unsafe { s.to_string() }.map_err(io::Error::other);
    unsafe { LocalFree(Some(HLOCAL(s.0 as *mut c_void))) };
    r
}

fn process_image(process: HANDLE) -> io::Result<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )?
    };
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}
