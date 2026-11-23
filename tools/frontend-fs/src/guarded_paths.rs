pub mod guarded_paths {
    use super::*;
    use std::path::Prefix;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_IF,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    };

    pub const SNAPSHOT_FILE: &str = "frontend.bundle";

    pub fn directory(file: &File) -> io::Result<()> {
        require_disk(file)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Expected a regular directory",
            ));
        }
        Ok(())
    }

    pub fn pin_directory(path: &Path, create_missing: bool) -> io::Result<Vec<File>> {
        let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
        if path.as_os_str().encode_wide().count() > 32767 || !path.is_absolute() {
            return Err(invalid());
        }
        let mut parts = path.components();
        let prefix = match parts.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => prefix,
                Prefix::UNC(host, share) | Prefix::VerbatimUNC(host, share)
                    if host.to_str().is_some_and(valid_file_component)
                        && share.to_str().is_some_and(|share| {
                            valid_file_component(share)
                                && !share.eq_ignore_ascii_case("pipe")
                                && !share.eq_ignore_ascii_case("mailslot")
                        }) =>
                {
                    prefix
                }
                _ => return Err(invalid()),
            },
            _ => return Err(invalid()),
        };
        if parts.next() != Some(Component::RootDir) {
            return Err(invalid());
        }
        let mut volume = PathBuf::from(prefix.as_os_str());
        volume.push(r"\");
        // Deny delete sharing to pin the namespace. The destination-parent
        // open performed by Windows for rename still needs write sharing.
        let first = OpenOptions::new()
            .access_mode(TRAVERSE | FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(volume)?;
        directory(&first)?;
        let mut pins = vec![first];
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(invalid());
            };
            let child = open_component_with_options(
                pins.last().ok_or_else(invalid)?,
                name,
                TRAVERSE | FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                if create_missing {
                    FILE_OPEN_IF
                } else {
                    FILE_OPEN
                },
                FILE_DIRECTORY_FILE,
            )?;
            directory(&child)?;
            pins.push(child);
        }
        Ok(pins)
    }

    pub fn open_regular_file(parent: &File, name: &str) -> io::Result<File> {
        let file = open_component_with_options(
            parent,
            OsStr::new(name),
            FILE_READ_DATA | ATTRIBUTES,
            FILE_SHARE_READ,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
        )?;
        require_disk(&file)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || basic_file_info(&file)?.nNumberOfLinks != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Frontend input or output must be a regular file without links",
            ));
        }
        Ok(file)
    }

    pub fn open_snapshot(parent: &File, stage: &str, name: &str) -> io::Result<File> {
        let folder = open_component_with_options(
            parent,
            OsStr::new(stage),
            TRAVERSE,
            // The build helper already holds the directory with DELETE access.
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?;
        directory(&folder)?;
        open_regular_file(&folder, name)
    }
}
