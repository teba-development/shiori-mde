use super::*;

const PENDING: &str = ".shiori-release-publication.pending";
const LEGACY_COMPLETE: &str = ".shiori-release-publication.complete";
const COMPLETE_PREFIX: &str = ".shiori-release-publication-";
const STAGING_PREFIX: &str = ".shiori-release-stage-";
const RECORD_LIMIT: u64 = 16 * 1024;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    id: FileId,
    digest: [u8; 32],
}

impl Identity {
    fn read(file: &File) -> io::Result<Self> {
        Ok(Self {
            id: file_id(file)?,
            digest: content_digest(file)?,
        })
    }

    fn verify(&self, file: &File, rename_changed_id: bool) -> io::Result<()> {
        if !recovery::matches_artifact(file, &self.id, self.digest, rename_changed_id)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Release file identity changed",
            ));
        }
        Ok(())
    }

    fn verify_for_deletion(&self, file: &File, rename_changed_id: bool) -> io::Result<()> {
        if rename_changed_id {
            return Err(io::Error::other(
                "Cannot prove the file's identity after a rename on this volume; kept for recovery",
            ));
        }
        self.verify(file, false)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: Vec<String>,
    directory_id: FileId,
    temporary: String,
    staged: Identity,
    original: Option<Identity>,
    #[serde(default)]
    private_stage: bool,
}

impl Entry {
    fn name(&self) -> &OsStr {
        OsStr::new(self.path.last().expect("validated artifact path"))
    }

    fn backup(&self) -> OsString {
        // The record contains the validated staging name. The backup name
        // comes from it, so the record cannot independently select a file.
        OsString::from(format!(
            "{}.release-backup",
            self.temporary.trim_end_matches(".tmp")
        ))
    }

    fn valid(&self) -> bool {
        valid_artifact_path(&self.path)
            && staging_key(&self.temporary, ".shiori-", ".tmp").is_some()
    }
}

struct PreparedArtifact {
    entry: Entry,
    original: Option<File>,
    staged: StagedFile,
    parents: Vec<File>,
}

impl PreparedArtifact {
    fn promote(&mut self) -> io::Result<()> {
        if let Some(original) = &self.original {
            rename_at(original, None, &self.entry.backup())?;
        }
        let directory = self.parents.last().ok_or_else(invalid)?;
        rename_at(&self.staged.file, Some(directory), self.entry.name())?;
        self.staged.persisted = true;
        Ok(())
    }
}

