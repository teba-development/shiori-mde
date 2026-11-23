fn main() {
    println!("cargo:rerun-if-env-changed=SHIORI_FRONTEND_SNAPSHOT");
    println!("cargo:rerun-if-env-changed=SHIORI_FRONTEND_SHA256");
    // tauri-build generates and then copies ACLs under the project directory,
    // even when Cargo uses a separate target directory.
    #[cfg(windows)]
    let _generated = lock_generated_files().expect("could not lock Tauri's generated files");
    tauri_build::build();
}

#[cfg(windows)]
fn lock_generated_files() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::{
        fs::{self, OpenOptions},
        io,
        os::windows::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        },
        path::PathBuf,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, SECURITY_IDENTIFICATION,
    };

    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").ok_or(io::ErrorKind::InvalidInput)?;
    let generated = PathBuf::from(manifest).join("gen");
    fs::create_dir_all(&generated)?;
    let directory = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(&generated)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(generated.join(".shiori-build.lock"))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: lock owns a live handle and info is writable for this structure.
    if unsafe { GetFileInformationByHandle(lock.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if !lock.metadata()?.is_file()
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    lock.lock()?;
    Ok((directory, lock))
}
