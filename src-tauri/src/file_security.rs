use std::{fs::File, io, mem::size_of, os::windows::io::AsRawHandle, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        AclSizeInformation,
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SetSecurityInfo,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        EqualSid, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
};

pub(super) struct Dacl {
    descriptor: PSECURITY_DESCRIPTOR,
    acl: *mut ACL,
    label: *mut ACL,
    owner: PSID,
    protected: bool,
}

impl Dacl {
    pub(super) fn deny_access(file: &File) -> io::Result<()> {
        let mut denied = Self {
            descriptor: null_mut(),
            acl: null_mut(),
            label: null_mut(),
            owner: null_mut(),
            protected: true,
        };
        // OWNER RIGHTS also removes the owner's implicit right to reopen
        // the file for WRITE_DAC. Keep our existing control handle live.
        let sddl: Vec<u16> = "D:P(D;;FA;;;OW)\0".encode_utf16().collect();
        // SAFETY: sddl is terminated and the descriptor output is writable.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(), SDDL_REVISION_1, &mut denied.descriptor, null_mut(),
            )
        } == 0 {
            return Err(io::Error::last_os_error());
        }
        let (mut present, mut defaulted) = (0, 0);
        // SAFETY: denied owns the descriptor and the outputs are writable.
        if unsafe {
            GetSecurityDescriptorDacl(
                denied.descriptor, &mut present, &mut denied.acl, &mut defaulted,
            )
        } == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || denied.acl.is_null() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the live handle has WRITE_DAC and denied retains its ACL.
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(), SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(), null_mut(), denied.acl, null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        Ok(())
    }

    pub(super) fn read(file: &File) -> io::Result<Self> {
        let (mut descriptor, mut acl, mut label, mut owner) =
            (null_mut(), null_mut(), null_mut(), null_mut());
        // SAFETY: file has READ_CONTROL and the outputs are writable. The
        // returned ACL and owner are held by the returned descriptor.
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                &mut label,
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let mut data = Self {
            descriptor,
            acl,
            label,
            owner,
            protected: false,
        };
        if data.descriptor.is_null() || data.owner.is_null() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let (mut control, mut revision) = (0, 0);
        // SAFETY: data owns the descriptor and the scalar outputs are writable.
        if unsafe { GetSecurityDescriptorControl(data.descriptor, &mut control, &mut revision) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        data.protected = control & SE_DACL_PROTECTED != 0;
        Ok(data)
    }

    pub(super) fn identity_matches(&self, file: &File) -> io::Result<bool> {
        let actual = Self::read(file)?;
        Ok(self.same_owner(&actual) && acl_bytes(self.label)? == acl_bytes(actual.label)?)
    }

    fn same_owner(&self, other: &Self) -> bool {
        // SAFETY: each non-null SID belongs to a live security descriptor.
        self.owner.is_null()
            || (!other.owner.is_null() && unsafe { EqualSid(self.owner, other.owner) } != 0)
    }

    pub(super) fn apply_identity(&self, file: &File) -> io::Result<()> {
        let actual = Self::read(file)?;
        let mut flags = 0;
        if !self.same_owner(&actual) {
            flags |= OWNER_SECURITY_INFORMATION;
        }
        if acl_bytes(self.label)? != acl_bytes(actual.label)? {
            flags |= LABEL_SECURITY_INFORMATION;
        }
        if flags == 0 {
            return Ok(());
        }
        // SAFETY: the handle has WRITE_OWNER and the SID and integrity-label
        // ACL both belong to this live security descriptor.
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                flags,
                if flags & OWNER_SECURITY_INFORMATION != 0 {
                    self.owner
                } else {
                    null_mut()
                },
                null_mut(),
                null_mut(),
                self.label,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result as i32))
        }
    }

    pub(super) fn apply(&self, file: &File) -> io::Result<()> {
        let inheritance = if self.protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        // SAFETY: file has WRITE_DAC, and the ACL pointer refers to our
        // descriptor or is null when the original itself has a null DACL.
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | inheritance,
                null_mut(),
                null_mut(),
                self.acl,
                null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        self.verify(file)
    }

    pub(super) fn verify(&self, file: &File) -> io::Result<()> {
        if !self.matches(file)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "保存先のアクセス権を維持できませんでした。",
            ));
        }
        Ok(())
    }

    pub(super) fn matches(&self, file: &File) -> io::Result<bool> {
        let actual = Self::read(file)?;
        Ok(self.same_owner(&actual)
            && actual.protected == self.protected
            && acl_bytes(actual.acl)? == acl_bytes(self.acl)?
            && acl_bytes(actual.label)? == acl_bytes(self.label)?)
    }
}

