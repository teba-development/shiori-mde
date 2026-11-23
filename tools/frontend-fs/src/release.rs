use std::{
    fs::TryLockError,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::WaitForSingleObject,
    },
};

use super::*;

mod pe {
    include!("pe.rs");
}

mod installer {
    include!("installer.rs");
}

mod publication {
    include!("publication.rs");
}

const POLL: Duration = Duration::from_millis(100);
const WORKSPACE_PREFIX: &str = ".shiori-build-";
const READY_WORKSPACE: &str = ".shiori-release-ready";

struct ReleaseLock {
    file: File,
    path: PathBuf,
    directories: Vec<File>,
}

impl ReleaseLock {
    fn open(output: &Path) -> io::Result<Self> {
        let directories = pin_directory(output, true)?;
        let target = directories.last().ok_or_else(invalid)?;
        let file = open_component_with_options(
            target,
            OsStr::new(RELEASE_LOCK),
            FILE_READ_DATA | FILE_WRITE_DATA | ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
            FILE_NON_DIRECTORY_FILE,
        )?;
        require_disk(&file)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || basic_file_info(&file)?.nNumberOfLinks != 1
        {
            return Err(invalid());
        }
        Ok(Self {
            file,
            path: opened_path(target)?,
            directories,
        })
    }

