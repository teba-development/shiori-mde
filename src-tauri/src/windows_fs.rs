use std::{
    ffi::{OsStr, OsString},
    fs::{File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf},
    ptr::{null, null_mut},
};

use sha2::{Digest, Sha256};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
            NTCREATEFILE_CREATE_DISPOSITION,
        },
    },
    Win32::{
        Foundation::{
            RtlNtStatusToDosError, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED, HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE,
            OBJ_DONT_REPARSE, UNICODE_STRING,
        },
        Storage::FileSystem::{
            FileDispositionInfo, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW,
            GetVolumeInformationByHandleW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            DELETE, FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
            FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
            FILE_TYPE_DISK, FILE_WRITE_DATA, READ_CONTROL, SECURITY_IDENTIFICATION, SYNCHRONIZE,
            VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
        },
        System::{
            SystemServices::{FILE_NAMED_STREAMS, FILE_PERSISTENT_ACLS},
            IO::IO_STATUS_BLOCK,
        },
    },
};

use crate::file_io::{valid_file_component, MAX_DOCUMENT_BYTES};

pub(crate) mod recovery {
    include!("save_recovery.rs");
}
mod file_security {
    include!("file_security.rs");
}
mod named_streams {
    include!("named_streams.rs");
}

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const ATTRIBUTES: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const TRAVERSE: u32 = FILE_TRAVERSE | ATTRIBUTES;
const MAX_COMPONENT_UNITS: usize = 255;

pub fn opened_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0; 32_768];
    // SAFETY: file owns a live handle and buffer is writable for the supplied number of UTF-16 units.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "パスが長すぎます。",
        ));
    }
    let path = PathBuf::from(OsString::from_wide(&buffer[..length]));
    Ok(dunce::simplified(&path).to_path_buf())
}

pub fn open_directory(path: &Path) -> io::Result<File> {
    // An explicitly selected folder may resolve through a junction. Its resolved
    // handle is retained with delete sharing so editing does not pin it in place.
    let folder = OpenOptions::new()
        .access_mode(ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    require_disk(&folder)?;
    if !folder.metadata()?.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(folder)
}

pub fn open_selected_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    require_disk(&file)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(file)
}

pub struct FileVersion {
    identity: File,
    digest: [u8; 32],
}

impl FileVersion {
    pub fn from_bytes(identity: File, bytes: &[u8]) -> Self {
        Self {
            identity,
            digest: Sha256::digest(bytes).into(),
        }
    }
}

fn content_digest(file: &File) -> io::Result<[u8; 32]> {
    // The caller holds a share mode that excludes writers. Bound the scan
    // by the size of that opened object, independent of the output limit.
    digest_bytes(file, file.metadata()?.len())
}

fn digest_bytes(mut file: &File, length: u64) -> io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    if io::copy(&mut file.take(length), &mut hasher)? != length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(hasher.finalize().into())
}

struct Volume {
    flags: u32,
    name: String,
}

impl Volume {
    fn read(file: &File) -> io::Result<Self> {
        let mut flags = 0;
        let mut name = [0u16; 261];
        // SAFETY: file owns a live handle and the requested outputs are writable.
        if unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                &mut flags,
                name.as_mut_ptr(),
                name.len() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let length = name
            .iter()
            .position(|unit| *unit == 0)
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok(Self {
            flags,
            name: String::from_utf16_lossy(&name[..length]),
        })
    }

    fn has_acl(&self) -> bool {
        self.flags & FILE_PERSISTENT_ACLS != 0
    }

    fn has_named_streams(&self) -> bool {
        self.flags & FILE_NAMED_STREAMS != 0
    }

    fn id_may_change_on_rename(&self) -> bool {
        !self.has_acl()
            && ["FAT", "FAT32", "exFAT"]
                .iter()
                .any(|name| self.name.eq_ignore_ascii_case(name))
    }
}

