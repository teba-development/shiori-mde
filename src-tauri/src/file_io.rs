use std::{
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(windows)]
use crate::windows_fs;

pub type AppResult<T> = Result<T, String>;

pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DOCUMENT_LINES: usize = 200_000;
pub const MAX_LINK_BYTES: usize = 8 * 1024;

pub fn file_error(action: &str, path: &Path, error: impl std::fmt::Display) -> String {
    format!("{action}：{}\n{error}", path.display())
}

pub fn valid_file_component(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with([' ', '.'])
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return false;
    }

    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let device_number = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !matches!(
        device_number,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
    )
}

pub(crate) fn validate_path(path: &Path) -> AppResult<()> {
    #[cfg(windows)]
    use std::path::Prefix;

    for component in path.components() {
        match component {
            Component::Normal(part) if !part.to_str().is_some_and(valid_file_component) => {
                return Err("ファイル名またはフォルダー名が無効です。".into());
            }
            #[cfg(windows)]
            Component::Prefix(prefix)
                if matches!(prefix.kind(), Prefix::DeviceNS(_) | Prefix::Verbatim(_)) =>
            {
                return Err("通常のファイルパスを指定してください。".into())
            }
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                if let Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) = prefix.kind() {
                    if share.eq_ignore_ascii_case("pipe") || share.eq_ignore_ascii_case("mailslot")
                    {
                        return Err("通常のファイルパスを指定してください。".into());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn canonical_path(path: &Path) -> AppResult<PathBuf> {
    validate_path(path)?;
    let path = dunce::canonicalize(path)
        .map_err(|error| file_error("パスを確認できませんでした", path, error))?;
    validate_path(&path)?;
    Ok(path)
}

#[derive(Clone)]
pub struct OpenedFolder {
    path: PathBuf,
    #[cfg(windows)]
    handle: Arc<File>,
}

impl OpenedFolder {
    pub fn open_selected(path: &Path) -> AppResult<Self> {
        #[cfg(windows)]
        {
            validate_path(path)?;
            let handle = windows_fs::open_directory(path)
                .map_err(|error| file_error("フォルダーを開けませんでした", path, error))?;
            let path = resolved_file_path(path, &handle)?;
            Ok(Self {
                path,
                handle: Arc::new(handle),
            })
        }
        #[cfg(not(windows))]
        {
            let path = canonical_path(path)?;
            if !path.is_dir() {
                return Err("フォルダーを指定してください。".into());
            }
            Ok(Self { path })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open_relative(&self, relative: &Path) -> io::Result<File> {
        #[cfg(windows)]
        {
            windows_fs::open_relative(&self.handle, &self.path, relative)
        }
        #[cfg(not(windows))]
        {
            let _ = relative;
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    pub fn contains_selected_file(&self, path: &Path, selected: &File) -> bool {
        #[cfg(windows)]
        {
            path.strip_prefix(self.path())
                .ok()
                .and_then(|relative| self.open_relative(relative).ok())
                .is_some_and(|file| windows_fs::same_file(&file, selected).unwrap_or(false))
        }
        #[cfg(not(windows))]
        {
            let _ = (path, selected);
            false
        }
    }
}

#[derive(Clone)]
pub struct SaveDestination {
    pub path: PathBuf,
    pub folder: OpenedFolder,
    #[cfg(windows)]
    expected: Option<Arc<windows_fs::FileVersion>>,
    #[cfg(not(windows))]
    expected: Option<Arc<File>>,
}

impl SaveDestination {
    pub fn exists(&self) -> bool {
        self.expected.is_some()
    }
}

pub fn save_path(path: &Path) -> AppResult<SaveDestination> {
    validate_path(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("保存先のフォルダーを指定してください。")?;
    let name = path
        .file_name()
        .ok_or("保存先のファイル名を指定してください。")?;
    let folder = OpenedFolder::open_selected(parent)?;
    let path = folder.path().join(name);
    #[cfg(windows)]
    let existing = windows_fs::capture_destination(&folder.handle, folder.path(), name);
    #[cfg(not(windows))]
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => File::open(&path).map(Some),
        Ok(_) => Err(io::ErrorKind::InvalidInput.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    let expected = existing
        .map_err(|error| file_error("保存先を確認できませんでした", &path, error))?
        .map(Arc::new);
    Ok(SaveDestination {
        path,
        folder,
        expected,
    })
}

pub fn open_selected_file(path: &Path) -> AppResult<(PathBuf, File)> {
    validate_path(path)?;
    #[cfg(windows)]
    let opened = windows_fs::open_selected_file(path);
    #[cfg(not(windows))]
    let opened = File::open(path);
    let file = opened.map_err(|error| file_error("ファイルを開けませんでした", path, error))?;
    let resolved = resolved_file_path(path, &file)?;
    Ok((resolved, file))
}

fn resolved_file_path(path: &Path, _file: &File) -> AppResult<PathBuf> {
    #[cfg(windows)]
    {
        let resolved = windows_fs::opened_path(_file)
            .map_err(|error| file_error("パスを確認できませんでした", path, error))?;
        validate_path(&resolved)?;
        Ok(resolved)
    }
    #[cfg(not(windows))]
    canonical_path(path)
}

pub fn open_document_file(path: &Path) -> AppResult<(SaveDestination, Vec<u8>)> {
    let (path, file) = open_selected_file(path)?;
    let parent = path.parent().ok_or("文書フォルダーが見つかりません。")?;
    let folder = OpenedFolder::open_selected(parent)?;
    #[cfg(windows)]
    let (file, observed) = {
        let name = path.file_name().ok_or("文書ファイルを指定してください。")?;
        // Associate the image scope only if its directory still contains the opened document.
        let read_error = |error| file_error("文書を確認できませんでした", &path, error);
        let reader =
            windows_fs::verify_child(&folder.handle, parent, name, &file).map_err(read_error)?;
        let observed =
            windows_fs::hold_identity(&folder.handle, name, &reader).map_err(read_error)?;
        (reader, observed)
    };
    #[cfg(not(windows))]
    let observed = file
        .try_clone()
        .map_err(|error| file_error("文書を確認できませんでした", &path, error))?;
    let bytes = read_bounded(&file, &path, MAX_DOCUMENT_BYTES, "Markdownファイル")?;
    #[cfg(windows)]
    let observed = windows_fs::FileVersion::from_bytes(observed, &bytes);
    Ok((
        SaveDestination {
            path,
            folder,
            expected: Some(Arc::new(observed)),
        },
        bytes,
    ))
}

pub fn read_bounded(file: &File, path: &Path, limit: usize, kind: &str) -> AppResult<Vec<u8>> {
    let read_error = |error| file_error("ファイルを読み込めませんでした", path, error);
    let metadata = file.metadata().map_err(read_error)?;
    if !metadata.is_file() {
        return Err("通常のファイルを指定してください。".into());
    }
    let too_large = || format!("{kind}は{} MiBまでです。", limit / (1024 * 1024));
    if metadata.len() > limit as u64 {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

pub fn validate_markdown(text: &str, encoded_bytes: usize) -> AppResult<()> {
    if encoded_bytes > MAX_DOCUMENT_BYTES {
        return Err("Markdownファイルは64 MiBまでです。".into());
    }
    if text.contains('\0') {
        return Err("NUL文字を含むファイルはMarkdownとして読み込めません。".into());
    }
    let mut lines = 1;
    let mut after_cr = false;
    for byte in text.bytes() {
        if byte == b'\r' || (byte == b'\n' && !after_cr) {
            lines += 1;
            if lines > MAX_DOCUMENT_LINES {
                return Err("Markdownファイルは20万行までです。".into());
            }
        }
        after_cr = byte == b'\r';
    }
    Ok(())
}

pub fn write_atomic(
    destination: &SaveDestination,
    recovery_dir: &Path,
    write: impl FnOnce(&mut BufWriter<&mut File>) -> io::Result<()>,
) -> AppResult<SaveDestination> {
    let SaveDestination {
        folder,
        path,
        expected,
    } = destination;
    let result = (|| {
        if path.parent() != Some(folder.path()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        #[cfg(windows)]
        {
            let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
            let (mut temporary, original) = windows_fs::StagedFile::begin_document(
                &folder.handle,
                folder.path(),
                name,
                expected.as_deref(),
            )?;
            write_and_sync(temporary.file_mut(), write)?;
            temporary.persist_document(&folder.handle, folder.path(), name, original, recovery_dir)
        }
        #[cfg(not(windows))]
        {
            let _ = recovery_dir;
            if expected.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "既存ファイルの上書きにはWindows版を使用してください。",
                ));
            }
            let mut temporary = tempfile::NamedTempFile::new_in(folder.path())?;
            write_and_sync(temporary.as_file_mut(), write)?;
            Ok(temporary.persist_noclobber(path)?)
        }
    })();
    let saved = result.map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            return file_error(
                "保存先に同名のファイルが作成されました",
                path,
                "保存先を選び直してください。",
            );
        }
        file_error("ファイルを保存できませんでした", path, error)
    })?;
    Ok(SaveDestination {
        path: path.clone(),
        folder: folder.clone(),
        expected: Some(Arc::new(saved)),
    })
}

fn write_and_sync(
    file: &mut File,
    write: impl FnOnce(&mut BufWriter<&mut File>) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffered = BufWriter::new(file);
    write(&mut buffered)?;
    buffered.flush()?;
    buffered.get_ref().sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn file_read_enforces_limit_and_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("文書.md");
        fs::write(&path, b"test").unwrap();
        let read = |limit| {
            let (resolved, file) = open_selected_file(&path)?;
            read_bounded(&file, &resolved, limit, "文書")
        };
        assert_eq!(read(4).unwrap(), b"test");
        assert!(read(3).is_err());
        assert!(open_document_file(dir.path()).is_err());
    }

    #[test]
    fn bounded_read_uses_the_opened_file_after_the_name_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("文書.md");
        fs::write(&path, b"original").unwrap();
        let (resolved, file) = open_selected_file(&path).unwrap();
        fs::rename(&path, dir.path().join("previous.md")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(
            read_bounded(&file, &resolved, 16, "文書").unwrap(),
            b"original"
        );
    }

    #[test]
    fn windows_component_validation_preserves_normal_names() {
        assert!(valid_file_component("画像 100%.png"));
        for name in [
            "CON.txt",
            "LPT¹.svg",
            "image.png.",
            "name:stream",
            "image\0.png",
        ] {
            assert!(!valid_file_component(name), "{name:?}");
        }
    }

    #[test]
    fn failed_atomic_write_preserves_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("文書.md");
        fs::write(&path, b"original").unwrap();
        let destination = save_path(&path).unwrap();
        assert!(
            write_atomic(&destination, &dir.path().join("recovery"), |writer| {
                writer.write_all(b"partial")?;
                Err(io::ErrorKind::WriteZero.into())
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_new_destination_does_not_replace_a_file_created_during_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("新規.md");
        let destination = save_path(&path).unwrap();
        assert!(destination.expected.is_none());
        let error = write_atomic(&destination, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"editor contents")?;
            fs::write(&path, b"other contents")
        })
        .err()
        .expect("a new destination must not replace an existing file");
        assert!(error.contains("同名のファイル"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"other contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn writes_a_file_with_a_255_unit_windows_name() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!("{}.md", "📖".repeat(126)));
        let destination = save_path(&path).unwrap();
        write_atomic(&destination, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"limit")
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"limit");
    }

    #[cfg(windows)]
    #[test]
    fn windows_save_rejects_a_replaced_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("selected");
        let moved = tmp.path().join("previous");
        fs::create_dir(&original).unwrap();
        let path = original.join("sample.md");
        // An existing file's identity handle can prevent its folder from
        // moving. A new destination must still reject a changed folder.
        let destination = save_path(&path).unwrap();
        fs::rename(&original, &moved).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(
            write_atomic(&destination, &tmp.path().join("recovery"), |writer| writer
                .write_all(b"updated"))
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert!(!moved.join("sample.md").exists());
    }

    #[cfg(windows)]
    #[test]
    fn existing_destination_refuses_to_overwrite_a_different_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("文書.md");
        let previous = tmp.path().join("以前の文書.md");
        fs::write(&path, b"approved").unwrap();
        let destination = save_path(&path).unwrap();
        fs::rename(&path, &previous).unwrap();
        fs::write(&path, b"other file").unwrap();
        let error = write_atomic(&destination, &tmp.path().join("recovery"), |writer| {
            writer.write_all(b"editor")
        })
        .err()
        .expect("the selected file identity changed");
        assert!(error.contains("ファイルが切り替わりました"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"other file");
        assert_eq!(fs::read(&previous).unwrap(), b"approved");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn subsequent_saves_use_the_new_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("文書.md");
        fs::write(&path, b"original").unwrap();
        let recovery = tmp.path().join("recovery");
        let (original, bytes) = open_document_file(&path).unwrap();
        assert_eq!(bytes, b"original");
        let first = write_atomic(&original, &recovery, |writer| {
            writer.write_all(b"first save")
        })
        .unwrap();
        assert!(write_atomic(&original, &recovery, |writer| writer
            .write_all(b"stale save"))
        .is_err());
        drop(original);
        let second =
            write_atomic(&first, &recovery, |writer| writer.write_all(b"second save")).unwrap();
        drop(first);
        assert_eq!(fs::read(&second.path).unwrap(), b"second save");
        assert_eq!(fs::read_dir(&recovery).unwrap().count(), 0);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn an_existing_large_export_can_be_replaced_by_a_small_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output.html");
        File::create(&path)
            .unwrap()
            .set_len(MAX_DOCUMENT_BYTES as u64 + 1)
            .unwrap();
        let selected = save_path(&path).unwrap();
        assert!(selected.exists());
        write_atomic(&selected, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"<!doctype html><p>Updated</p>")
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"<!doctype html><p>Updated</p>");
    }

    #[cfg(windows)]
    #[test]
    fn relative_images_require_a_single_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = OpenedFolder::open_selected(tmp.path()).unwrap();
        let path = tmp.path().join("sample.svg");
        fs::write(&path, b"<svg/>").unwrap();
        let (path, selected) = open_selected_file(&path).unwrap();
        assert!(folder.contains_selected_file(&path, &selected));

        fs::hard_link(&path, tmp.path().join("alias.svg")).unwrap();
        assert!(!folder.contains_selected_file(&path, &selected));
        assert!(folder.open_relative(Path::new("alias.svg")).is_err());
        assert_eq!(
            read_bounded(&selected, &path, 16, "画像").unwrap(),
            b"<svg/>"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_validation_requires_a_file_share() {
        for path in [r"\\server\pipe\item", r"\\?\UNC\server\MailSlot\item"] {
            assert!(validate_path(Path::new(path)).is_err());
        }
        assert!(validate_path(Path::new(r"\\server\documents\文書.md")).is_ok());
    }
}
