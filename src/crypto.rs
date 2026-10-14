use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use sha2::{Digest, Sha256};
use thiserror::Error;
use webrtc_dtls::crypto::Certificate;

pub(crate) mod peers;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

pub fn certificate_fingerprint(cert: &Certificate) -> String {
    let certificate = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(certificate)
}

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        match ensure_private(path) {
            Ok(true) => log::warn!("{path:?} was accessible to other users, restricted it"),
            Ok(false) => {}
            Err(e) => log::warn!("{path:?}: could not check permissions: {e}"),
        }
        Ok(load_certificate(path)?)
    } else {
        generate_key_and_cert(path)
    }
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    let f = create_private(path)?;
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    writer.flush()?;
    Ok(cert)
}

/// create (or truncate) `path` readable and writable by the current user only
#[cfg(unix)]
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    let f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // mode only applies to newly created files
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

/// restrict `path` to the current user; true if it had to be repaired
fn ensure_private(path: &Path) -> io::Result<bool> {
    if key_file_is_private(path)? {
        return Ok(false);
    }
    restrict(path)?;
    Ok(true)
}

/// whether nobody but the current user can access `path`
#[cfg(unix)]
pub(crate) fn key_file_is_private(path: &Path) -> io::Result<bool> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o077 == 0)
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
pub(crate) use windows_acl::create_private;
#[cfg(windows)]
pub(crate) use windows_acl::key_file_is_private;
#[cfg(windows)]
use windows_acl::restrict;

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    File::create(path)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn key_file_is_private(_path: &Path) -> io::Result<bool> {
    Ok(true)
}

#[cfg(not(any(unix, windows)))]
fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// whether the DACL in `sddl` (as returned for DACL_SECURITY_INFORMATION) is
/// protected from inheritance and allows access to nobody but `sid`
#[cfg_attr(not(windows), allow(dead_code))]
fn dacl_is_private(sddl: &str, sid: &str) -> bool {
    let Some(dacl) = sddl.strip_prefix("D:") else {
        // no DACL: everyone has full access
        return false;
    };
    let (flags, mut aces) = dacl.split_at(dacl.find('(').unwrap_or(dacl.len()));
    if !flags.contains('P') || flags.contains("NO_ACCESS_CONTROL") {
        return false;
    }
    while let Some(rest) = aces.strip_prefix('(') {
        let Some(end) = rest.find(')') else {
            return false;
        };
        let fields = rest[..end].split(';').collect::<Vec<_>>();
        let [ace_type, .., trustee] = fields.as_slice() else {
            return false;
        };
        let allows = matches!(*ace_type, "A" | "OA" | "XA" | "ZA");
        if allows && !trustee.eq_ignore_ascii_case(sid) {
            return false;
        }
        aces = &rest[end + 1..];
    }
    aces.is_empty()
}

#[cfg(windows)]
mod windows_acl {
    use std::{fs::File, io, os::windows::io::FromRawHandle, path::Path, ptr};

    use windows::{
        Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, HLOCAL, LocalFree},
            Security::{
                ACL,
                Authorization::{
                    ConvertSecurityDescriptorToStringSecurityDescriptorW,
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
                    SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
                },
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, OBJECT_SECURITY_INFORMATION,
                PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            },
            Storage::FileSystem::{
                CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE,
            },
        },
        core::{BOOL, HSTRING, PWSTR},
    };

    use crate::ipc::current_user_sid;

    /// security descriptor allocated by the system, freed on drop
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    /// protected DACL granting full access to `sid` only
    fn private_descriptor(sid: &str) -> io::Result<Descriptor> {
        let sddl = HSTRING::from(format!("D:P(A;;FA;;;{sid})"));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?
        };
        Ok(Descriptor(sd))
    }

    pub(crate) fn create_private(path: &Path) -> io::Result<File> {
        let sd = private_descriptor(&current_user_sid()?)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(path),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                Some(&sa),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )?
        };
        // Safety: freshly opened handle, owned by the File from here on
        let f = unsafe { File::from_raw_handle(handle.0) };
        // an existing file keeps its DACL with CREATE_ALWAYS
        if !key_file_is_private(path)? {
            restrict(path)?;
        }
        Ok(f)
    }

    pub(crate) fn key_file_is_private(path: &Path) -> io::Result<bool> {
        let name = HSTRING::from(path);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let err = unsafe {
            GetNamedSecurityInfoW(
                &name,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                &mut sd,
            )
        };
        if err.is_err() {
            return Err(io::Error::from_raw_os_error(err.0 as i32));
        }
        let sd = Descriptor(sd);
        let mut s = PWSTR(ptr::null_mut());
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                None,
            )?
        };
        let sddl = unsafe { s.to_string() }.map_err(io::Error::other);
        unsafe { LocalFree(Some(HLOCAL(s.0 as *mut _))) };
        Ok(super::dacl_is_private(&sddl?, &current_user_sid()?))
    }

    /// replace the DACL of `path` with one granting the current user only
    pub(super) fn restrict(path: &Path) -> io::Result<()> {
        let name = HSTRING::from(path);
        let private = private_descriptor(&current_user_sid()?)?;
        let mut present = BOOL::default();
        let mut defaulted = BOOL::default();
        let mut acl: *mut ACL = ptr::null_mut();
        unsafe { GetSecurityDescriptorDacl(private.0, &mut present, &mut acl, &mut defaulted)? };
        let err = unsafe {
            SetNamedSecurityInfoW(
                &name,
                SE_FILE_OBJECT,
                OBJECT_SECURITY_INFORMATION(
                    DACL_SECURITY_INFORMATION.0 | PROTECTED_DACL_SECURITY_INFORMATION.0,
                ),
                None,
                None,
                Some(acl),
                None,
            )
        };
        if err.is_err() {
            return Err(io::Error::from_raw_os_error(err.0 as i32));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn private_dacl_accepted() {
        assert!(dacl_is_private(&format!("D:P(A;;FA;;;{SID})"), SID));
        assert!(dacl_is_private(&format!("D:PAI(A;;0x1f01ff;;;{SID})"), SID));
        // deny entries for others don't widen access
        assert!(dacl_is_private(
            &format!("D:P(D;;FA;;;WD)(A;;FA;;;{SID})"),
            SID
        ));
    }

    #[test]
    fn shared_dacl_rejected() {
        // inherited defaults: SYSTEM, Administrators, user
        assert!(!dacl_is_private(
            &format!("D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FA;;;{SID})"),
            SID
        ));
        // protected but grants someone else
        assert!(!dacl_is_private(
            &format!("D:P(A;;FA;;;{SID})(A;;FR;;;BU)"),
            SID
        ));
        // not protected: inheritable entries may be added later
        assert!(!dacl_is_private(&format!("D:(A;;FA;;;{SID})"), SID));
        // null DACL / no DACL
        assert!(!dacl_is_private("D:NO_ACCESS_CONTROL", SID));
        assert!(!dacl_is_private("", SID));
        // malformed
        assert!(!dacl_is_private(&format!("D:P(A;;FA;;;{SID}"), SID));
    }

    #[cfg(unix)]
    #[test]
    fn unix_key_is_private() {
        let dir = std::env::temp_dir().join(format!("mimouse-crypto-{}", std::process::id()));
        let path = dir.join("mimouse.pem");
        generate_key_and_cert(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_generate_key_and_cert(&path).is_ok());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!ensure_private(&path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