pub fn capture_destination(
    directory: &File,
    path: &Path,
    name: &OsStr,
) -> io::Result<Option<FileVersion>> {
    check_directory_path(directory, path)?;
    match open_save_target(
        directory,
        name,
        FILE_READ_DATA | ATTRIBUTES,
        FILE_SHARE_READ,
    ) {
        Ok(file) => Ok(Some(FileVersion {
            identity: hold_identity(directory, name, &file)?,
            digest: content_digest(&file)?,
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_save_target(directory: &File, name: &OsStr, access: u32, share: u32) -> io::Result<File> {
    let file = open_child(directory, name, access, share)?;
    require_disk(&file)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "保存先にはリンク以外の通常のファイルを指定してください。",
        ));
    }
    Ok(file)
}

fn require_disk(file: &File) -> io::Result<()> {
    // SAFETY: file owns a live handle for the duration of the call.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}

fn check_directory_path(directory: &File, expected_path: &Path) -> io::Result<()> {
    if opened_path(directory)? != expected_path {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "選択したフォルダーの場所が変わりました。",
        ));
    }
    Ok(())
}

fn open_child(directory: &File, name: &OsStr, access: u32, share: u32) -> io::Result<File> {
    open_component(directory, name, access, share, FILE_OPEN)
}

fn open_component(
    directory: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
) -> io::Result<File> {
    open_component_with_options(directory, name, access, share, disposition, 0)
}

fn open_component_with_options(
    directory: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: u32,
) -> io::Result<File> {
    if !name.to_str().is_some_and(valid_file_component) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut wide: Vec<u16> = name.encode_wide().take(MAX_COMPONENT_UNITS + 1).collect();
    if wide.len() > MAX_COMPONENT_UNITS {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    open_relative_name(directory, &mut wide, access, share, disposition, options)
}

// Callers validate either a single component or a component with a named
// $DATA stream suffix before passing this counted name to Windows.
fn open_relative_name(
    directory: &File,
    wide: &mut [u16],
    access: u32,
    share: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: u32,
) -> io::Result<File> {
    let length = u16::try_from(wide.len())
        .ok()
        .and_then(|units| units.checked_mul(size_of::<u16>() as u16))
        .ok_or(io::ErrorKind::InvalidInput)?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle(),
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE
            | if disposition == FILE_CREATE {
                OBJ_DONT_REPARSE
            } else {
                0
            },
        ..Default::default()
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle = null_mut();
    // SAFETY: directory and the counted, single-component UTF-16 name remain
    // live for the synchronous call. Both output pointers are writable and the
    // optional allocation size and extended attributes are absent.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            null(),
            0,
            share,
            disposition,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | options,
            null(),
            0,
        )
    };
    check_status(status)?;
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other("Windows returned an invalid file handle"));
    }
    // SAFETY: a successful open transfers ownership of the synchronous file handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn check_status(status: i32) -> io::Result<()> {
    if status < 0 {
        // SAFETY: the function accepts any NTSTATUS value.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

pub struct StagedFile {
    file: File,
    name: String,
    persisted: bool,
    stream_handles: Vec<File>,
}

pub struct OriginalFile {
    file: File,
    digest: [u8; 32],
    dacl: Option<file_security::Dacl>,
    streams: Option<named_streams::NamedStreams>,
}

impl OriginalFile {
    fn open(directory: &File, name: &OsStr, expected: &FileVersion) -> io::Result<Self> {
        // Some remote filesystems cannot report volume flags. Preserve the
        // descriptor in that case so a failed query cannot relax permissions.
        let volume = Volume::read(directory).ok();
        let has_acl = volume.as_ref().is_none_or(Volume::has_acl);
        let extra_access = if has_acl { READ_CONTROL } else { 0 };
        let file = open_save_target(
            directory,
            name,
            FILE_READ_DATA | ATTRIBUTES | DELETE | extra_access,
            FILE_SHARE_READ,
        )
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                changed_destination()
            } else {
                error
            }
        })?;
        if !same_file(&expected.identity, &file)? {
            return Err(changed_destination());
        }
        let metadata = file.metadata()?;
        check_overwrite_attributes(metadata.file_attributes())?;
        let digest = content_digest(&file)?;
        if digest != expected.digest {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "保存先の内容が外部で変更されました。開き直すか、別の保存先を指定してください。",
            ));
        }
        if metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "保存先のファイルは読み取り専用です。",
            ));
        }
        let dacl = has_acl
            .then(|| file_security::Dacl::read(&file))
            .transpose()?;
        let streams = volume
            .as_ref()
            .is_none_or(Volume::has_named_streams)
            .then(|| named_streams::NamedStreams::open(directory, name, &file))
            .transpose()?;
        Ok(Self {
            file,
            digest,
            dacl,
            streams,
        })
    }

    fn verify_saved_metadata(&self, staged: &File) -> io::Result<()> {
        if let Some(streams) = &self.streams {
            streams.verify(&self.file)?;
            streams.verify(staged)?;
        }
        if let Some(dacl) = &self.dacl {
            dacl.verify(&self.file)?;
            dacl.verify(staged)?;
        }
        Ok(())
    }
}

