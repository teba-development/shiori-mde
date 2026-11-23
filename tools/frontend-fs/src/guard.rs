pub mod frontend {
    use super::guarded_paths::{directory, open_regular_file, pin_directory, SNAPSHOT_FILE};
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, HashMap},
        io::{Read, Write},
        os::windows::io::OwnedHandle,
    };
    use windows_sys::Wdk::Storage::FileSystem::{
        FileNamesInformation, NtQueryDirectoryFile, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE,
        FILE_NON_DIRECTORY_FILE, FILE_OPEN_IF,
    };
    use windows_sys::Win32::{
        Foundation::STATUS_NO_MORE_FILES, Storage::FileSystem::FILE_LIST_DIRECTORY,
    };

    const OUTPUT_BYTES: usize = 128 * 1024 * 1024;
    const OUTPUT_FILES: usize = 4097; // Up to 4096 esbuild outputs and index.html.
    const INDEX_BYTES: usize = 128 * 1024;
    const CLEANUP_ENTRIES: usize = 8192;
    const RELEASE_LOCK: &str = ".shiori-release.lock";

    fn invalid() -> io::Error {
        io::ErrorKind::InvalidInput.into()
    }
    fn parent_process(pid: u32) -> io::Result<OwnedHandle> {
        use windows_sys::Win32::System::Threading::OpenProcess;
        if pid == 0 || pid == std::process::id() {
            return Err(invalid());
        }
        // SAFETY: only SYNCHRONIZE access is requested for the caller's process.
        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call transferred ownership of the process handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(process) })
    }
    pub fn watch_parent(pid: u32) -> io::Result<()> {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{WaitForSingleObject, INFINITE},
        };
        let process = parent_process(pid)?;
        std::thread::Builder::new().spawn(move || {
            // SAFETY: this thread owns the process handle until the wait ends.
            let result = unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
            // A forced parent exit may leave inherited pipe writers in other
            // children. Release the capabilities even without stdin EOF.
            std::process::exit(i32::from(result != WAIT_OBJECT_0));
        })?;
        Ok(())
    }
    mod release {
        include!("release.rs");
    }
    pub use release::run as run_release;
    fn child_dir(parent: &File, name: &str, create: bool, delete: bool) -> io::Result<File> {
        let file = open_component_with_options(
            parent,
            OsStr::new(name),
            TRAVERSE | FILE_LIST_DIRECTORY | if delete { DELETE } else { 0 },
            FILE_SHARE_READ,
            if create { FILE_CREATE } else { FILE_OPEN },
            FILE_DIRECTORY_FILE,
        )?;
        directory(&file)?;
        Ok(file)
    }
    fn rename_directory(file: &File, parent: &File, name: &str) -> io::Result<()> {
        rename_at(file, Some(parent), OsStr::new(name))
    }
    fn names(file: &File, restart: bool) -> io::Result<Option<Vec<OsString>>> {
        const DIRECTORY_BUFFER: usize = 64 * 1024;
        let mut buffer = vec![0u64; DIRECTORY_BUFFER / size_of::<u64>()];
        let mut status = IO_STATUS_BLOCK::default();
        // SAFETY: the directory handle supports listing, the output buffer is
        // aligned and writable for 65536 bytes, and optional pointers are null.
        let result = unsafe {
            NtQueryDirectoryFile(
                file.as_raw_handle(),
                null_mut(),
                None,
                null(),
                &mut status,
                buffer.as_mut_ptr().cast(),
                DIRECTORY_BUFFER as u32,
                FileNamesInformation,
                false,
                null(),
                restart,
            )
        };
        if result == STATUS_NO_MORE_FILES {
            return Ok(None);
        }
        check_status(result)?;
        if status.Information == 0 || status.Information > DIRECTORY_BUFFER {
            return Err(invalid());
        }
        // SAFETY: Information was checked against the initialized allocation.
        let mut bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), status.Information) };
        let mut result = Vec::new();
        loop {
            let header = bytes.first_chunk::<12>().ok_or_else(invalid)?;
            let next = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(header[8..].try_into().unwrap()) as usize;
            if length == 0 || length > MAX_COMPONENT_UNITS * 2 || !length.is_multiple_of(2) {
                return Err(invalid());
            }
            let content = bytes.get(12..12 + length).ok_or_else(invalid)?;
            let units: Vec<u16> = content
                .as_chunks::<2>()
                .0
                .iter()
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .collect();
            let name = OsString::from_wide(&units);
            if name != "." && name != ".." {
                result.push(name);
            }
            if next == 0 {
                break;
            }
            if next < 12 + length || !next.is_multiple_of(4) || next > bytes.len() - 12 {
                return Err(invalid());
            }
            bytes = &bytes[next..];
        }
        Ok(Some(result))
    }
    fn remove_tree(file: &File) -> io::Result<()> {
        let mut budget = CLEANUP_ENTRIES;
        remove_entries(file, &mut budget, 0)
    }
    fn remove_entries(file: &File, budget: &mut usize, depth: usize) -> io::Result<()> {
        if depth > 32 {
            return Err(invalid());
        }
        let metadata = file.metadata()?;
        if metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            let mut restart = true;
            let mut children = Vec::new();
            while let Some(entries) = names(file, restart)? {
                restart = false;
                *budget = budget.checked_sub(entries.len()).ok_or_else(invalid)?;
                children.extend(entries);
            }
            for name in children {
                let child = match open_child(
                    file,
                    &name,
                    ATTRIBUTES | FILE_LIST_DIRECTORY | DELETE,
                    FILE_SHARE_READ,
                ) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                // Reparse points are unlinked themselves, never traversed.
                remove_entries(&child, budget, depth + 1)?;
            }
        }
        delete_on_close(file)
    }
    struct Asset {
        bytes: usize,
        digest: [u8; 32],
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum DirectoryKind {
        Build,
        Dev,
        Package,
    }

    struct OwnedDirectory {
        file: File,
        path: PathBuf,
        kind: DirectoryKind,
        assets: BTreeMap<String, Asset>,
        bytes: usize,
        readers: Option<Vec<File>>,
    }

    impl OwnedDirectory {
        fn verify(&self) -> io::Result<Vec<File>> {
            let required = match self.kind {
                DirectoryKind::Build => "index.html",
                DirectoryKind::Package => SNAPSHOT_FILE,
                DirectoryKind::Dev => return Err(invalid()),
            };
            if !self.assets.contains_key(required) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Missing required frontend output",
                ));
            }
            directory(&self.file)?;
            let invalid_output = || {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Generated frontend files changed",
                )
            };
            let mut restart = true;
            let mut count = 0;
            while let Some(entries) = names(&self.file, restart)? {
                restart = false;
                for name in entries {
                    count += 1;
                    if count > self.assets.len()
                        || !name
                            .to_str()
                            .is_some_and(|name| self.assets.contains_key(name))
                    {
                        return Err(invalid_output());
                    }
                }
            }
            if count != self.assets.len() {
                return Err(invalid_output());
            }
            let mut readers = Vec::with_capacity(count);
            for (name, asset) in &self.assets {
                let mut file = open_regular_file(&self.file, name)?;
                if file.metadata()?.len() != asset.bytes as u64 {
                    return Err(invalid_output());
                }
                let mut digest = Sha256::new();
                let count = io::copy(
                    &mut Read::by_ref(&mut file).take(asset.bytes as u64 + 1),
                    &mut digest,
                )?;
                if count != asset.bytes as u64
                    || <[u8; 32]>::from(digest.finalize()) != asset.digest
                {
                    return Err(invalid_output());
                }
                readers.push(file);
            }
            Ok(readers)
        }
    }
    struct Pending {
        file: StagedFile,
        directory: String,
        name: String,
        size: usize,
        bytes: usize,
        digest: Sha256,
    }

    pub struct Session {
        pins: Vec<File>,
        root_path: PathBuf,
        directories: HashMap<String, OwnedDirectory>,
        pending: HashMap<String, Pending>,
    }
    impl Session {
        pub fn new(path: &Path, expected_volume: u64, expected_id: u64) -> io::Result<Self> {
            let pins = pin_directory(path, false)?;
            let root = pins.last().ok_or_else(invalid)?;
            let identity = basic_file_info(root)?;
            let id = (u64::from(identity.nFileIndexHigh) << 32) | u64::from(identity.nFileIndexLow);
            if id == 0
                || id != expected_id
                || u64::from(identity.dwVolumeSerialNumber) != expected_volume
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Project directory identity changed",
                ));
            }
            let root_path = opened_path(root)?;
            Ok(Self {
                pins,
                root_path,
                directories: HashMap::new(),
                pending: HashMap::new(),
            })
        }
        fn root(&self) -> &File {
            self.pins.last().expect("root handle")
        }
        pub fn root_path(&self) -> &Path {
            &self.root_path
        }
        pub fn validate_root(&self) -> io::Result<()> {
            for pin in &self.pins {
                directory(pin)?;
            }
            Ok(())
        }
        pub fn read(&self, name: &str) -> io::Result<(Vec<u8>, bool)> {
            if name.len() > 8192 {
                return Err(invalid());
            }
            let mut parts: Vec<_> = name.split('/').collect();
            if parts.len() > 32 || parts.iter().any(|part| !valid_file_component(part)) {
                return Err(invalid());
            }
            let filename = parts.pop().ok_or_else(invalid)?;
            let (limit, binary) = match (parts.first().copied(), filename) {
                (None, "index.html" | "tsconfig.json") => (64 * 1024, false),
                (Some("src"), filename) if filename.ends_with(".ts") => (2 * 1024 * 1024, false),
                (Some("src"), filename) if filename.ends_with(".css") => (1024 * 1024, false),
                (Some("assets"), filename) if filename.ends_with(".png") => (8 * 1024 * 1024, true),
                _ => return Err(invalid()),
            };
            let mut parents = Vec::with_capacity(parts.len());
            let mut parent = self.root();
            for part in parts {
                parents.push(child_dir(parent, part, false, false)?);
                parent = parents.last().ok_or_else(invalid)?;
            }
            let mut file = open_regular_file(parent, filename)?;
            let length = file.metadata()?.len();
            if length > limit as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Frontend input exceeds its byte limit",
                ));
            }
            let mut bytes = Vec::with_capacity(length as usize);
            Read::by_ref(&mut file)
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(invalid());
            }
            if !binary && bytes.contains(&0) {
                return Err(invalid());
            }
            Ok((bytes, binary))
        }
        fn token() -> io::Result<String> {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).map_err(io::Error::other)?;
            Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
        }
        pub fn make(&mut self, kind: &str) -> io::Result<(String, PathBuf)> {
            if self.directories.len() >= 16 {
                return Err(invalid());
            }
            let layout = match kind {
                "build" => DirectoryKind::Build,
                "dev" => DirectoryKind::Dev,
                "package" => DirectoryKind::Package,
                _ => return Err(invalid()),
            };
            for _ in 0..8 {
                let id = Self::token()?;
                if self.directories.contains_key(&id) {
                    continue;
                }
                let name = format!(".frontend-{kind}-{id}");
                let file = match child_dir(self.root(), &name, true, true) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                };
                let path = self.root_path.join(name);
                self.directories.insert(
                    id.clone(),
                    OwnedDirectory {
                        file,
                        path: path.clone(),
                        kind: layout,
                        assets: BTreeMap::new(),
                        bytes: 0,
                        readers: None,
                    },
                );
                return Ok((id, path));
            }
            Err(io::ErrorKind::AlreadyExists.into())
        }
        pub fn check(&self, id: &str) -> io::Result<()> {
            directory(&self.directories.get(id).ok_or_else(invalid)?.file)
        }
        pub fn begin(&mut self, id: &str, name: &str, size: usize) -> io::Result<String> {
            if !valid_file_component(name)
                || name.encode_utf16().count() > 255
                || self.pending.len() >= 8
            {
                return Err(invalid());
            }
            let dir = self.directories.get(id).ok_or_else(invalid)?;
            let allowed = match dir.kind {
                DirectoryKind::Build => {
                    name == "index.html" && (1..=INDEX_BYTES).contains(&size)
                        || [".js", ".css", ".png", ".map", ".txt"]
                            .iter()
                            .any(|ext| name.ends_with(ext))
                }
                DirectoryKind::Package => name == SNAPSHOT_FILE && size >= 12,
                DirectoryKind::Dev => false,
            };
            let mut total = dir.bytes;
            let mut count = dir.assets.len();
            for pending in self
                .pending
                .values()
                .filter(|pending| pending.directory == id)
            {
                if pending.name == name {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                total += pending.size;
                count += 1;
            }
            if !allowed
                || dir.readers.is_some()
                || dir.assets.contains_key(name)
                || count >= OUTPUT_FILES
                || size > OUTPUT_BYTES.saturating_sub(total)
            {
                return Err(invalid());
            }
            directory(&dir.file)?;
            let token = Self::token()?;
            if self.pending.contains_key(&token) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            let file = StagedFile::create(&dir.file, &dir.path)?;
            self.pending.insert(
                token.clone(),
                Pending {
                    file,
                    directory: id.into(),
                    name: name.into(),
                    size,
                    bytes: 0,
                    digest: Sha256::new(),
                },
            );
            Ok(token)
        }
        pub fn append(&mut self, id: &str, bytes: &[u8]) -> io::Result<()> {
            let pending = self.pending.get_mut(id).ok_or_else(invalid)?;
            let count = pending
                .bytes
                .checked_add(bytes.len())
                .filter(|n| *n <= pending.size)
                .ok_or_else(invalid)?;
            pending.file.file_mut().write_all(bytes)?;
            pending.digest.update(bytes);
            pending.bytes = count;
            Ok(())
        }
        pub fn commit(&mut self, id: &str) -> io::Result<()> {
            let pending = self.pending.remove(id).ok_or_else(invalid)?;
            if pending.bytes != pending.size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Incomplete frontend output",
                ));
            }
            let dir = self
                .directories
                .get_mut(&pending.directory)
                .ok_or_else(invalid)?;
            directory(&dir.file)?;
            check_directory_path(&dir.file, &dir.path)?;
            pending.file.publish(OsStr::new(&pending.name))?;
            dir.bytes += pending.size;
            dir.assets.insert(
                pending.name,
                Asset {
                    bytes: pending.size,
                    digest: pending.digest.finalize().into(),
                },
            );
            Ok(())
        }
        pub fn abort(&mut self, id: &str) {
            self.pending.remove(id);
        }
        pub fn remove(&mut self, id: &str) -> io::Result<()> {
            if self.pending.values().any(|p| p.directory == id) {
                return Err(invalid());
            }
            let dir = self.directories.get_mut(id).ok_or_else(invalid)?;
            dir.readers.take();
            remove_tree(&dir.file)?;
            self.directories.remove(id);
            Ok(())
        }
        pub fn seal(&mut self, id: &str) -> io::Result<Vec<String>> {
            if self.pending.values().any(|p| p.directory == id) {
                return Err(invalid());
            }
            let dir = self.directories.get_mut(id).ok_or_else(invalid)?;
            if dir.readers.is_none() {
                dir.readers = Some(dir.verify()?);
            }
            Ok(dir.assets.keys().cloned().collect())
        }
        pub fn publish(&mut self, id: &str) -> io::Result<()> {
            if self.pending.values().any(|p| p.directory == id) {
                return Err(invalid());
            }
            let owned = self.directories.get(id).ok_or_else(invalid)?;
            if owned.kind != DirectoryKind::Build || owned.readers.is_some() {
                return Err(invalid());
            }
            let lock = open_component_with_options(
                self.root(),
                OsStr::new(".frontend-publish.lock"),
                ATTRIBUTES | DELETE,
                0,
                FILE_CREATE,
                FILE_DELETE_ON_CLOSE | FILE_NON_DIRECTORY_FILE,
            )
            .map_err(|error| io::Error::other(format!("Acquire publish lock: {error}")))?;
            // The kernel arms cleanup at creation, including process failure.
            let _lock = lock;
            let previous = match child_dir(self.root(), "dist", false, true) {
                Ok(file) => Some(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(io::Error::other(format!("Open previous output: {error}")))
                }
            };
            drop(owned.verify()?);
            let backup_name = format!(".frontend-previous-{}", Self::token()?);
            if let Some(previous) = &previous {
                rename_directory(previous, self.root(), &backup_name).map_err(|error| {
                    io::Error::other(format!("Rename previous output: {error}"))
                })?;
            }
            let mut moved = false;
            let publish = rename_directory(&owned.file, self.root(), "dist").and_then(|()| {
                moved = true;
                owned.verify()
            });
            let readers = match publish {
                Ok(readers) => readers,
                Err(error) => {
                    if moved {
                        let name = owned
                            .path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .ok_or_else(invalid)?;
                        if let Err(restore) = rename_directory(&owned.file, self.root(), name) {
                            // Another reader may have opened dist. Keep both trees
                            // intact so cleanup cannot delete the published path.
                            self.directories.remove(id);
                            return Err(io::Error::other(format!(
                                "Validate published output: {error}; move it back: {restore}; previous output (if present): {}",
                                self.root_path.join(&backup_name).display()
                            )));
                        }
                    }
                    if let Some(previous) = &previous {
                        if let Err(restore) = rename_directory(previous, self.root(), "dist") {
                            return Err(io::Error::other(format!("Publish failed: {error}; restore failed: {restore}; previous output: {}", self.root_path.join(&backup_name).display())));
                        }
                    }
                    return Err(io::Error::other(format!("Publish new output: {error}")));
                }
            };
            let published = self.directories.remove(id);
            if let Some(previous) = previous {
                if let Err(error) = remove_tree(&previous) {
                    eprintln!("Could not remove previous frontend output: {error}");
                }
            }
            drop(readers);
            drop(published);
            Ok(())
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            self.pending.clear();
            for (_, mut directory) in self.directories.drain() {
                directory.readers.take();
                if let Err(error) = remove_tree(&directory.file) {
                    eprintln!("Could not remove private frontend output: {error}");
                }
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        fn setup() -> (tempfile::TempDir, Session) {
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap();
            let temp = tempfile::tempdir_in(workspace.join(".build-tools")).unwrap();
            fs::create_dir(temp.path().join("src")).unwrap();
            fs::write(temp.path().join("src/main.ts"), "SOURCE CANARY").unwrap();
            let root = open_directory(temp.path()).unwrap();
            let info = basic_file_info(&root).unwrap();
            let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
            drop(root);
            let session =
                Session::new(temp.path(), u64::from(info.dwVolumeSerialNumber), id).unwrap();
            (temp, session)
        }
        #[test]
        fn source_handles_block_directory_and_file_replacement_during_read() {
            let (temp, session) = setup();
            let source = child_dir(session.root(), "src", false, false).unwrap();
            let file = open_child(
                &source,
                OsStr::new("main.ts"),
                FILE_READ_DATA | ATTRIBUTES,
                FILE_SHARE_READ,
            )
            .unwrap();
            assert!(fs::rename(temp.path().join("src"), temp.path().join("moved-src")).is_err());
            assert!(
                fs::rename(temp.path().join("src/main.ts"), temp.path().join("old.ts")).is_err()
            );
            assert!(fs::write(temp.path().join("src/main.ts"), "REPLACEMENT").is_err());
            drop(file);
            drop(source);
            assert_eq!(session.read("src/main.ts").unwrap().0, b"SOURCE CANARY");
        }
        #[test]
        fn staged_file_name_cannot_be_swapped_before_commit_or_abort() {
            let (_temp, mut session) = setup();
            let (directory, path) = session.make("build").unwrap();
            let write = session.begin(&directory, "index.html", 8).unwrap();
            let temporary =
                opened_path(session.pending.get_mut(&write).unwrap().file.file_mut()).unwrap();
            assert!(fs::rename(&temporary, path.join("replacement")).is_err());
            assert!(fs::remove_file(&temporary).is_err());
            session.append(&write, b"APPROVED").unwrap();
            session.commit(&write).unwrap();
            assert_eq!(
                fs::read_to_string(path.join("index.html")).unwrap(),
                "APPROVED"
            );
            let write = session.begin(&directory, "main.js", 10).unwrap();
            let temporary =
                opened_path(session.pending.get_mut(&write).unwrap().file.file_mut()).unwrap();
            session.abort(&write);
            assert!(!temporary.exists());
        }
        #[test]
        fn publish_lock_is_deleted_by_its_handle_and_cannot_be_replaced() {
            let (temp, session) = setup();
            let path = temp.path().join(".frontend-publish.lock");
            let lock = open_component_with_options(
                session.root(),
                OsStr::new(".frontend-publish.lock"),
                ATTRIBUTES | DELETE,
                0,
                FILE_CREATE,
                FILE_DELETE_ON_CLOSE,
            )
            .unwrap();
            assert!(fs::rename(&path, temp.path().join("old-lock")).is_err());
            assert!(fs::remove_file(&path).is_err());
            drop(lock);
            fs::write(&path, "NEW OWNER").unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "NEW OWNER");
        }
        #[test]
        fn publish_and_replace_work_with_pinned_parent_handles() {
            let (temp, mut session) = setup();
            for contents in [b"FIRST".as_slice(), b"SECOND".as_slice()] {
                let (id, path) = session.make("build").unwrap();
                assert!(fs::rename(&path, temp.path().join("moved-stage")).is_err());
                let write = session.begin(&id, "index.html", contents.len()).unwrap();
                session.append(&write, contents).unwrap();
                session.commit(&write).unwrap();
                session.publish(&id).unwrap();
                assert_eq!(
                    fs::read(temp.path().join("dist/index.html")).unwrap(),
                    contents
                );
            }
        }
        #[test]
        fn commit_requires_the_exact_declared_length_and_never_replaces_a_name() {
            let (_temp, mut session) = setup();
            let (id, path) = session.make("build").unwrap();
            let write = session.begin(&id, "main.js", 4).unwrap();
            session.append(&write, b"abc").unwrap();
            assert_eq!(
                session.commit(&write).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
            let target = path.join("main.js");
            assert!(!target.exists());
            assert!(session.begin(&id, "main.js", OUTPUT_BYTES + 1).is_err());
            let write = session.begin(&id, "main.js", 4).unwrap();
            session.append(&write, b"abcd").unwrap();
            fs::write(&target, b"existing").unwrap();
            assert!(session.commit(&write).is_err());
            assert_eq!(fs::read(target).unwrap(), b"existing");
        }
    }
}
