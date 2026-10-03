//! Windows-only launch-file protection through the already-open file handle.
//!
//! The caller creates a NEW file with sharing disabled, then calls `restrict`
//! before writing credentials. That exclusive handle prevents another process
//! from retaining a read handle while an inherited ACL is being replaced.
use crate::{AppError, Result};
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    CONTAINER_INHERIT_ACE, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation, IsValidSid,
    NO_INHERITANCE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED, TOKEN_QUERY,
    TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Set current-user ownership and a protected DACL granting only that user and
/// SYSTEM access. The file must have READ_CONTROL, WRITE_DAC and WRITE_OWNER.
pub(crate) fn restrict(file: &File) -> Result<()> {
    let result = (|| -> io::Result<()> {
        check_regular_handle(file)?;
        let user = CurrentUser::load()?;
        let system = KnownSid::new(WinLocalSystemSid)?;
        set_private_acl(file, user.sid(), &[user.sid(), system.sid()], false)?;
        verify_inner(file, false)
    })();
    result.map_err(|_| {
        AppError::local(
            "Could not protect the launch-info file for this Windows user. No launch token was written.",
        )
    })
}

/// Check the SAME handle that will be read; do not inspect a named path and then
/// reopen it. The caller must use FILE_FLAG_OPEN_REPARSE_POINT for its read open.
pub(crate) fn verify(file: &File) -> Result<()> {
    verify_inner(file, false).map_err(|_| {
        AppError::invalid(
            "Launch information must be a regular file owned by this Windows user, with a protected ACL granting access only to this user and SYSTEM.",
        )
    })
}

/// Protect a newly created empty account directory. Callers must not use this
/// to reset an existing directory tree's access controls.
pub(crate) fn restrict_directory(directory: &File) -> Result<()> {
    (|| -> io::Result<()> {
        check_directory_handle(directory)?;
        let user = CurrentUser::load()?;
        let system = KnownSid::new(WinLocalSystemSid)?;
        set_private_acl(directory, user.sid(), &[user.sid(), system.sid()], true)?;
        verify_inner(directory, true)
    })()
    .map_err(|_| {
        AppError::local("Could not protect the new account directory for this Windows user.")
    })
}

pub(crate) fn verify_directory(directory: &File) -> Result<()> {
    verify_inner(directory, true).map_err(|_| AppError::invalid("The account directory must belong to this Windows user and have a protected inheritable ACL granting access only to this user and SYSTEM."))
}

fn check_directory_handle(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "account storage is not a regular directory",
        ));
    }
    Ok(())
}

fn check_regular_handle(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("launch information is not a regular file"));
    }
    Ok(())
}

struct CurrentUser {
    // usize provides sufficient alignment for TOKEN_USER and its embedded SID.
    words: Vec<usize>,
    used_bytes: usize,
}