fn open_optional(
    directory: &File,
    name: &OsStr,
    access: u32,
    share: u32,
) -> io::Result<Option<File>> {
    match open_save_target(directory, name, access, share) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn staging_key<'a>(name: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let key = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(key)
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StagedIdentity {
    name: String,
    id: FileId,
}

struct StagingDirectory {
    directory: File,
    record: File,
}

fn append_stage_record(record: &mut File, entry: &impl serde::Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(entry)?;
    bytes.push(b'\n');
    record.write_all(&bytes)?;
    record.sync_all()
}

impl StagingDirectory {
    fn create_file(&mut self) -> io::Result<StagedFile> {
        let file = StagedFile::create(&self.directory, &opened_path(&self.directory)?)?;
        // Persist each identity before copying bytes into the new file.
        append_stage_record(
            &mut self.record,
            &StagedIdentity {
                name: file.name.clone(),
                id: file_id(&file.file)?,
            },
        )?;
        Ok(file)
    }
}

fn begin_staging(output: &ReleaseLock) -> io::Result<StagingDirectory> {
    let root = output.directory();
    for _ in 0..8 {
        let key = getrandom::u64().map_err(io::Error::other)?;
        let name = format!("{STAGING_PREFIX}{key:016x}");
        let directory = match child_dir(root, &name, true, true) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // Record the directory, while it is still empty, before creating any
        // copies. The parent and the directory remain pinned by handles.
        let record = (|| {
            let id = file_id(&directory)?;
            let mut marker = open_component(
                root,
                OsStr::new(&format!("{name}.json")),
                FILE_WRITE_DATA | ATTRIBUTES | DELETE,
                0,
                FILE_CREATE,
            )?;
            let written = append_stage_record(&mut marker, &id);
            if written.is_err() {
                let _ = delete_on_close(&marker);
            }
            written.map(|()| marker)
        })();
        match record {
            Ok(record) => return Ok(StagingDirectory { directory, record }),
            Err(error) => {
                let _ = delete_on_close(&directory);
                return Err(error);
            }
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

fn read_stage_record(record: &File) -> io::Result<(FileId, Vec<StagedIdentity>)> {
    let bytes = read_record_bytes(record)?;
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    // A 0.1.17 record contains only the directory ID. Its files are unverified.
    let id = serde_json::from_slice(lines.next().ok_or_else(invalid)?)?;
    let mut files: Vec<StagedIdentity> = Vec::new();
    for line in lines {
        if line.last() != Some(&b'\n') {
            // An interrupted append never authorizes removing a file.
            break;
        }
        let entry: StagedIdentity = serde_json::from_slice(line)?;
        if files.len() >= 3
            || staging_key(&entry.name, ".shiori-", ".tmp").is_none()
            || files
                .iter()
                .any(|file| file.name.eq_ignore_ascii_case(&entry.name))
        {
            return Err(invalid());
        }
        files.push(entry);
    }
    Ok((id, files))
}

fn clean_staging(output: &ReleaseLock, marker: &str, key: &str) -> io::Result<()> {
    let Some(record) = open_record(output, marker)? else {
        return Ok(());
    };
    let (id, files) = read_stage_record(&record)?;
    let directory = match child_dir(
        output.directory(),
        &format!("{STAGING_PREFIX}{key}"),
        false,
        true,
    ) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return delete_on_close(&record),
        Err(error) => return Err(error),
    };
    if file_id(&directory)? != id {
        return Err(changed_destination());
    }
    if Volume::read(&directory)?.id_may_change_on_rename() {
        return Err(io::Error::other(
            "Staging files and their recovery record retained because this filesystem can reuse file IDs",
        ));
    }
    for file in files {
        if let Some(temporary) = open_optional(
            &directory,
            OsStr::new(&file.name),
            FILE_READ_DATA | ATTRIBUTES | DELETE,
            FILE_SHARE_READ,
        )? {
            if file_id(&temporary)? != file.id || basic_file_info(&temporary)?.nNumberOfLinks != 1 {
                return Err(io::Error::other(format!(
                    "Unverified staging file retained: {}",
                    file.name,
                )));
            }
            delete_on_close(&temporary)?;
        }
    }
    // The filesystem refuses to delete a directory that still has files.
    delete_on_close(&directory)?;
    drop(directory);
    delete_on_close(&record)
}

fn stage_artifact(
    source: &File,
    staging: &mut StagingDirectory,
    name: &OsStr,
    path: &[String],
) -> io::Result<StagedFile> {
    let mut file = open_save_target(source, name, FILE_READ_DATA | ATTRIBUTES, FILE_SHARE_READ)?;
    let length = file.metadata()?.len();
    pe::validate_file(&mut file, &path[0], path.len() == 5)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.join("/"))))?;
    file.seek(SeekFrom::Start(0))?;
    let mut staged = staging.create_file()?;
    if io::copy(&mut file, staged.file_mut())? != length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    staged.file_mut().sync_all()?;
    Ok(staged)
}

fn prepare(
    workspace: &Workspace,
    output: &ReleaseLock,
    staging: &mut StagingDirectory,
    path: &[String],
) -> io::Result<PreparedArtifact> {
    let (name, parents) = path.split_last().ok_or_else(invalid)?;
    let sources = artifact_directory(&workspace.directory, parents, false)?;
    let destinations = artifact_directory(output.directory(), parents, true)?;
    let source = sources.last().ok_or_else(invalid)?;
    let destination = destinations.last().ok_or_else(invalid)?;
    let name = OsStr::new(name);
    let staged = stage_artifact(source, staging, name, path)?;
    let mut attempts = 0;
    let original = loop {
        match open_optional(
            destination,
            name,
            FILE_READ_DATA | ATTRIBUTES | DELETE,
            FILE_SHARE_READ,
        ) {
            Err(error)
                if attempts < 20
                    && matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_SHARING_VIOLATION | ERROR_ACCESS_DENIED)
                    ) =>
            {
                attempts += 1;
                thread::sleep(POLL);
            }
            result => break result?,
        }
    };
    let entry = Entry {
        path: path.to_vec(),
        directory_id: file_id(destination)?,
        temporary: staged.name.clone(),
        staged: Identity::read(&staged.file)?,
        original: original.as_ref().map(Identity::read).transpose()?,
        private_stage: true,
    };
    Ok(PreparedArtifact {
        entry,
        original,
        staged,
        parents: destinations,
    })
}