    fn wait(&self, parent: &OwnedHandle) -> io::Result<()> {
        loop {
            check_parent(parent)?;
            match self.file.try_lock() {
                Ok(()) => return check_parent(parent),
                Err(TryLockError::WouldBlock) => thread::sleep(POLL),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
    }

    fn directory(&self) -> &File {
        self.directories.last().expect("pinned output directory")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyWorkspace {
    name: String,
    id: FileId,
}

struct Workspace {
    directory: File,
    path: PathBuf,
    name: String,
}

impl Workspace {
    fn take(output: &ReleaseLock) -> io::Result<Self> {
        let root = output.directory();
        let marker = match open_save_target(
            root,
            OsStr::new(READY_WORKSPACE),
            FILE_READ_DATA | ATTRIBUTES | DELETE,
            0,
        ) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(marker) = marker {
            if basic_file_info(&marker)?.nNumberOfLinks != 1 {
                return Err(invalid());
            }
            let mut bytes = Vec::new();
            (&marker).take(1025).read_to_end(&mut bytes)?;
            // Consume the proof of completion before handing the directory
            // to any child. An interrupted run cannot make it reusable.
            delete_on_close(&marker)?;
            drop(marker);
            if bytes.len() <= 1024 {
                if let Ok(ready) = serde_json::from_slice::<ReadyWorkspace>(&bytes) {
                    if let Some(workspace) = Self::reopen(root, ready)? {
                        return Ok(workspace);
                    }
                }
            }
        }
        for _ in 0..8 {
            let suffix = getrandom::u64().map_err(io::Error::other)?;
            let name = format!("{WORKSPACE_PREFIX}{suffix:016x}");
            match build_directory(root, &name, FILE_CREATE) {
                Ok(directory) => return Self::from_directory(directory, name),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    fn reopen(root: &File, ready: ReadyWorkspace) -> io::Result<Option<Self>> {
        let valid_name = ready
            .name
            .strip_prefix(WORKSPACE_PREFIX)
            .is_some_and(|suffix| {
                suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        if !valid_name {
            return Ok(None);
        }
        match build_directory(root, &ready.name, FILE_OPEN) {
            Ok(directory) if file_id(&directory)? == ready.id => {
                Ok(Some(Self::from_directory(directory, ready.name)?))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn from_directory(directory: File, name: String) -> io::Result<Self> {
        Ok(Self {
            path: opened_path(&directory)?,
            directory,
            name,
        })
    }

    fn mark_ready(&self, output: &ReleaseLock) -> io::Result<()> {
        let marker = ReadyWorkspace {
            name: self.name.clone(),
            id: file_id(&self.directory)?,
        };
        let mut staged = StagedFile::create(output.directory(), &output.path)?;
        serde_json::to_writer(staged.file_mut(), &marker)?;
        staged.file_mut().sync_all()?;
        staged.publish(OsStr::new(READY_WORKSPACE))
    }

    fn publish(&self, output: &ReleaseLock, artifacts: &[Vec<String>], job: &BuildJob) -> io::Result<()> {
        let published = publication::publish(self, output, artifacts, job)?;
        for path in published {
            println!("栞: published {}", path.display());
        }
        Ok(())
    }
}

fn build_directory(
    parent: &File,
    name: &str,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
) -> io::Result<File> {
    let file = open_component_with_options(
        parent,
        OsStr::new(name),
        TRAVERSE | FILE_LIST_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        disposition,
        FILE_DIRECTORY_FILE,
    )?;
    directory(&file)?;
    Ok(file)
}

fn artifact_directory(root: &File, parents: &[String], create: bool) -> io::Result<Vec<File>> {
    let mut pins = Vec::with_capacity(parents.len());
    for name in parents {
        let parent = pins.last().unwrap_or(root);
        pins.push(build_directory(
            parent,
            name,
            if create { FILE_OPEN_IF } else { FILE_OPEN },
        )?);
    }
    Ok(pins)
}

fn parse_artifacts(encoded: &OsStr) -> io::Result<Vec<Vec<String>>> {
    let text = encoded
        .to_str()
        .filter(|text| text.len() <= 2048)
        .ok_or_else(invalid)?;
    let artifacts: Vec<Vec<String>> = serde_json::from_str(text)?;
    if artifacts.len() > 2 || artifacts.iter().any(|parts| !valid_artifact_path(parts)) {
        return Err(invalid());
    }
    Ok(artifacts)
}

fn valid_artifact_path(parts: &[String]) -> bool {
    (parts.len() == 3 || parts.len() == 5)
        && matches!(
            parts[0].as_str(),
            "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc"
        )
        && matches!(parts[1].as_str(), "release" | "debug")
        && (parts.len() == 3 || parts[2..4] == ["bundle", "nsis"])
        && parts.last().is_some_and(|part| part.ends_with(".exe"))
        && parts.iter().all(|part| {
            valid_file_component(part) && part.encode_utf16().count() <= MAX_COMPONENT_UNITS
        })
}

fn check_parent(parent: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the caller's process handle is live and allows SYNCHRONIZE.
    match unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) } {
        WAIT_TIMEOUT => Ok(()),
        WAIT_OBJECT_0 => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Packaging caller exited before the build started",
        )),
        _ => Err(io::Error::last_os_error()),
    }
}

struct BuildJob(OwnedHandle);

impl BuildJob {
    fn new() -> io::Result<Self> {
        // SAFETY: null parameters create a private, non-inheritable job handle.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful creation transfers ownership of the handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is live and limits has the specified Windows layout.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are live. The worker waits for stdin before launching any tools.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: the handle owns this build's private job, including tool descendants.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait_for_children(&self) -> io::Result<()> {
        loop {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: the job handle is live and info is writable for the supplied size.
            if unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            thread::sleep(POLL);
        }
    }
}

pub fn run(arguments: &[OsString]) -> io::Result<i32> {
    let [pid, output, node, worker, artifacts, args @ ..] = arguments else {
        return Err(invalid());
    };
    let artifacts = parse_artifacts(artifacts)?;
    let pid = pid
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid)?;
    for (path, extension) in [(node, "exe"), (worker, "mjs")] {
        let path = Path::new(path);
        if !path.is_absolute()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
        {
            return Err(invalid());
        }
    }
    let parent = parent_process(pid)?;
    let output = ReleaseLock::open(Path::new(output))?;
    output.wait(&parent)?;
    println!("栞: release output locked");
    publication::recover(&output)?;
    let workspace = Workspace::take(&output)?;
    println!("栞: build workspace {}", workspace.path.display());

    let job = BuildJob::new()?;
    let mut child = Command::new(node)
        .arg(worker)
        .args(args)
        .env("CARGO_TARGET_DIR", &workspace.path)
        .env("CARGO_BUILD_BUILD_DIR", &workspace.path)
        // Preloaded modules could start tools before the worker receives its gate byte.
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let started = child
        .stdin
        .take()
        .ok_or_else(invalid)
        .and_then(|mut pipe| pipe.write_all(&[1]));
    // Once running, finish supervising the job even if the original Node
    // process exits. Cargo and NSIS can continue using output after that exit.
    let status = child.wait();
    job.wait_for_children()?;
    started?;
    let code = status?.code().unwrap_or(1);
    let publication = if code == 0 {
        workspace.publish(&output, &artifacts, &job)
    } else {
        Ok(())
    };
    // Publication also runs tools. Its result is ready for reuse only after
    // their jobs have finished or have been terminated on an error.
    job.wait_for_children()?;
    if let Err(error) = workspace.mark_ready(&output) {
        eprintln!("Could not mark the finished build workspace reusable: {error}");
    }
    publication?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_lock_is_exclusive_until_the_owner_closes() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("new-target").join("nested");
        let owner = ReleaseLock::open(&output).unwrap();
        owner.file.lock().unwrap();
        let contender = ReleaseLock::open(&output).unwrap();
        assert!(matches!(
            contender.file.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(owner);
        contender.file.try_lock().unwrap();
    }

    #[test]
    fn only_a_finished_workspace_can_be_reused() {
        let directory = tempfile::tempdir().unwrap();
        let output = ReleaseLock::open(directory.path()).unwrap();
        let interrupted = Workspace::take(&output).unwrap();
        let next = Workspace::take(&output).unwrap();
        assert_ne!(interrupted.path, next.path);
        next.mark_ready(&output).unwrap();
        let resumed = Workspace::take(&output).unwrap();
        assert_eq!(next.path, resumed.path);
        let third = Workspace::take(&output).unwrap();
        assert_ne!(resumed.path, third.path);
    }

    #[test]
    fn publish_copies_only_the_finished_executable_to_the_shared_output() {
        let directory = tempfile::tempdir().unwrap();
        let output = ReleaseLock::open(directory.path()).unwrap();
        let workspace = Workspace::take(&output).unwrap();
        let artifact = vec![
            "aarch64-pc-windows-msvc".into(),
            "debug".into(),
            "test.exe".into(),
        ];
        let source = workspace.path.join(artifact.iter().collect::<PathBuf>());
        let published = output.path.join(artifact.iter().collect::<PathBuf>());
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(published.parent().unwrap()).unwrap();
        let previous = pe::application_fixture(pe::ARM64, b"previous");
        let current = pe::application_fixture(pe::ARM64, b"current");
        std::fs::write(&published, &previous).unwrap();
        std::fs::write(&source, b"MZ").unwrap();
        let job = BuildJob::new().unwrap();
        assert!(workspace
            .publish(&output, std::slice::from_ref(&artifact), &job)
            .is_err());
        assert_eq!(std::fs::read(&published).unwrap(), previous);
        std::fs::write(&source, &current).unwrap();
        workspace.publish(&output, &[artifact], &job).unwrap();
        assert_eq!(std::fs::read(&published).unwrap(), current);
    }
}
