use std::{collections::HashSet, fs, os::windows::fs::OpenOptionsExt};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::ERROR_SHARING_VIOLATION, Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
};

use super::*;

const MAX_RECORD_BYTES: u64 = 256 * 1024;
const RECORD_VERSION: u32 = 1;
const ARTIFACT_PREFIX: &str = ".shiori-";

fn invalid_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "保存の復元記録が無効です。")
}

fn artifact_key<'a>(name: &'a str, extension: &str) -> Option<&'a str> {
    let key = name
        .strip_prefix(ARTIFACT_PREFIX)?
        .strip_suffix(extension)?;
    (key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(key)
}

fn open_store(path: &Path) -> io::Result<File> {
    let directory = OpenOptions::new()
        .access_mode(ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    require_disk(&directory)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_record());
    }
    Ok(directory)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SaveRecord {
    version: u32,
    directory: PathBuf,
    directory_id: FileId,
    name: String,
    original_id: FileId,
    original_digest: [u8; 32],
    temporary: String,
    staged_id: FileId,
    staged_digest: [u8; 32],
    pub(super) backup: String,
}

#[derive(Debug)]
pub(super) struct Journal(File);

#[derive(Debug)]
struct NoticeReceipt {
    journal: Journal,
    acknowledged_name: OsString,
}

#[derive(Debug, Default)]
struct RecoveryWarnings {
    messages: Vec<String>,
    receipts: Vec<NoticeReceipt>,
}

impl std::fmt::Display for RecoveryWarnings {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.messages.join("\n\n"))
    }
}

impl std::error::Error for RecoveryWarnings {}

impl RecoveryWarnings {
    fn add(&mut self, context: &Path, error: io::Error) {
        self.messages
            .push(format!("{}\n{error}", context.display()));
        if let Some(inner) = error.into_inner() {
            if let Ok(mut warnings) = inner.downcast::<Self>() {
                self.receipts.append(&mut warnings.receipts);
            }
        }
    }
}

fn pending_notice(message: String, journal: Journal, acknowledged_name: String) -> io::Error {
    io::Error::other(RecoveryWarnings {
        messages: vec![message],
        receipts: vec![NoticeReceipt {
            journal,
            acknowledged_name: OsString::from(acknowledged_name),
        }],
    })
}

// Each receipt owns the same exclusive handle used to read the notice.
// Another startup cannot acknowledge a warning that this window has not shown.
pub fn acknowledge_recovery(error: &io::Error) -> io::Result<()> {
    let Some(warnings) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<RecoveryWarnings>())
    else {
        return Ok(());
    };
    for receipt in &warnings.receipts {
        rename_at(&receipt.journal.0, None, &receipt.acknowledged_name)?;
    }
    Ok(())
}

impl Journal {
    pub(super) fn remove(self) {
        if let Err(error) = delete_on_close(&self.0) {
            eprintln!("Could not remove save recovery record: {error}");
        }
    }
}

impl SaveRecord {
    pub(super) fn new(
        directory: &File,
        path: &Path,
        name: &OsStr,
        original: &OriginalFile,
        staged: &StagedFile,
        saved: &FileVersion,
    ) -> io::Result<Self> {
        let key = artifact_key(&staged.name, ".tmp").ok_or_else(invalid_record)?;
        Ok(Self {
            version: RECORD_VERSION,
            directory: path.to_path_buf(),
            directory_id: file_id(directory)?,
            name: name.to_str().ok_or_else(invalid_record)?.to_owned(),
            original_id: file_id(&original.file)?,
            original_digest: original.digest,
            temporary: staged.name.clone(),
            staged_id: file_id(&saved.identity)?,
            staged_digest: saved.digest,
            backup: format!("{ARTIFACT_PREFIX}{key}.bak"),
        })
    }