impl CurrentUser {
    fn load() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a valid pseudo handle; output points to
        // live storage. TOKEN_QUERY grants no mutation rights.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful OpenProcessToken returns a new owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut required = 0u32;
        // SAFETY: null output with zero size is the documented size-query form.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &mut required,
            );
        }
        if (required as usize) < size_of::<TOKEN_USER>() || required > 64 * 1024 {
            return Err(io::Error::other("invalid token information size"));
        }
        let mut words = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
        // SAFETY: the aligned allocation contains at least `required` bytes and
        // both it and the length output remain alive for the synchronous call.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                words.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if (required as usize) < size_of::<TOKEN_USER>()
            || required as usize > std::mem::size_of_val(words.as_slice())
        {
            return Err(io::Error::other("invalid returned token information size"));
        }
        let result = Self {
            words,
            used_bytes: required as usize,
        };
        // The returned SID must be inside the owned token-information buffer.
        let sid = result.sid();
        validate_sid_region(sid, result.words.as_ptr() as usize, result.used_bytes)?;
        Ok(result)
    }

    fn sid(&self) -> PSID {
        // SAFETY: load allocates an aligned, sufficiently sized TOKEN_USER and
        // fills it with GetTokenInformation before this accessor is used.
        unsafe { (*self.words.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

struct KnownSid {
    // SECURITY_MAX_SID_SIZE is 68 bytes; u32 also provides SID alignment.
    words: [u32; 17],
}

impl KnownSid {
    fn new(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Self> {
        let mut result = Self { words: [0; 17] };
        let mut length = size_of::<[u32; 17]>() as u32;
        // SAFETY: buffer is aligned and has the capacity supplied in length;
        // these well-known SIDs do not require a domain SID.
        if unsafe {
            CreateWellKnownSid(
                kind,
                ptr::null_mut(),
                result.words.as_mut_ptr().cast(),
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }

    fn sid(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: these allocations come only from Win32 security APIs that
        // explicitly require LocalFree. Each allocation is owned exactly once.
        unsafe { LocalFree(self.0) };
    }
}

fn set_private_acl(
    file: &File,
    owner: PSID,
    allowed_sids: &[PSID],
    directory: bool,
) -> io::Result<()> {
    let entries: Vec<_> = allowed_sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if directory {
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
            } else {
                NO_INHERITANCE
            },
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: (*sid).cast(),
                ..Default::default()
            },
        })
        .collect();
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: all SID buffers and the explicit-entry array remain live, and the
    // null old ACL builds a fresh ACL without retaining inherited broad grants.
    let status = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            ptr::null(),
            &mut acl,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if acl.is_null() {
        return Err(io::Error::other("security API returned no ACL"));
    }
    let acl_owner = LocalAllocation(acl.cast());
    // SAFETY: File owns a live handle opened with the required security access;
    // owner and ACL buffers remain live throughout this synchronous call.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            ptr::null_mut(),
            acl_owner.0.cast(),
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

fn verify_inner(file: &File, directory: bool) -> io::Result<()> {
    if directory {
        check_directory_handle(file)?;
    } else {
        check_regular_handle(file)?;
    }
    let user = CurrentUser::load()?;
    let system = KnownSid::new(WinLocalSystemSid)?;
    let mut owner = ptr::null_mut();
    let mut acl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: all output pointers address live local storage. The descriptor
    // returned on success owns the owner SID and DACL views until LocalFree.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::other("security API returned no descriptor"));
    }
    let descriptor_owner = LocalAllocation(descriptor);
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: this is a valid descriptor returned by GetSecurityInfo, still
    // owned by descriptor_owner; the other arguments are writable scalars.
    if unsafe { GetSecurityDescriptorControl(descriptor_owner.0, &mut control, &mut revision) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null()
        || acl.is_null()
        || control & (SE_DACL_PRESENT | SE_DACL_PROTECTED) != (SE_DACL_PRESENT | SE_DACL_PROTECTED)
    {
        return Err(io::Error::other("missing protected owner ACL"));
    }
    // SAFETY: owner is a valid SID in the API-owned security descriptor; user
    // was validated inside its owned token-information buffer.
    if unsafe { IsValidSid(owner) == 0 || EqualSid(owner, user.sid()) == 0 } {
        return Err(io::Error::other(
            "launch information belongs to another owner",
        ));
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: ACL belongs to the valid descriptor; info points to a correctly
    // sized output structure and remains alive during the call.
    if unsafe {
        GetAclInformation(
            acl,
            ptr::from_mut(&mut info).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.AceCount == 0 || info.AceCount > 16 || info.AclBytesInUse < size_of::<ACL>() as u32 {
        return Err(io::Error::other("unexpected launch-file ACL shape"));
    }
    let mut user_grant = false;
    for index in 0..info.AceCount {
        let mut ace = ptr::null_mut();
        // SAFETY: index is below the count reported for this valid ACL.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let offset = (ace as usize)
            .checked_sub(acl as usize)
            .ok_or_else(|| io::Error::other("invalid ACL entry address"))?;
        if offset
            .checked_add(size_of::<ACE_HEADER>())
            .is_none_or(|end| end > info.AclBytesInUse as usize)
        {
            return Err(io::Error::other("invalid ACL entry header"));
        }
        // SAFETY: the header lies within the bounds reported for this ACL.
        let header = unsafe { ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
        let size = header.AceSize as usize;
        // ACCESS_ALLOWED_ACE_TYPE is 0. Reject callback/object/inherited/deny
        // variants rather than interpreting a different ACE layout as an allow.
        if header.AceType != 0
            || header.AceFlags
                != if directory {
                    (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
                } else {
                    0
                }
            || size < offset_of!(ACCESS_ALLOWED_ACE, SidStart) + 8
            || offset
                .checked_add(size)
                .is_none_or(|end| end > info.AclBytesInUse as usize)
        {
            return Err(io::Error::other("unsupported launch-file ACL entry"));
        }
        // SAFETY: the SID offset and its minimum header fit in this allow ACE.
        let sid = unsafe {
            ace.cast::<u8>()
                .add(offset_of!(ACCESS_ALLOWED_ACE, SidStart))
        }
        .cast();
        validate_sid_region(sid, ace as usize, size)?;
        // SAFETY: all three SIDs are validated and held in live allocations.
        let is_user = unsafe { EqualSid(sid, user.sid()) != 0 };
        let is_system = unsafe { EqualSid(sid, system.sid()) != 0 };
        if !is_user && !is_system {
            return Err(io::Error::other(
                "launch-file ACL grants another identity access",
            ));
        }
        user_grant |= is_user;
    }
    if !user_grant {
        return Err(io::Error::other(
            "launch-file ACL has no current-user grant",
        ));
    }
    Ok(())
}

fn validate_sid_region(sid: PSID, base: usize, length: usize) -> io::Result<()> {
    let offset = (sid as usize)
        .checked_sub(base)
        .ok_or_else(|| io::Error::other("invalid SID address"))?;
    if offset.checked_add(8).is_none_or(|end| end > length) {
        return Err(io::Error::other("SID header is outside its allocation"));
    }
    // SAFETY: the SID's fixed eight-byte header is inside the checked region.
    let subauthorities = unsafe { *sid.cast::<u8>().add(1) } as usize;
    if offset
        .checked_add(8 + subauthorities * 4)
        .is_none_or(|end| end > length)
    {
        return Err(io::Error::other("SID is outside its allocation"));
    }
    // SAFETY: the full SID length described by its header fits in this live region.
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::other("invalid SID"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Security::{UNPROTECTED_DACL_SECURITY_INFORMATION, WinWorldSid};
    use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC, WRITE_OWNER};

    fn new_exclusive(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[test]
    fn private_acl_is_verified_before_nonsecret_data_is_written_and_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private-fixture.txt");
        let mut file = new_exclusive(&path);
        // No other read handle can be acquired while the inherited ACL is replaced.
        assert!(File::open(&path).is_err());
        restrict(&file).unwrap();
        verify(&file).unwrap();
        file.write_all(b"nonsecret fixture").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut reopened = File::open(&path).unwrap();
        verify(&reopened).unwrap();
        let mut text = String::new();
        reopened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "nonsecret fixture");
    }

    #[test]
    fn verifier_rejects_an_additional_everyone_grant() {
        let directory = tempfile::tempdir().unwrap();
        let file = new_exclusive(&directory.path().join("broad-fixture.txt"));
        restrict(&file).unwrap();
        let user = CurrentUser::load().unwrap();
        let system = KnownSid::new(WinLocalSystemSid).unwrap();
        let everyone = KnownSid::new(WinWorldSid).unwrap();
        set_private_acl(
            &file,
            user.sid(),
            &[user.sid(), system.sid(), everyone.sid()],
            false,
        )
        .unwrap();
        assert!(verify(&file).is_err());
    }

    #[test]
    fn verifier_rejects_unprotected_acl_inheritance() {
        let directory = tempfile::tempdir().unwrap();
        let file = new_exclusive(&directory.path().join("inherited-fixture.txt"));
        restrict(&file).unwrap();
        let mut acl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: File owns a live security-readable handle, with correctly
        // typed outputs whose allocation is released with LocalFree below.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        let _descriptor_owner = LocalAllocation(descriptor);
        // SAFETY: File has WRITE_DAC; the retrieved DACL remains live. This
        // fixture never contains a credential or other secret.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        assert!(verify(&file).is_err());
    }
}