fn acl_bytes(acl: *mut ACL) -> io::Result<Option<Vec<u8>>> {
    if acl.is_null() {
        return Ok(None);
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: the ACL belongs to the live descriptor and info is writable.
    if unsafe {
        GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: Windows reported this byte count for the initialized part of
    // the ACL. The returned slice cannot outlive the descriptor.
    Ok(Some(
        unsafe { std::slice::from_raw_parts(acl.cast(), info.AclBytesInUse as usize) }.to_vec(),
    ))
}

impl Drop for Dacl {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: GetSecurityInfo allocated the descriptor with LocalAlloc.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::{save_path, write_atomic};
    use crate::windows_fs::{
        capture_destination, open_directory, opened_path, rename_at, StagedFile, SHARE_ALL,
    };
    use std::ffi::OsStr;
    use std::{fs, io::Write, os::windows::fs::OpenOptionsExt};
    use windows_sys::Win32::Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        GetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};

    fn owner_only_dacl() -> Dacl {
        let mut dacl = Dacl {
            descriptor: null_mut(),
            acl: null_mut(),
            label: null_mut(),
            owner: null_mut(),
            protected: true,
        };
        // The temporary file grants full access only to its owner.
        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
        // SAFETY: the input is null-terminated and the output pointer is writable.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut dacl.descriptor,
                    null_mut(),
                )
            },
            0
        );
        let (mut present, mut defaulted) = (0, 0);
        // SAFETY: dacl owns the descriptor and the outputs are writable.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(
                    dacl.descriptor,
                    &mut present,
                    &mut dacl.acl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        dacl
    }

    #[test]
    fn a_protected_dacl_is_present_before_writing_and_after_publishing() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("document.md");
        fs::write(&path, b"original").unwrap();
        let original = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .open(&path)
            .unwrap();
        owner_only_dacl().apply(&original).unwrap();
        let dacl = Dacl::read(&original).unwrap();
        drop(original);

        let selected = save_path(&path).unwrap();
        write_atomic(&selected, &work.path().join("recovery"), |writer| {
            dacl.verify(writer.get_ref())?;
            writer.write_all(b"updated")
        })
        .unwrap();
        let result = File::open(&path).unwrap();
        dacl.verify(&result).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"updated");
    }

    #[test]
    fn comparison_after_moving_the_original_rejects_updated_access_rules() {
        let work = tempfile::tempdir().unwrap();
        let directory = open_directory(work.path()).unwrap();
        let path = opened_path(&directory).unwrap();
        let name = OsStr::new("document.md");
        let backup = OsStr::new("original.md");
        fs::write(path.join(name), b"original").unwrap();
        let expected = capture_destination(&directory, &path, name)
            .unwrap()
            .unwrap();
        let (staged, original) =
            StagedFile::begin_document(&directory, &path, name, Some(&expected)).unwrap();
        let original = original.unwrap();
        rename_at(&original.file, None, backup).unwrap();
        original.verify_saved_metadata(&staged.file).unwrap();
        let access = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(SHARE_ALL)
            .open(path.join(backup))
            .unwrap();
        owner_only_dacl().apply(&access).unwrap();
        drop(access);
        assert!(original.verify_saved_metadata(&staged.file).is_err());
        rename_at(&original.file, None, name).unwrap();
        drop(original);
        assert_eq!(fs::read(path.join(name)).unwrap(), b"original");
    }

    #[test]
    fn published_document_is_rolled_back_if_the_original_access_rules_changed() {
        let work = tempfile::tempdir().unwrap();
        let directory = open_directory(work.path()).unwrap();
        let path = opened_path(&directory).unwrap();
        let name = OsStr::new("document.md");
        let backup = OsStr::new("backup.md");
        fs::write(path.join(name), b"original").unwrap();
        let expected = capture_destination(&directory, &path, name)
            .unwrap()
            .unwrap();
        let (mut staged, original) =
            StagedFile::begin_document(&directory, &path, name, Some(&expected)).unwrap();
        let original = original.unwrap();
        staged.file.write_all(b"new").unwrap();
        rename_at(&original.file, None, backup).unwrap();
        rename_at(&staged.file, None, name).unwrap();
        staged.persisted = true;
        let access = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(SHARE_ALL)
            .open(path.join(backup))
            .unwrap();
        owner_only_dacl().apply(&access).unwrap();
        drop(access);
        assert!(staged.verify_published(&directory, name, &original).is_err());
        assert!(!staged.persisted);
        drop(original);
        drop(staged);
        assert_eq!(fs::read(path.join(name)).unwrap(), b"original");
        assert!(!path.join(backup).exists());
    }

    #[test]
    fn a_failed_rollback_keeps_the_new_file_under_the_latest_permissions() {
        let work = tempfile::tempdir().unwrap();
        let directory = open_directory(work.path()).unwrap();
        let path = opened_path(&directory).unwrap();
        let name = OsStr::new("document.md");
        let backup = OsStr::new("backup.md");
        fs::write(path.join(name), b"original").unwrap();
        let expected = capture_destination(&directory, &path, name)
            .unwrap()
            .unwrap();
        let (mut staged, original) =
            StagedFile::begin_document(&directory, &path, name, Some(&expected)).unwrap();
        let original = original.unwrap();
        staged.file.write_all(b"new contents").unwrap();
        rename_at(&original.file, None, backup).unwrap();
        rename_at(&staged.file, None, name).unwrap();
        staged.persisted = true;

        let access = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(SHARE_ALL)
            .open(path.join(backup))
            .unwrap();
        owner_only_dacl().apply(&access).unwrap();
        let updated = Dacl::read(&access).unwrap();
        drop(access);
        let occupied = path.join(&staged.name);
        fs::write(&occupied, b"another file").unwrap();

        let error = staged
            .verify_published(&directory, name, &original)
            .unwrap_err();
        assert!(error.to_string().contains("最新の権限"), "{error}");
        assert!(staged.persisted);
        drop(staged);
        updated.verify(&File::open(path.join(name)).unwrap()).unwrap();
        assert_eq!(fs::read(path.join(name)).unwrap(), b"new contents");
        assert_eq!(fs::read(&occupied).unwrap(), b"another file");
        drop(original);
        assert_eq!(fs::read(path.join(backup)).unwrap(), b"original");
    }

    #[test]
    fn emergency_protection_blocks_reopening_the_retained_file() {
        let work = tempfile::tempdir().unwrap();
        let directory = open_directory(work.path()).unwrap();
        let path = opened_path(&directory).unwrap();
        let mut staged =
            StagedFile::create_with_access(&directory, &path, READ_CONTROL | WRITE_DAC).unwrap();
        let target = path.join(&staged.name);
        staged.file.write_all(b"retained").unwrap();
        let control = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(SHARE_ALL)
            .open(&target)
            .unwrap();
        let original = Dacl::read(&control).unwrap();
        Dacl::deny_access(&staged.file).unwrap();
        staged.persisted = true;
        drop(staged);
        let read = File::open(&target);
        let access = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .open(&target);
        // Restore the fixture before assertions through a handle opened
        // before denying access, so a regression can still be cleaned up.
        original.apply(&control).unwrap();
        drop(control);
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(access.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&target).unwrap(), b"retained");
    }

    #[test]
    fn overwrite_preserves_an_explicit_integrity_label() {
        use std::{os::windows::process::CommandExt, process::Command};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("labelled.md");
        fs::write(&path, b"original").unwrap();
        let label = Command::new("icacls.exe")
            .arg(&path)
            .args(["/setintegritylevel", "L"])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            label.status.success(),
            "{}",
            String::from_utf8_lossy(&label.stderr)
        );
        let original = File::open(&path).unwrap();
        let security = Dacl::read(&original).unwrap();
        assert!(!security.label.is_null());
        drop(original);
        let destination = save_path(&path).unwrap();
        write_atomic(&destination, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"saved")
        })
        .unwrap();
        security.verify(&File::open(&path).unwrap()).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"saved");
    }
}