fn valid_set<T: AsRef<[String]>>(paths: &[T]) -> bool {
    paths.len() <= 2
        && paths.iter().all(|path| valid_artifact_path(path.as_ref()))
        && (paths.len() < 2
            || !paths[0]
                .as_ref()
                .join("\\")
                .to_lowercase()
                .eq(&paths[1].as_ref().join("\\").to_lowercase()))
}

pub(super) fn publish(
    workspace: &Workspace,
    output: &ReleaseLock,
    paths: &[Vec<String>],
    job: &BuildJob,
) -> io::Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if !valid_set(paths) {
        return Err(invalid());
    }
    let mut staging = begin_staging(output)?;
    let commit = commit_publication(workspace, output, &mut staging, paths, job);
    drop(staging);
    let recovery = recover(output);
    match (commit, recovery) {
        (Ok(()), Ok(())) => Ok(paths.iter().map(|path| output.path.join(path.iter().collect::<PathBuf>())).collect()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(restore)) => Err(io::Error::other(format!(
            "Release publication failed: {error}\nPrevious outputs could not all be restored: {restore}"
        ))),
    }
}

fn commit_publication(
    workspace: &Workspace,
    output: &ReleaseLock,
    staging: &mut StagingDirectory,
    paths: &[Vec<String>],
    job: &BuildJob,
) -> io::Result<()> {
    let mut prepared = paths
        .iter()
        .map(|path| prepare(workspace, output, staging, path))
        .collect::<io::Result<Vec<_>>>()?;
    for item in &prepared {
        if item.entry.path.len() == 5 {
            let path = &item.entry.path;
            let application = prepared
                .iter()
                .find(|candidate| {
                    candidate.entry.path.len() == 3
                        && candidate.entry.path[..2] == path[..2]
                        && candidate.entry.name() == OsStr::new("shiori-mde.exe")
                })
                .ok_or_else(invalid)?;
            let parents = artifact_directory(&workspace.directory, &path[..path.len() - 1], false)?;
            let source = open_save_target(
                parents.last().ok_or_else(invalid)?,
                item.entry.name(),
                FILE_READ_DATA | ATTRIBUTES,
                FILE_SHARE_READ,
            )?;
            if content_digest(&source)? != item.entry.staged.digest {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The installer changed after preparation",
                ));
            }
            let expected = installer::expected_payload_digest(&application.staged.file)?;
            installer::verify_payload(&source, expected, job)?;
        }
    }
    let journal = begin_publication(output, staging, &prepared)?;
    let key = staging_key(&journal.name, ".shiori-", ".tmp").ok_or_else(invalid)?;
    let complete = format!("{COMPLETE_PREFIX}{key}.complete");
    for item in &mut prepared {
        item.promote()?;
    }
    rename_at(&journal.file, None, OsStr::new(&complete))?;
    for item in &prepared {
        if let Some(original) = &item.original {
            // A live handle identifies the original even on FAT. Failed
            // cleanup remains recorded in the completed transaction.
            let _ = delete_on_close(original);
        }
    }
    Ok(())
}

fn begin_publication(
    output: &ReleaseLock,
    staging: &mut StagingDirectory,
    prepared: &[PreparedArtifact],
) -> io::Result<StagedFile> {
    let entries: Vec<_> = prepared.iter().map(|item| &item.entry).collect();
    let mut journal = staging.create_file()?;
    serde_json::to_writer(journal.file_mut(), &entries)?;
    journal.file_mut().sync_all()?;
    rename_at(&journal.file, Some(output.directory()), OsStr::new(PENDING))?;
    journal.persisted = true;
    Ok(journal)
}