fn check_overwrite_attributes(attributes: u32) -> io::Result<()> {
    if attributes & FILE_ATTRIBUTE_ENCRYPTED != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "元ファイルのEFS暗号化を維持できないため、上書きを中止しました。",
        ));
    }
    Ok(())
}

impl StagedFile {
    pub fn create(directory: &File, path: &Path) -> io::Result<Self> {
        Self::create_with_access(directory, path, 0)
    }

    pub fn begin_document(
        directory: &File,
        path: &Path,
        name: &OsStr,
        expected: Option<&FileVersion>,
    ) -> io::Result<(Self, Option<OriginalFile>)> {
        check_directory_path(directory, path)?;
        let original = expected
            .map(|version| OriginalFile::open(directory, name, version))
            .transpose()?;
        let dacl = original
            .as_ref()
            .and_then(|original| original.dacl.as_ref());
        let mut staged = if let Some(dacl) = dacl {
            let staged = Self::create_with_access(directory, path, READ_CONTROL | WRITE_DAC)?;
            if !dacl.identity_matches(&staged.file)? {
                // WRITE_OWNER is needed only when the temporary file's owner
                // differs. Keep the exclusive data handle throughout.
                let apply_owner = || {
                    let handle = open_save_target(
                        directory,
                        OsStr::new(&staged.name),
                        ATTRIBUTES | READ_CONTROL | WRITE_OWNER,
                        SHARE_ALL,
                    )
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("整合性ラベル用ハンドルを開けませんでした。\n{error}"),
                        )
                    })?;
                    if !same_file(&handle, &staged.file)? {
                        return Err(changed_destination());
                    }
                    dacl.apply_identity(&handle)
                };
                apply_owner().map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "元ファイルの所有者・整合性ラベルを維持できませんでした。\n{error}"
                        ),
                    )
                })?;
            }
            dacl.apply(&staged.file)?;
            staged
        } else {
            Self::create(directory, path)?
        };
        if let Some(streams) = original
            .as_ref()
            .and_then(|original| original.streams.as_ref())
        {
            staged.stream_handles =
                streams.copy_to(directory, OsStr::new(&staged.name), &staged.file)?;
        }
        Ok((staged, original))
    }

    fn create_with_access(directory: &File, path: &Path, access: u32) -> io::Result<Self> {
        check_directory_path(directory, path)?;
        for _ in 0..8 {
            let suffix = getrandom::u64().map_err(io::Error::other)?;
            let name = format!(".shiori-{suffix:016x}.tmp");
            match open_component(
                directory,
                OsStr::new(&name),
                FILE_READ_DATA | FILE_WRITE_DATA | ATTRIBUTES | DELETE | access,
                0,
                FILE_CREATE,
            ) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        name,
                        persisted: false,
                        stream_handles: Vec::new(),
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    fn publish(mut self, name: &OsStr) -> io::Result<()> {
        rename_at(&self.file, None, name)?;
        self.persisted = true;
        Ok(())
    }

    pub fn persist_document(
        mut self,
        directory: &File,
        path: &Path,
        name: &OsStr,
        original: Option<OriginalFile>,
        recovery_dir: &Path,
    ) -> io::Result<FileVersion> {
        check_directory_path(directory, path)?;
        if self.file.metadata()?.len() > MAX_DOCUMENT_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "出力ファイルは64 MiBまでです。",
            ));
        }
        // Keep an attribute-only handle to the new file before releasing the
        // writer. This records its identity without reopening the final name.
        let saved = FileVersion {
            identity: hold_identity(directory, OsStr::new(&self.name), &self.file)?,
            digest: content_digest(&self.file)?,
        };
        let Some(original) = original else {
            self.publish(name)?;
            return Ok(saved);
        };
        // Move only the verified object, then claim the now-free name without
        // replacing anything that another process may create in the meantime.
        let record = recovery::SaveRecord::new(directory, path, name, &original, &self, &saved)?;
        let journal = record.start(recovery_dir)?;
        if let Err(error) = rename_at(&original.file, None, OsStr::new(&record.backup)) {
            journal.remove();
            return Err(error);
        }
        // The original name is no longer available. Check its streams and
        // access rules on the held backup before exposing the replacement.
        let publish = original
            .verify_saved_metadata(&self.file)
            .and_then(|()| rename_at(&self.file, None, name));
        if let Err(error) = publish {
            return match rename_at(&original.file, None, name) {
                Ok(()) => {
                    journal.remove();
                    Err(error)
                }
                Err(restore) => Err(restore_error(path, &record.backup, error, restore)),
            };
        }
        self.persisted = true;
        if let Err(error) = self.verify_published(directory, name, &original) {
            if !self.persisted {
                journal.remove();
            }
            return Err(error);
        }
        let cleanup = recovery::mark_cleanup(&journal, &record)
            .and_then(|()| delete_on_close(&original.file));
        if let Err(error) = cleanup {
            // The new file was already committed. Keep the backup and return
            // the new identity so the caller can continue saving this document.
            eprintln!(
                "Could not remove previous file {}: {error}",
                path.join(&record.backup).display()
            );
        } else {
            journal.remove();
        }
        Ok(saved)
    }

    fn verify_published(
        &mut self,
        directory: &File,
        name: &OsStr,
        original: &OriginalFile,
    ) -> io::Result<()> {
        if let Err(error) = original.verify_saved_metadata(&self.file) {
            let restore = rename_at(&self.file, None, OsStr::new(&self.name))
                .and_then(|()| rename_at(&original.file, None, name));
            if let Err(restore) = restore {
                let retained = opened_path(&self.file)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| self.name.clone());
                if let Err(protection) = self.apply_current_security(directory, original) {
                    let blocked = file_security::Dacl::deny_access(&self.file);
                    let state = match blocked {
                        Ok(()) => "復旧用ファイルへのアクセスを禁止して保全しました。".to_owned(),
                        Err(blocked) => {
                            format!("復旧用ファイルのアクセス禁止にも失敗しました。\n{blocked}")
                        }
                    };
                    return Err(io::Error::other(format!(
                        "保存を中止しました。{state}\n{}\n{error}\n{restore}\n{protection}",
                        retained,
                    )));
                }
                return Err(io::Error::other(format!(
                    "保存後のアクセス権または付加データが変わりました。元ファイルの最新の権限を反映して復旧用ファイルを保持しました。\n{}\n{error}\n{restore}",
                    retained,
                )));
            }
            self.persisted = false;
            return Err(error);
        }
        Ok(())
    }

    fn apply_current_security(&self, directory: &File, original: &OriginalFile) -> io::Result<()> {
        if original.dacl.is_none() {
            return Ok(());
        }
        let current = file_security::Dacl::read(&original.file)?;
        if !current.identity_matches(&self.file)? {
            let path = opened_path(&self.file)?;
            let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
            let identity = open_save_target(
                directory,
                name,
                ATTRIBUTES | READ_CONTROL | WRITE_OWNER,
                SHARE_ALL,
            )?;
            if !same_file(&identity, &self.file)? {
                return Err(changed_destination());
            }
            current.apply_identity(&identity)?;
        }
        current.apply(&self.file)?;
        current.verify(&original.file)
    }
}

