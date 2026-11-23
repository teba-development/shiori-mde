use super::*;
use std::{io::PipeReader, mem::size_of_val};
use windows_sys::Win32::{
    Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT},
    System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, CREATE_NO_WINDOW,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, STARTUPINFOW,
    },
};

const MAX_APPLICATION_BYTES: u64 = 128 * 1024 * 1024;
const UNKNOWN_BUNDLE_MARKER: &[u8] = b"__TAURI_BUNDLE_TYPE_VAR_UNK";
const NSIS_BUNDLE_MARKER: &[u8] = b"__TAURI_BUNDLE_TYPE_VAR_NSS";

pub(super) fn expected_payload_digest(mut application: &File) -> io::Result<[u8; 32]> {
    application.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    application
        .take(MAX_APPLICATION_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_APPLICATION_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    normalize_bundle_marker(&mut bytes)?;
    Ok(Sha256::digest(&bytes).into())
}

fn normalize_bundle_marker(bytes: &mut [u8]) -> io::Result<()> {
    let mut positions = bytes
        .windows(UNKNOWN_BUNDLE_MARKER.len())
        .enumerate()
        .filter_map(|(index, value)| (value == UNKNOWN_BUNDLE_MARKER).then_some(index));
    let first = positions.next();
    if positions.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Ambiguous Tauri bundle type marker",
        ));
    }
    if let Some(offset) = first {
        // Tauri embeds the NSS variant in the installer and restores the
        // build output to UNK after bundling. Change only that complete marker.
        bytes[offset..offset + NSIS_BUNDLE_MARKER.len()].copy_from_slice(NSIS_BUNDLE_MARKER);
    }
    Ok(())
}

struct Extractor {
    executable: PathBuf,
    _directories: Vec<File>,
    _files: Vec<File>,
}

impl Extractor {
    fn open() -> io::Result<Self> {
        let configured = std::env::var_os("SHIORI_7ZIP").map(PathBuf::from);
        let candidates = configured.into_iter().chain(
            ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|root| PathBuf::from(root).join("7-Zip").join("7z.exe")),
        );
        for candidate in candidates {
            if !candidate.is_absolute() || candidate.file_name() != Some(OsStr::new("7z.exe")) {
                return Err(invalid());
            }
            let parent = candidate.parent().ok_or_else(invalid)?;
            let directories = match pin_directory(parent, false) {
                Ok(directories) => directories,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let root = directories.last().ok_or_else(invalid)?;
            let executable = open_regular_file(root, "7z.exe")?;
            let library = open_regular_file(root, "7z.dll")?;
            return Ok(Self {
                executable: opened_path(&executable)?,
                _directories: directories,
                _files: vec![executable, library],
            });
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Installer verification requires 7-Zip. Install it or set SHIORI_7ZIP to the absolute path of 7z.exe",
        ))
    }

    fn spawn(&self, archive: &File, job: &BuildJob) -> io::Result<(OwnedHandle, PipeReader)> {
        // Both paths refer to pinned files. Validated file paths have no quotes
        // or trailing separators, so surrounding them with quotes is sufficient.
        let archive_path = opened_path(archive)?;
        let mut command = OsString::from("\"");
        command.push(&self.executable);
        command.push("\" x -so -bso0 -bsp0 -bse2 -y \"");
        command.push(archive_path);
        command.push("\" shiori-mde.exe");
        let mut command: Vec<u16> = command.encode_wide().chain([0]).collect();
        let executable: Vec<u16> = self
            .executable
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();

        let (stdout, writer) = io::pipe()?;
        let null_device = OpenOptions::new().read(true).write(true).open(r"\\.\NUL")?;
        let inherited = [null_device.as_raw_handle(), writer.as_raw_handle()];
        for handle in inherited {
            // SAFETY: these handles are owned by this function and remain open.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        let jobs = [job.0.as_raw_handle()];
        let mut attributes = ProcessAttributes::new(&inherited, &jobs)?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: inherited[0],
                hStdOutput: inherited[1],
                hStdError: inherited[0],
                ..Default::default()
            },
            lpAttributeList: attributes.as_mut_ptr(),
        };
        let mut child = PROCESS_INFORMATION::default();
        // SAFETY: both strings are NUL-terminated, the command line is writable,
        // and the startup structure, attributes, and referenced handles are live.
        if unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                std::ptr::from_ref(&startup).cast(),
                &mut child,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful process creation transfers ownership of both handles.
        let process = unsafe { OwnedHandle::from_raw_handle(child.hProcess) };
        drop(unsafe { OwnedHandle::from_raw_handle(child.hThread) });
        Ok((process, stdout))
    }
}

struct ProcessAttributes<'a> {
    storage: Vec<usize>,
    _values: [&'a [HANDLE]; 2],
}

impl<'a> ProcessAttributes<'a> {
    fn new(handles: &'a [HANDLE], jobs: &'a [HANDLE]) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list queries the required size for two attributes.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: storage is aligned and writable for the required byte count.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = Self {
            storage,
            _values: [handles, jobs],
        };
        // The extractor joins the job at creation, before it can execute any code.
        for (key, values) in [
            (PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles),
            (PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs),
        ] {
            // SAFETY: the initialized list borrows these arrays until it is deleted.
            if unsafe {
                UpdateProcThreadAttribute(
                    attributes.as_mut_ptr(),
                    0,
                    key as usize,
                    values.as_ptr().cast(),
                    size_of_val(values),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(attributes)
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for ProcessAttributes<'_> {
    fn drop(&mut self) {
        // SAFETY: this list was initialized and its storage and values are live.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

pub(super) fn verify_payload(archive: &File, expected: [u8; 32], job: &BuildJob) -> io::Result<()> {
    let extractor = Extractor::open()?;
    let (child, stdout) = extractor.spawn(archive, job)?;
    let read: io::Result<[u8; 32]> = (|| {
        let mut digest = Sha256::new();
        let length = io::copy(&mut stdout.take(MAX_APPLICATION_BYTES + 1), &mut digest)?;
        if length == 0 || length > MAX_APPLICATION_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(digest.finalize().into())
    })();
    if read.is_err() {
        // Stop the extractor and its descendants if stdout could not be read within the limit.
        job.terminate()?;
    }
    // SAFETY: child owns the process handle returned by CreateProcessW.
    if unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    job.wait_for_children()?;
    let actual: [u8; 32] = read?;
    let mut exit_code = 0;
    // SAFETY: the process handle is live and exit_code receives its status.
    if unsafe { GetExitCodeProcess(child.as_raw_handle(), &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if exit_code != 0 || actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "The installer does not contain the application prepared for this build",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_complete_tauri_bundle_marker_is_adjusted() {
        let mut bytes = b"prefix UNK __TAURI_BUNDLE_TYPE_VAR_UNK suffix".to_vec();
        normalize_bundle_marker(&mut bytes).unwrap();
        assert_eq!(bytes, b"prefix UNK __TAURI_BUNDLE_TYPE_VAR_NSS suffix");
        normalize_bundle_marker(&mut bytes).unwrap();
        assert_eq!(bytes, b"prefix UNK __TAURI_BUNDLE_TYPE_VAR_NSS suffix");
        let mut duplicate = UNKNOWN_BUNDLE_MARKER.repeat(2);
        assert!(normalize_bundle_marker(&mut duplicate).is_err());
    }
}