pub(super) fn recover(output: &ReleaseLock) -> io::Result<()> {
    recover_batch(output, PENDING, false)?;
    if let Err(error) = clean_finished_publications(output) {
        eprintln!("Could not check for finished release publications: {error}");
    }
    Ok(())
}

fn clean_finished_publications(output: &ReleaseLock) -> io::Result<()> {
    let mut restart = true;
    while let Some(entries) = names(output.directory(), restart)? {
        restart = false;
        for name in entries {
            let Some(name) = name.to_str() else { continue };
            let result = if name == LEGACY_COMPLETE
                || staging_key(name, COMPLETE_PREFIX, ".complete").is_some()
            {
                recover_batch(output, name, true)
            } else if let Some(key) = staging_key(name, STAGING_PREFIX, ".json") {
                clean_staging(output, name, key)
            } else {
                continue;
            };
            if let Err(error) = result {
                eprintln!("Release cleanup for {name} will retry: {error}");
            }
        }
    }
    Ok(())
}

fn open_record(output: &ReleaseLock, name: &str) -> io::Result<Option<File>> {
    let record = open_optional(
        output.directory(),
        OsStr::new(name),
        FILE_READ_DATA | ATTRIBUTES | DELETE,
        0,
    )?;
    if let Some(file) = &record {
        if basic_file_info(file)?.nNumberOfLinks != 1 {
            return Err(invalid());
        }
    }
    Ok(record)
}

fn read_record_bytes(file: &File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(invalid());
    }
    Ok(bytes)
}