fn changed_destination() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "保存先のファイルが切り替わりました。開き直すか、別の保存先を指定してください。",
    )
}

fn restore_error(path: &Path, backup: &str, error: io::Error, restore: io::Error) -> io::Error {
    io::Error::other(format!(
        "保存を中止しました。元のファイルは {} に残っています。\n{error}\n復元: {restore}",
        path.join(backup).display()
    ))
}

fn rename_at(file: &File, parent: Option<&File>, name: &OsStr) -> io::Result<()> {
    if !name.to_str().is_some_and(valid_file_component) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut info = RenameInformation {
        replace_if_exists: 0,
        root_directory: parent.map_or(null_mut(), AsRawHandle::as_raw_handle),
        name_length: 0,
        name: [0; MAX_COMPONENT_UNITS + 2],
    };
    let mut units = 0;
    let name_buffer = &mut info.name[..MAX_COMPONENT_UNITS];
    for unit in name.encode_wide() {
        *name_buffer
            .get_mut(units)
            .ok_or(io::ErrorKind::InvalidInput)? = unit;
        units += 1;
    }
    info.name_length = (units * size_of::<u16>()) as u32;
    let length = size_of::<FILE_RENAME_INFORMATION>() + info.name_length as usize;
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the layout and buffer size are asserted below. The counted name
    // is a single component. A null parent keeps the file in its own directory.
    check_status(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            (&info as *const RenameInformation).cast(),
            length as u32,
            FileRenameInformation,
        )
    })
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = delete_on_close(&self.file);
        }
    }
}