    pub(super) fn start(&self, store: &Path) -> io::Result<Journal> {
        let key = artifact_key(&self.temporary, ".tmp").ok_or_else(invalid_record)?;
        self.validate(key)?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid_record());
        }
        fs::create_dir_all(store)?;
        let directory = open_store(store)?;
        let pending = format!("{ARTIFACT_PREFIX}{key}.pending");
        // Exclusive sharing blocks recovery until this save finishes or exits.
        let mut journal = Journal(open_component(
            &directory,
            OsStr::new(&pending),
            FILE_WRITE_DATA | ATTRIBUTES | DELETE,
            0,
            FILE_CREATE,
        )?);
        let publish = journal
            .0
            .write_all(&bytes)
            .and_then(|()| journal.0.sync_all())
            .and_then(|()| {
                rename_at(
                    &journal.0,
                    None,
                    OsStr::new(&format!("{ARTIFACT_PREFIX}{key}.json")),
                )
            });
        if let Err(error) = publish {
            journal.remove();
            return Err(error);
        }
        Ok(journal)
    }

    fn validate(&self, key: &str) -> io::Result<()> {
        if self.version != RECORD_VERSION
            || !self.directory.is_absolute()
            || self
                .directory
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || crate::file_io::validate_path(&self.directory).is_err()
            || !valid_file_component(&self.name)
            || artifact_key(&self.temporary, ".tmp") != Some(key)
            || artifact_key(&self.backup, ".bak") != Some(key)
            || self.name.eq_ignore_ascii_case(&self.temporary)
            || self.name.eq_ignore_ascii_case(&self.backup)
        {
            return Err(invalid_record());
        }
        Ok(())
    }

    fn open_document_directory(&self) -> io::Result<File> {
        let directory = open_directory(&self.directory)?;
        check_directory_path(&directory, &self.directory)?;
        if file_id(&directory)? != self.directory_id {
            return Err(changed_destination());
        }
        Ok(directory)
    }

    fn recover_in(&self, directory: &File, id_may_change: bool) -> io::Result<()> {
        let Some(backup) = self.open_artifact(
            directory,
            &self.backup,
            &self.original_id,
            self.original_digest,
            id_may_change,
        )?
        else {
            self.open_verified_document(directory, id_may_change)?
                .ok_or_else(changed_destination)?;
            return Ok(());
        };
        match self.open_document(directory, 0) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                rename_at(&backup, None, OsStr::new(&self.name))
            }
            Ok(current)
                if matches_artifact(
                    &current,
                    &self.staged_id,
                    self.staged_digest,
                    id_may_change,
                )? =>
            {
                Ok(())
            }
            Ok(_) => Err(changed_destination()),
            Err(error) => Err(error),
        }
    }

    fn open_document(&self, directory: &File, extra_access: u32) -> io::Result<File> {
        open_save_target(
            directory,
            OsStr::new(&self.name),
            FILE_READ_DATA | ATTRIBUTES | extra_access,
            FILE_SHARE_READ,
        )
    }

    fn open_verified_document(
        &self,
        directory: &File,
        id_may_change: bool,
    ) -> io::Result<Option<File>> {
        let file = match self.open_document(directory, 0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if matches_artifact(
            &file,
            &self.original_id,
            self.original_digest,
            id_may_change,
        )? || matches_artifact(&file, &self.staged_id, self.staged_digest, id_may_change)?
        {
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    // This phase never moves a document. If a backup can no longer be
    // identified by its exact ID, preserve it even on a rename-sensitive FS.
    fn clean_up(&self, directory: &File) -> io::Result<Vec<PathBuf>> {
        let volume = Volume::read(directory).ok();
        let mut retained = Vec::new();
        let temporary = match open_save_target(
            directory,
            OsStr::new(&self.temporary),
            FILE_READ_DATA | ATTRIBUTES | DELETE,
            FILE_SHARE_READ,
        ) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                retained.push(self.directory.join(&self.temporary));
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(temporary) = temporary {
            let id_may_change = volume.as_ref().is_some_and(Volume::id_may_change_on_rename);
            if matches_artifact(&temporary, &self.staged_id, self.staged_digest, false)? {
                if let Some(_current) = self.open_verified_document(directory, id_may_change)? {
                    delete_on_close(&temporary)?;
                    drop(temporary);
                } else {
                    retained.push(self.directory.join(&self.temporary));
                }
            } else {
                retained.push(self.directory.join(&self.temporary));
            }
        }
        let has_acl = volume.as_ref().is_none_or(Volume::has_acl);
        let access = if has_acl { READ_CONTROL } else { 0 };
        let backup = match open_save_target(
            directory,
            OsStr::new(&self.backup),
            FILE_READ_DATA | ATTRIBUTES | DELETE | access,
            FILE_SHARE_READ,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(retained),
            Err(error) => return Err(error),
        };
        let backup_path = self.directory.join(&self.backup);
        if matches_artifact(&backup, &self.original_id, self.original_digest, false)? {
            match self.open_document(directory, access) {
                Ok(current)
                    if matches_artifact(&current, &self.staged_id, self.staged_digest, false)? =>
                {
                    // Keep the per-stream read handles until after scheduling deletion.
                    let streams = volume
                        .as_ref()
                        .is_none_or(Volume::has_named_streams)
                        .then(|| -> io::Result<_> {
                            Ok((
                                named_streams::NamedStreams::open(
                                    directory,
                                    OsStr::new(&self.backup),
                                    &backup,
                                )?,
                                named_streams::NamedStreams::open(
                                    directory,
                                    OsStr::new(&self.name),
                                    &current,
                                )?,
                            ))
                        })
                        .transpose()?;
                    if let Some((original, saved)) = &streams {
                        if !original.same_data(saved)? {
                            retained.push(backup_path);
                            return Ok(retained);
                        }
                        original.verify(&backup)?;
                        saved.verify(&current)?;
                    }
                    if has_acl && !file_security::Dacl::read(&backup)?.matches(&current)? {
                        retained.push(backup_path);
                        return Ok(retained);
                    }
                    delete_on_close(&backup)?;
                    drop(backup);
                    drop(streams);
                    return Ok(retained);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        retained.push(backup_path);
        Ok(retained)
    }

    fn open_artifact(
        &self,
        directory: &File,
        name: &str,
        id: &FileId,
        digest: [u8; 32],
        allow_renamed_id: bool,
    ) -> io::Result<Option<File>> {
        match open_save_target(
            directory,
            OsStr::new(name),
            FILE_READ_DATA | ATTRIBUTES | DELETE,
            FILE_SHARE_READ,
        ) {
            Ok(file) => {
                if matches_artifact(&file, id, digest, allow_renamed_id)? {
                    Ok(Some(file))
                } else {
                    Err(changed_destination())
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub(super) fn matches_artifact(
    file: &File,
    id: &FileId,
    digest: [u8; 32],
    allow_renamed_id: bool,
) -> io::Result<bool> {
    let actual = file_id(file)?;
    // On FAT a rename can change the directory-entry ID. The journal pins
    // the directory and volume; the fallback also requires an exact byte match.
    let identified = &actual == id || (allow_renamed_id && actual.same_volume(id));
    Ok(identified && content_digest(file)? == digest)
}

pub fn recover_saves(store: &Path) -> io::Result<()> {
    let directory = match open_store(store) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let path = opened_path(&directory)?;
    let mut warnings = RecoveryWarnings::default();
    let mut processed = HashSet::new();
    for entry in fs::read_dir(&path)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings
                    .messages
                    .push(format!("{}\n{error}", path.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            continue;
        };
        let key = [".json", ".cleanup", ".notice", ".pending", ".retained"]
            .iter()
            .find_map(|extension| artifact_key(text, extension));
        if let Some(key) = key {
            if !processed.insert(key.to_owned()) {
                continue;
            }
        }
        let result = if let Some(key) = artifact_key(text, ".json") {
            recover_record(&directory, &name, key, false)
        } else if let Some(key) = artifact_key(text, ".cleanup") {
            recover_record(&directory, &name, key, true)
        } else if let Some(key) = artifact_key(text, ".notice") {
            report_notice(&directory, &name, key)
        } else if let Some(key) = artifact_key(text, ".retained") {
            // Older versions did not record whether the dialog was confirmed.
            report_notice(&directory, &name, key)
        } else if let Some(prefix) = text
            .strip_suffix(".invalid-notice")
            .or_else(|| text.strip_suffix(".invalid"))
        {
            let valid = prefix
                .strip_prefix(ARTIFACT_PREFIX)
                .and_then(|suffix| suffix.split_once('-'))
                .is_some_and(|(first, second)| {
                    [first, second].iter().all(|key| {
                        key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                });
            if valid {
                let original_key = &prefix[ARTIFACT_PREFIX.len()..ARTIFACT_PREFIX.len() + 16];
                if !processed.insert(original_key.to_owned()) {
                    continue;
                }
                report_invalid_notice(&directory, &name, prefix)
            } else {
                continue;
            }
        } else if artifact_key(text, ".pending").is_some() {
            // A pending journal cannot authorize moving the original.
            open_journal(&directory, &name).map(|journal| {
                if let Some(journal) = journal {
                    journal.remove();
                }
            })
        } else {
            continue;
        };
        if let Err(error) = result {
            warnings.add(&path.join(name), error);
        }
    }
    if warnings.messages.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(warnings))
    }
}

fn open_journal(directory: &File, name: &OsStr) -> io::Result<Option<Journal>> {
    let journal = match open_save_target(directory, name, FILE_READ_DATA | ATTRIBUTES | DELETE, 0) {
        Ok(file) => Journal(file),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    if basic_file_info(&journal.0)?.nNumberOfLinks != 1 {
        return Err(invalid_record());
    }
    Ok(Some(journal))
}

fn read_record(journal: &Journal, key: &str) -> io::Result<SaveRecord> {
    let mut bytes = Vec::new();
    (&journal.0)
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid_record());
    }
    let record: SaveRecord = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    record.validate(key)?;
    Ok(record)
}

fn recover_record(directory: &File, name: &OsStr, key: &str, cleanup_only: bool) -> io::Result<()> {
    let Some(journal) = open_journal(directory, name)? else {
        return Ok(());
    };
    let record = match read_record(&journal, key) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            // An older build could leave a partial .json. Retain it for
            // inspection and report it once, without retrying it at startup.
            let suffix = getrandom::u64().map_err(io::Error::other)?;
            let prefix = format!("{ARTIFACT_PREFIX}{key}-{suffix:016x}");
            let quarantined = format!("{prefix}.invalid-notice");
            rename_at(&journal.0, None, OsStr::new(&quarantined))?;
            return Err(pending_notice(
                format!(
                    "無効な復元記録を退避しました：{}\n{error}",
                    opened_path(directory)?.join(quarantined).display()
                ),
                journal,
                format!("{prefix}.invalid-acknowledged"),
            ));
        }
        Err(error) => return Err(error),
    };
    let document_dir = record.open_document_directory().map_err(|error| {
        if cleanup_only {
            cleanup_error(error)
        } else {
            restore_failed(&record, error)
        }
    })?;
    if !cleanup_only {
        let id_may_change =
            Volume::read(&document_dir).is_ok_and(|volume| volume.id_may_change_on_rename());
        record
            .recover_in(&document_dir, id_may_change)
            .map_err(|error| restore_failed(&record, error))?;
        mark_cleanup(&journal, &record)?;
    }
    finish_cleanup(&record, &document_dir, journal, key)
}

pub(super) fn mark_cleanup(journal: &Journal, record: &SaveRecord) -> io::Result<()> {
    let key = artifact_key(&record.temporary, ".tmp").ok_or_else(invalid_record)?;
    rename_at(
        &journal.0,
        None,
        OsStr::new(&format!("{ARTIFACT_PREFIX}{key}.cleanup")),
    )
    .map_err(cleanup_error)
}

fn finish_cleanup(
    record: &SaveRecord,
    directory: &File,
    journal: Journal,
    key: &str,
) -> io::Result<()> {
    let retained = record.clean_up(directory).map_err(cleanup_error)?;
    if !retained.is_empty() {
        // Stop retrying a completed recovery while keeping the backup and
        // its recovery information available to the user.
        rename_at(
            &journal.0,
            None,
            OsStr::new(&format!("{ARTIFACT_PREFIX}{key}.notice")),
        )
        .map_err(cleanup_error)?;
        return Err(pending_notice(
            format!(
                "文書本体の処理は完了しました。次の復旧用ファイルを保持しています。\n{}",
                retained
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            journal,
            format!("{ARTIFACT_PREFIX}{key}.acknowledged"),
        ));
    }
    delete_on_close(&journal.0).map_err(cleanup_error)
}

fn report_notice(directory: &File, name: &OsStr, key: &str) -> io::Result<()> {
    let Some(journal) = open_journal(directory, name)? else {
        return Ok(());
    };
    let record = read_record(&journal, key)?;
    let message = format!(
        "復旧用ファイルの保持記録があります。文書と次の場所を確認してください。\n{}\n{}",
        record.directory.join(&record.temporary).display(),
        record.directory.join(&record.backup).display(),
    );
    Err(pending_notice(
        message,
        journal,
        format!("{ARTIFACT_PREFIX}{key}.acknowledged"),
    ))
}

fn report_invalid_notice(directory: &File, name: &OsStr, prefix: &str) -> io::Result<()> {
    let Some(journal) = open_journal(directory, name)? else {
        return Ok(());
    };
    Err(pending_notice(
        format!(
            "無効な復元記録を退避しています：{}",
            opened_path(directory)?.join(name).display(),
        ),
        journal,
        format!("{prefix}.invalid-acknowledged"),
    ))
}

fn restore_failed(record: &SaveRecord, error: io::Error) -> io::Error {
    io::Error::other(format!(
        "文書本体を復元または確認できませんでした。復旧用ファイル：\n{}\n{}\n{error}",
        record.directory.join(&record.backup).display(),
        record.directory.join(&record.temporary).display(),
    ))
}

fn cleanup_error(error: io::Error) -> io::Error {
    io::Error::other(format!(
        "文書本体の処理は完了しました。後片付けは次回起動時に再試行します。\n{error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InterruptedSave {
        _directory: tempfile::TempDir,
        original: PathBuf,
        backup: PathBuf,
        temporary: PathBuf,
        store: PathBuf,
    }

    impl InterruptedSave {
        fn new(published: bool) -> Self {
            Self::with_stream(published, false)
        }

        fn with_stream(published: bool, stream: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let folder = open_directory(directory.path()).unwrap();
            let path = opened_path(&folder).unwrap();
            let name = OsStr::new("文書.md");
            let original = path.join(name);
            fs::write(&original, b"original").unwrap();
            if stream {
                fs::write(path.join("文書.md:note"), b"first").unwrap();
            }
            let expected = capture_destination(&folder, &path, name).unwrap().unwrap();
            let (mut staged, original_file) =
                StagedFile::begin_document(&folder, &path, name, Some(&expected)).unwrap();
            let original_file = original_file.unwrap();
            staged.file_mut().write_all(b"edited").unwrap();
            let identity = hold_identity(&folder, OsStr::new(&staged.name), &staged.file).unwrap();
            let saved = FileVersion::from_bytes(identity, b"edited");
            let record =
                SaveRecord::new(&folder, &path, name, &original_file, &staged, &saved).unwrap();
            let store = path.join("recovery");
            let journal = record.start(&store).unwrap();
            rename_at(&original_file.file, None, OsStr::new(&record.backup)).unwrap();
            // Another instance must leave a save with an open journal alone.
            recover_saves(&store).unwrap();
            assert!(!original.exists());
            if published {
                staged.publish(name).unwrap();
            } else {
                // Simulate process exit, which closes the handle without running the stage cleanup.
                staged.persisted = true;
            }
            drop(journal);
            Self {
                _directory: directory,
                original,
                backup: path.join(record.backup),
                temporary: path.join(record.temporary),
                store,
            }
        }

        fn assert_recovered(&self, contents: &[u8]) {
            assert_eq!(fs::read(&self.original).unwrap(), contents);
            assert!(!self.backup.exists());
            assert!(!self.temporary.exists());
            assert_eq!(fs::read_dir(&self.store).unwrap().count(), 0);
        }
    }

    #[test]
    fn recovery_restores_the_original_name_after_an_interrupted_save() {
        let save = InterruptedSave::new(false);
        recover_saves(&save.store).unwrap();
        save.assert_recovered(b"original");
        recover_saves(&save.store).unwrap();
    }

    #[test]
    fn recovery_keeps_the_published_version_after_an_interruption() {
        let save = InterruptedSave::new(true);
        recover_saves(&save.store).unwrap();
        save.assert_recovered(b"edited");
    }

    #[test]
    fn recovery_preserves_the_backup_if_the_original_name_is_taken() {
        let save = InterruptedSave::new(false);
        fs::write(&save.original, b"other file").unwrap();
        let error = recover_saves(&save.store).unwrap_err();
        assert!(error.to_string().contains(&*save.backup.to_string_lossy()));
        assert_eq!(fs::read(&save.original).unwrap(), b"other file");
        assert_eq!(fs::read(&save.backup).unwrap(), b"original");
        fs::rename(&save.original, save.original.with_extension("other")).unwrap();
        recover_saves(&save.store).unwrap();
        save.assert_recovered(b"original");
    }

    #[test]
    fn recovery_keeps_the_staged_document_until_the_current_document_is_verified() {
        let save = InterruptedSave::new(false);
        let retained = save.backup.with_extension("retained");
        fs::rename(&save.backup, &retained).unwrap();
        assert!(recover_saves(&save.store).is_err());
        assert_eq!(fs::read(&save.temporary).unwrap(), b"edited");
        fs::write(&save.original, b"different").unwrap();
        assert!(recover_saves(&save.store).is_err());
        assert_eq!(fs::read(&save.temporary).unwrap(), b"edited");
        assert_eq!(fs::read(&save.original).unwrap(), b"different");
        fs::remove_file(&save.original).unwrap();
        fs::rename(retained, &save.backup).unwrap();
        recover_saves(&save.store).unwrap();
        save.assert_recovered(b"original");
    }

    #[test]
    fn recovery_keeps_the_backup_when_the_published_named_stream_changed() {
        let save = InterruptedSave::with_stream(true, true);
        let stream_path = |path: &Path| {
            let mut stream = path.as_os_str().to_os_string();
            stream.push(":note");
            PathBuf::from(stream)
        };
        // Updating the alternate stream leaves the file's main data unchanged.
        fs::write(stream_path(&save.original), b"later").unwrap();
        let notice = recover_saves(&save.store).unwrap_err();
        assert!(notice.to_string().contains("復旧用ファイルを保持"));
        assert_eq!(fs::read(&save.original).unwrap(), b"edited");
        assert_eq!(fs::read(stream_path(&save.original)).unwrap(), b"later");
        assert_eq!(fs::read(stream_path(&save.backup)).unwrap(), b"first");
        acknowledge_recovery(&notice).unwrap();
        drop(notice);
        recover_saves(&save.store).unwrap();
    }

    #[test]
    fn fat_recovery_allows_a_renamed_id_only_for_the_verified_backup() {
        let save = InterruptedSave::new(false);
        let marker = fs::read_dir(&save.store).unwrap().next().unwrap().unwrap();
        let mut record: SaveRecord =
            serde_json::from_slice(&fs::read(marker.path()).unwrap()).unwrap();
        match &mut record.original_id {
            FileId::Extended(_, id) => id[0] ^= 1,
            FileId::Legacy(_, id) => *id ^= 1,
        }
        let directory = open_directory(&record.directory).unwrap();
        assert!(record.recover_in(&directory, false).is_err());
        assert!(!save.original.exists());
        fs::write(&save.backup, b"changed!").unwrap();
        assert!(record.recover_in(&directory, true).is_err());
        assert!(!save.original.exists());
        fs::write(&save.backup, b"original").unwrap();
        record.recover_in(&directory, true).unwrap();
        assert_eq!(fs::read(&save.original).unwrap(), b"original");
        assert!(!save.backup.exists());
    }

    #[test]
    fn incomplete_pending_journals_are_discarded_and_legacy_corruption_reported_once() {
        let directory = tempfile::tempdir().unwrap();
        let pending = directory.path().join(".shiori-0011223344556677.pending");
        fs::write(&pending, b"{").unwrap();
        recover_saves(directory.path()).unwrap();
        assert!(!pending.exists());
        let legacy = directory.path().join(".shiori-0011223344556677.json");
        fs::write(&legacy, b"{").unwrap();
        let warning = recover_saves(directory.path()).unwrap_err();
        assert!(warning.to_string().contains("無効な復元記録を退避"));
        assert!(!legacy.exists());
        acknowledge_recovery(&warning).unwrap();
        drop(warning);
        let preserved = fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(preserved.path()).unwrap(), b"{");
        recover_saves(directory.path()).unwrap();
    }

    #[test]
    fn a_completed_fat_save_retains_the_backup_and_stops_recovery() {
        let save = InterruptedSave::new(true);
        let marker = fs::read_dir(&save.store).unwrap().next().unwrap().unwrap();
        let name = marker.file_name();
        let key = artifact_key(name.to_str().unwrap(), ".json").unwrap();
        let mut record: SaveRecord =
            serde_json::from_slice(&fs::read(marker.path()).unwrap()).unwrap();
        for identity in [&mut record.original_id, &mut record.staged_id] {
            match identity {
                FileId::Extended(_, id) => id[0] ^= 1,
                FileId::Legacy(_, id) => *id ^= 1,
            }
        }
        let store = open_store(&save.store).unwrap();
        let journal = open_journal(&store, &name).unwrap().unwrap();
        let directory = record.open_document_directory().unwrap();
        record.recover_in(&directory, true).unwrap();
        mark_cleanup(&journal, &record).unwrap();
        let notice = finish_cleanup(&record, &directory, journal, key).unwrap_err();
        assert!(notice.to_string().contains("復旧用ファイルを保持"));
        acknowledge_recovery(&notice).unwrap();
        drop(notice);
        assert!(save
            .store
            .join(format!("{ARTIFACT_PREFIX}{key}.acknowledged"))
            .is_file());
        recover_saves(&save.store).unwrap();
        assert_eq!(fs::read(&save.original).unwrap(), b"edited");
        assert_eq!(fs::read(&save.backup).unwrap(), b"original");
    }

    #[test]
    fn temporary_file_cleanup_is_retried_after_the_document_is_restored() {
        let save = InterruptedSave::new(false);
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&save.temporary)
            .unwrap();
        let notice = recover_saves(&save.store).unwrap_err();
        assert!(notice.to_string().contains("文書本体の処理は完了"));
        assert_eq!(fs::read(&save.original).unwrap(), b"original");
        assert!(fs::read_dir(&save.store)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "cleanup"));
        drop(reader);
        recover_saves(&save.store).unwrap();
        save.assert_recovered(b"original");
    }

    #[test]
    fn editing_the_restored_document_retires_cleanup_and_reports_the_temporary_file() {
        let save = InterruptedSave::new(false);
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&save.temporary)
            .unwrap();
        assert!(recover_saves(&save.store).is_err());
        assert_eq!(fs::read(&save.original).unwrap(), b"original");
        drop(reader);

        fs::write(&save.original, b"edited again").unwrap();
        let notice = recover_saves(&save.store).unwrap_err();
        assert!(notice
            .to_string()
            .contains(save.temporary.to_string_lossy().as_ref()));
        assert_eq!(fs::read(&save.temporary).unwrap(), b"edited");
        assert_eq!(fs::read(&save.original).unwrap(), b"edited again");
        acknowledge_recovery(&notice).unwrap();
        drop(notice);
        recover_saves(&save.store).unwrap();
    }

    #[test]
    fn all_retained_backups_are_reported_on_their_first_recovery() {
        let first = InterruptedSave::with_stream(true, true);
        let second = InterruptedSave::with_stream(true, true);
        let common = tempfile::tempdir().unwrap();
        for save in [&first, &second] {
            let mut stream = save.original.as_os_str().to_os_string();
            stream.push(":note");
            fs::write(PathBuf::from(stream), b"later").unwrap();
            let record = fs::read_dir(&save.store).unwrap().next().unwrap().unwrap();
            fs::rename(record.path(), common.path().join(record.file_name())).unwrap();
        }
        let notice = recover_saves(common.path()).unwrap_err();
        for save in [&first, &second] {
            assert!(save.backup.is_file());
            assert!(notice
                .to_string()
                .contains(save.backup.to_string_lossy().as_ref()));
        }
        acknowledge_recovery(&notice).unwrap();
        drop(notice);
        recover_saves(common.path()).unwrap();
    }

    #[test]
    fn a_changed_temporary_is_reported_until_the_notice_is_acknowledged() {
        let save = InterruptedSave::new(false);
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&save.temporary)
            .unwrap();
        assert!(recover_saves(&save.store).is_err());
        drop(reader);
        fs::write(&save.temporary, b"changed recovery file").unwrap();
        let first_notice = recover_saves(&save.store).unwrap_err();
        assert!(first_notice
            .to_string()
            .contains(save.temporary.to_string_lossy().as_ref()));
        // Closing before the dialog was confirmed must keep its notification.
        drop(first_notice);
        let next_notice = recover_saves(&save.store).unwrap_err();
        assert!(next_notice
            .to_string()
            .contains(save.temporary.to_string_lossy().as_ref()));
        assert_eq!(fs::read(&save.temporary).unwrap(), b"changed recovery file");
        acknowledge_recovery(&next_notice).unwrap();
        drop(next_notice);
        recover_saves(&save.store).unwrap();
        assert_eq!(fs::read(&save.original).unwrap(), b"original");
    }

    #[test]
    fn a_legacy_retention_record_is_shown_until_explicitly_acknowledged() {
        let save = InterruptedSave::with_stream(true, true);
        let mut stream = save.original.as_os_str().to_os_string();
        stream.push(":note");
        fs::write(PathBuf::from(stream), b"later").unwrap();
        drop(recover_saves(&save.store).unwrap_err());
        let entry = fs::read_dir(&save.store).unwrap().next().unwrap().unwrap();
        fs::rename(entry.path(), entry.path().with_extension("retained")).unwrap();
        let warning = recover_saves(&save.store).unwrap_err();
        assert!(warning
            .to_string()
            .contains(save.backup.to_string_lossy().as_ref()));
        drop(warning);
        let retry = recover_saves(&save.store).unwrap_err();
        acknowledge_recovery(&retry).unwrap();
        drop(retry);
        recover_saves(&save.store).unwrap();
        assert!(save.backup.is_file());
    }
}