fn recover_batch(output: &ReleaseLock, name: &str, complete: bool) -> io::Result<()> {
    let Some(journal) = open_record(output, name)? else {
        return Ok(());
    };
    let entries: Vec<Entry> = serde_json::from_slice(&read_record_bytes(&journal)?)?;
    let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_slice()).collect();
    if entries.is_empty() || !valid_set(&paths) || entries.iter().any(|entry| !entry.valid()) {
        return Err(invalid());
    }
    let mut first_error = None;
    for entry in entries {
        if let Err(error) = settle(output, &entry, complete) {
            first_error.get_or_insert_with(|| {
                io::Error::other(format!("{}: {error}", entry.path.join("/")))
            });
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    delete_on_close(&journal)
}

fn settle(output: &ReleaseLock, entry: &Entry, complete: bool) -> io::Result<()> {
    let parents = artifact_directory(
        output.directory(),
        &entry.path[..entry.path.len() - 1],
        false,
    )?;
    let directory = parents.last().ok_or_else(invalid)?;
    if file_id(directory)? != entry.directory_id {
        return Err(changed_destination());
    }
    let can_rename_id =
        Volume::read(directory).is_ok_and(|volume| volume.id_may_change_on_rename());
    settle_at(directory, entry, complete, can_rename_id)
}

fn settle_at(
    directory: &File,
    entry: &Entry,
    complete: bool,
    can_rename_id: bool,
) -> io::Result<()> {
    let access = FILE_READ_DATA | ATTRIBUTES | DELETE;
    let backup_path = entry.backup();
    let backup = open_optional(directory, &backup_path, access, FILE_SHARE_READ)?;
    if complete {
        if let Some(backup) = backup {
            entry
                .original
                .as_ref()
                .ok_or_else(invalid)?
                .verify_for_deletion(&backup, can_rename_id)?;
            let current = open_save_target(
                directory,
                entry.name(),
                FILE_READ_DATA | ATTRIBUTES,
                FILE_SHARE_READ,
            )?;
            entry.staged.verify(&current, false)?;
            delete_on_close(&backup)?;
        }
    } else {
        let current = open_optional(directory, entry.name(), access, FILE_SHARE_READ)?;
        match (entry.original.as_ref(), backup) {
            (Some(old), Some(backup)) => {
                old.verify(&backup, can_rename_id)?;
                if let Some(current) = current {
                    entry.staged.verify_for_deletion(&current, can_rename_id)?;
                    delete_on_close(&current)?;
                    drop(current);
                }
                rename_at(&backup, None, entry.name())?;
            }
            (Some(old), None) => {
                old.verify(&current.ok_or_else(changed_destination)?, can_rename_id)?
            }
            (None, None) => {
                if let Some(current) = current {
                    if can_rename_id {
                        return Err(io::Error::other(
                            "Incomplete publication: current file could not be identified after a rename; file and recovery record retained",
                        ));
                    }
                    if recovery::matches_artifact(
                        &current,
                        &entry.staged.id,
                        entry.staged.digest,
                        false,
                    )? {
                        delete_on_close(&current)?;
                    }
                }
            }
            (None, Some(_)) => return Err(changed_destination()),
        }
    }
    // 0.1.16 wrote its temporary files beside the published artifacts.
    if !entry.private_stage {
        if let Some(temporary) = open_optional(
            directory,
            OsStr::new(&entry.temporary),
            access,
            FILE_SHARE_READ,
        )? {
            entry
                .staged
                .verify_for_deletion(&temporary, can_rename_id)?;
            delete_on_close(&temporary)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn image(path: &[String], marker: &[u8]) -> Vec<u8> {
        assert_eq!(path.len(), 3);
        pe::application_fixture(pe::ARM64, marker)
    }

    struct Outputs {
        _directory: tempfile::TempDir,
        public: ReleaseLock,
        workspace: Workspace,
        paths: [Vec<String>; 2],
    }

    impl Outputs {
        fn fixture() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let public = ReleaseLock::open(directory.path()).unwrap();
            let workspace = Workspace::take(&public).unwrap();
            let profile = ["aarch64-pc-windows-msvc", "release"];
            let paths = [
                [profile.as_slice(), &["test.exe"]].concat(),
                [profile.as_slice(), &["second.exe"]].concat(),
            ]
            .map(|parts| parts.into_iter().map(str::to_owned).collect::<Vec<_>>());
            for path in &paths {
                let name = path.iter().collect::<PathBuf>();
                for (folder, marker) in [
                    (&public.path, b"previous".as_slice()),
                    (&workspace.path, b"prepared".as_slice()),
                ] {
                    let file = folder.join(&name);
                    fs::create_dir_all(file.parent().unwrap()).unwrap();
                    fs::write(file, image(path, marker)).unwrap();
                }
            }
            Self {
                _directory: directory,
                public,
                workspace,
                paths,
            }
        }

        fn assert_previous(&self) {
            for path in &self.paths {
                assert_eq!(
                    fs::read(self.public.path.join(path.iter().collect::<PathBuf>())).unwrap(),
                    image(path, b"previous")
                );
            }
        }
    }

    #[test]
    fn invalid_second_artifact_leaves_both_outputs_in_place() {
        let outputs = Outputs::fixture();
        let second = outputs
            .workspace
            .path
            .join(outputs.paths[1].iter().collect::<PathBuf>());
        fs::write(second, b"MZ").unwrap();
        let job = BuildJob::new().unwrap();
        assert!(publish(&outputs.workspace, &outputs.public, &outputs.paths, &job).is_err());
        outputs.assert_previous();
        assert!(!outputs.public.path.join(PENDING).exists());
    }

    #[test]
    fn interrupted_publication_restores_the_previous_set() {
        let outputs = Outputs::fixture();
        let mut staging = begin_staging(&outputs.public).unwrap();
        let mut artifacts: Vec<_> = outputs
            .paths
            .iter()
            .map(|path| prepare(&outputs.workspace, &outputs.public, &mut staging, path).unwrap())
            .collect();
        let journal = begin_publication(&outputs.public, &mut staging, &artifacts).unwrap();
        artifacts[0].promote().unwrap();
        // A terminated process closes handles but does not run stage cleanup.
        artifacts[1].staged.persisted = true;
        drop(artifacts);
        drop(journal);
        drop(staging);
        recover(&outputs.public).unwrap();
        outputs.assert_previous();
        assert!(!outputs.public.path.join(PENDING).exists());
    }

    #[test]
    fn interrupted_preparation_is_removed_without_touching_published_outputs() {
        let outputs = Outputs::fixture();
        let mut staging = begin_staging(&outputs.public).unwrap();
        let directory = opened_path(&staging.directory).unwrap();
        let mut artifact = prepare(
            &outputs.workspace,
            &outputs.public,
            &mut staging,
            &outputs.paths[0],
        )
        .unwrap();
        let file = directory.join(&artifact.entry.temporary);
        // Simulate interruption before the batch record was written.
        artifact.staged.persisted = true;
        drop(artifact);
        drop(staging);
        assert!(file.is_file());
        assert!(!outputs.public.path.join(PENDING).exists());
        recover(&outputs.public).unwrap();
        outputs.assert_previous();
        assert!(!directory.exists());
        assert!(!directory.with_extension("json").exists());
    }

    #[test]
    fn cleanup_preserves_unregistered_staging_files() {
        let outputs = Outputs::fixture();
        let mut staging = begin_staging(&outputs.public).unwrap();
        let directory = opened_path(&staging.directory).unwrap();
        let mut registered = staging.create_file().unwrap();
        let registered_path = directory.join(&registered.name);
        registered.persisted = true;
        drop(registered);
        drop(staging);

        let other = directory.join(".shiori-0123456789abcdef.tmp");
        fs::write(&other, b"not prepared by this build").unwrap();
        recover(&outputs.public).unwrap();
        assert!(!registered_path.exists());
        assert_eq!(fs::read(&other).unwrap(), b"not prepared by this build");
        assert!(directory.with_extension("json").is_file());
        outputs.assert_previous();
    }

    #[test]
    fn cleanup_preserves_a_replacement_of_a_registered_file() {
        let outputs = Outputs::fixture();
        let mut staging = begin_staging(&outputs.public).unwrap();
        let directory = opened_path(&staging.directory).unwrap();
        let mut registered = staging.create_file().unwrap();
        let path = directory.join(&registered.name);
        registered.persisted = true;
        drop(registered);
        drop(staging);

        fs::rename(&path, outputs.public.path.join("moved.tmp")).unwrap();
        fs::write(&path, b"another file").unwrap();
        recover(&outputs.public).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"another file");
        assert!(directory.with_extension("json").is_file());
    }

    #[test]
    fn completed_readonly_backup_does_not_block_a_later_publication() {
        let outputs = Outputs::fixture();
        let job = BuildJob::new().unwrap();
        let name = outputs.paths[0].iter().collect::<PathBuf>();
        let current = outputs.public.path.join(&name);
        let mut permissions = fs::metadata(&current).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&current, permissions).unwrap();
        publish(&outputs.workspace, &outputs.public, &outputs.paths, &job).unwrap();
        assert_eq!(
            fs::read(&current).unwrap(),
            image(&outputs.paths[0], b"prepared")
        );
        recover(&outputs.public).unwrap();
        let next = image(&outputs.paths[0], b"next");
        fs::write(outputs.workspace.path.join(name), &next).unwrap();
        publish(&outputs.workspace, &outputs.public, &outputs.paths, &job).unwrap();
        assert_eq!(fs::read(&current).unwrap(), next);
    }

    #[test]
    fn on_fat_an_unverified_current_output_keeps_rollback_incomplete() {
        let outputs = Outputs::fixture();
        let name = outputs.paths[0].iter().collect::<PathBuf>();
        let current = outputs.public.path.join(name);
        fs::remove_file(&current).unwrap();
        let mut staging = begin_staging(&outputs.public).unwrap();
        let artifact = prepare(
            &outputs.workspace,
            &outputs.public,
            &mut staging,
            &outputs.paths[0],
        )
        .unwrap();
        assert!(artifact.entry.original.is_none());
        let prepared = image(&outputs.paths[0], b"prepared");
        fs::write(&current, &prepared).unwrap();
        let folder = artifact.parents.last().unwrap();
        let error = settle_at(folder, &artifact.entry, false, true).unwrap_err();
        assert!(error.to_string().contains("Incomplete publication"));
        assert_eq!(fs::read(current).unwrap(), prepared);
    }
}