fn delete_on_close(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the live handle has DELETE access, and info is the initialized
    // input structure for FileDispositionInfo.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[repr(C)]
struct RenameInformation {
    replace_if_exists: u32,
    root_directory: HANDLE,
    name_length: u32,
    name: [u16; MAX_COMPONENT_UNITS + 2],
}

const _: () = {
    assert!(
        offset_of!(RenameInformation, root_directory)
            == offset_of!(FILE_RENAME_INFORMATION, RootDirectory)
    );
    assert!(
        offset_of!(RenameInformation, name_length)
            == offset_of!(FILE_RENAME_INFORMATION, FileNameLength)
    );
    assert!(offset_of!(RenameInformation, name) == offset_of!(FILE_RENAME_INFORMATION, FileName));
    assert!(
        size_of::<RenameInformation>()
            >= size_of::<FILE_RENAME_INFORMATION>() + MAX_COMPONENT_UNITS * size_of::<u16>()
    );
};

fn reject_reparse(metadata: &Metadata) -> io::Result<()> {
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "リンクやジャンクションを経由する画像は、画像の挿入から直接選んでください。",
        ));
    }
    Ok(())
}

pub fn open_relative(root: &File, root_path: &Path, relative: &Path) -> io::Result<File> {
    reject_reparse(&root.metadata()?)?;
    check_directory_path(root, root_path)?;
    let mut directories = Vec::new();
    let mut parts = relative.components().peekable();
    while let Some(Component::Normal(name)) = parts.next() {
        let directory = directories.last().unwrap_or(root);
        if parts.peek().is_none() {
            let file = open_child(directory, name, FILE_READ_DATA | ATTRIBUTES, SHARE_ALL)?;
            let metadata = file.metadata()?;
            reject_reparse(&metadata)?;
            if !metadata.is_file() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            if basic_file_info(&file)?.nNumberOfLinks != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ハードリンクの画像は、画像の挿入から直接選んでください。",
                ));
            }
            return Ok(file);
        }
        let child = open_child(directory, name, TRAVERSE, FILE_SHARE_READ)?;
        let metadata = child.metadata()?;
        reject_reparse(&metadata)?;
        if !metadata.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        // Keep ancestors open without write/delete sharing until the file is open.
        directories.push(child);
    }
    Err(io::ErrorKind::InvalidInput.into())
}

pub fn verify_child(root: &File, root_path: &Path, name: &OsStr, file: &File) -> io::Result<File> {
    check_directory_path(root, root_path)?;
    let child = open_save_target(root, name, FILE_READ_DATA | ATTRIBUTES, FILE_SHARE_READ)?;
    if !same_file(&child, file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "選択したファイルの参照先が変わりました。",
        ));
    }
    Ok(child)
}

pub fn hold_identity(root: &File, name: &OsStr, file: &File) -> io::Result<File> {
    let identity = open_save_target(root, name, ATTRIBUTES, SHARE_ALL)?;
    if !same_file(&identity, file)? {
        return Err(changed_destination());
    }
    Ok(identity)
}

pub fn same_file(first: &File, second: &File) -> io::Result<bool> {
    Ok(file_id(first)? == file_id(second)?)
}

#[derive(PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum FileId {
    Extended(u64, [u8; 16]),
    Legacy(u32, u64),
}

impl FileId {
    fn same_volume(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Extended(first, _), Self::Extended(second, _)) => first == second,
            (Self::Legacy(first, _), Self::Legacy(second, _)) => first == second,
            _ => false,
        }
    }
}

fn file_id(file: &File) -> io::Result<FileId> {
    let mut id = FILE_ID_INFO::default();
    // SAFETY: file owns a live handle. FileIdInfo writes a FILE_ID_INFO into
    // the correctly sized and aligned output buffer.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_FUNCTION | ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)
        ) {
            return Err(error);
        }
    } else if id.FileId.Identifier != [0; 16] {
        return Ok(FileId::Extended(
            id.VolumeSerialNumber,
            id.FileId.Identifier,
        ));
    }
    // Filesystems without 128-bit IDs can report an all-zero identifier.
    let legacy = basic_file_info(file)?;
    let index = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ファイルの識別情報を取得できません。",
        ));
    }
    Ok(FileId::Legacy(legacy.dwVolumeSerialNumber, index))
}

fn basic_file_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle and info is writable for the complete structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read};

    #[test]
    fn relative_open_stays_with_directory_after_its_name_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("images");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("fixture.svg"), b"original").unwrap();
        let folder = open_directory(&path).unwrap();
        check_directory_path(&folder, &opened_path(&folder).unwrap()).unwrap();

        fs::rename(&path, tmp.path().join("previous")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("fixture.svg"), b"replacement").unwrap();

        let mut file = open_child(
            &folder,
            OsStr::new("fixture.svg"),
            FILE_READ_DATA | ATTRIBUTES,
            SHARE_ALL,
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }

    #[test]
    fn a_directory_in_the_walk_cannot_be_renamed_while_held() {
        let tmp = tempfile::tempdir().unwrap();
        let child_path = tmp.path().join("images");
        let new_path = tmp.path().join("moved");
        fs::create_dir(&child_path).unwrap();
        let root = open_directory(tmp.path()).unwrap();
        let child = open_child(&root, OsStr::new("images"), TRAVERSE, FILE_SHARE_READ).unwrap();
        assert!(fs::rename(&child_path, &new_path).is_err());
        drop(child);
        fs::rename(&child_path, &new_path).unwrap();
    }

    #[test]
    fn a_replacement_document_cannot_supply_the_image_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.md");
        fs::write(&path, b"original").unwrap();
        let file = File::open(&path).unwrap();
        let folder = open_directory(tmp.path()).unwrap();
        let folder_path = opened_path(&folder).unwrap();
        let name = path.file_name().unwrap();
        verify_child(&folder, &folder_path, name, &file).unwrap();

        fs::rename(&path, tmp.path().join("previous.md")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(verify_child(&folder, &folder_path, name, &file).is_err());
    }

    #[test]
    fn a_captured_destination_detects_external_content_changes() {
        use crate::file_io::{save_path, write_atomic};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("文書.md");
        fs::write(&path, b"original").unwrap();
        let approved = save_path(&path).unwrap();
        fs::write(&path, b"external").unwrap();
        let result = write_atomic(&approved, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"edited")
        });
        let error = result.err().expect("external edits must be preserved");
        assert!(error.contains("内容が外部で変更"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
