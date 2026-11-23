use windows_sys::{
    Wdk::Storage::FileSystem::{
        FileStreamInformation, NtQueryInformationFile, FILE_STREAM_INFORMATION,
    },
    Win32::Foundation::{
        STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH,
    },
};

use super::*;

const STREAM_HEADER: usize = offset_of!(FILE_STREAM_INFORMATION, StreamName);
const MAX_STREAM_INFORMATION: usize = 1024 * 1024;
const MAX_STREAM_COUNT: usize = 256;
const MAX_STREAM_BYTES: u64 = MAX_DOCUMENT_BYTES as u64;
const DATA_SUFFIX: [u16; 6] = [58, 36, 68, 65, 84, 65];

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct Stream {
    suffix: Vec<u16>,
    length: u64,
}

impl Stream {
    fn open(
        &self,
        directory: &File,
        base: &OsStr,
        expected: &File,
        create: bool,
    ) -> io::Result<File> {
        if !base.to_str().is_some_and(valid_file_component) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut name: Vec<u16> = base.encode_wide().take(MAX_COMPONENT_UNITS + 1).collect();
        if name.len() > MAX_COMPONENT_UNITS {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        name.extend_from_slice(&self.suffix);
        let (access, share, disposition) = if create {
            (FILE_WRITE_DATA, FILE_SHARE_DELETE, FILE_CREATE)
        } else {
            (
                FILE_READ_DATA,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_OPEN,
            )
        };
        let stream = open_relative_name(
            directory,
            &mut name,
            access | ATTRIBUTES,
            share,
            disposition,
            0,
        )?;
        require_disk(&stream)?;
        if !same_file(&stream, expected)? {
            return Err(changed_destination());
        }
        if !create && stream.metadata()?.len() != self.length {
            return Err(changed_destination());
        }
        Ok(stream)
    }
}

pub(super) struct NamedStreams(Vec<(Stream, File)>);

impl NamedStreams {
    pub(super) fn open(directory: &File, base: &OsStr, file: &File) -> io::Result<Self> {
        let listed = streams(file)?;
        let mut result = Vec::with_capacity(listed.len());
        for stream in listed {
            let handle = stream.open(directory, base, file, false)?;
            result.push((stream, handle));
        }
        let opened = Self(result);
        opened.verify(file)?;
        Ok(opened)
    }

    pub(super) fn copy_to(
        &self,
        directory: &File,
        base: &OsStr,
        file: &File,
    ) -> io::Result<Vec<File>> {
        let mut handles = Vec::with_capacity(self.0.len());
        for (stream, source) in &self.0 {
            let mut destination = stream.open(directory, base, file, true)?;
            if io::copy(&mut source.take(stream.length), &mut destination)? != stream.length {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            destination.sync_all()?;
            handles.push(destination);
        }
        Ok(handles)
    }

    pub(super) fn same_data(&self, other: &Self) -> io::Result<bool> {
        if self
            .0
            .iter()
            .map(|(entry, _)| entry)
            .ne(other.0.iter().map(|(entry, _)| entry))
        {
            return Ok(false);
        }
        for ((entry, first), (_, second)) in self.0.iter().zip(&other.0) {
            if digest_bytes(first, entry.length)? != digest_bytes(second, entry.length)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn verify(&self, file: &File) -> io::Result<()> {
        if streams(file)?
            .iter()
            .ne(self.0.iter().map(|(entry, _)| entry))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ファイルの付加データを維持できなかったため、上書きを中止しました。",
            ));
        }
        Ok(())
    }
}

fn streams(file: &File) -> io::Result<Vec<Stream>> {
    let mut bytes = 4096;
    loop {
        let mut buffer = vec![0u64; bytes / size_of::<u64>()];
        let mut status = IO_STATUS_BLOCK::default();
        // SAFETY: the file handle is live and the initialized, aligned buffer
        // and status block are writable for the entire synchronous query.
        let result = unsafe {
            NtQueryInformationFile(
                file.as_raw_handle(),
                &mut status,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                FileStreamInformation,
            )
        };
        if matches!(
            result,
            STATUS_BUFFER_OVERFLOW | STATUS_BUFFER_TOO_SMALL | STATUS_INFO_LENGTH_MISMATCH
        ) && bytes < MAX_STREAM_INFORMATION
        {
            bytes *= 2;
            continue;
        }
        check_status(result)?;
        if status.Information > bytes {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the returned length was checked against the initialized buffer.
        let returned =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), status.Information) };
        let parsed = parse_streams(returned)?;
        check_stream_limits(&parsed)?;
        return Ok(parsed);
    }
}

fn check_stream_limits(streams: &[Stream]) -> io::Result<()> {
    let total = streams
        .iter()
        .try_fold(0u64, |sum, stream| sum.checked_add(stream.length));
    if streams.len() > MAX_STREAM_COUNT || total.is_none_or(|bytes| bytes > MAX_STREAM_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ファイルの付加データは合計64 MiB、256件までです。元ファイルは保持しました。",
        ));
    }
    Ok(())
}

fn parse_streams(mut bytes: &[u8]) -> io::Result<Vec<Stream>> {
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let header = bytes.get(..STREAM_HEADER).ok_or_else(invalid)?;
        let next = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let name_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let length = u64::try_from(i64::from_le_bytes(header[8..16].try_into().unwrap()))
            .map_err(|_| invalid())?;
        let end = STREAM_HEADER.checked_add(name_size).ok_or_else(invalid)?;
        let raw_name = bytes.get(STREAM_HEADER..end).ok_or_else(invalid)?;
        if !name_size.is_multiple_of(2) {
            return Err(invalid());
        }
        if !raw_name.is_empty() {
            let suffix: Vec<u16> = raw_name
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect();
            let name = suffix
                .strip_prefix(&[58])
                .and_then(|name| name.strip_suffix(&DATA_SUFFIX))
                .ok_or_else(invalid)?;
            if name.iter().any(|unit| matches!(unit, 0 | 47 | 58 | 92)) {
                return Err(invalid());
            }
            if !name.is_empty() {
                result.push(Stream { suffix, length });
            }
        }
        if next == 0 {
            break;
        }
        if next < end || next >= bytes.len() || !next.is_multiple_of(8) {
            return Err(invalid());
        }
        bytes = &bytes[next..];
    }
    result.sort_unstable();
    if result
        .windows(2)
        .any(|pair| pair[0].suffix == pair[1].suffix)
    {
        return Err(invalid());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::{save_path, write_atomic};
    use std::fs;

    #[test]
    fn alternate_stream_limits_apply_to_the_total_before_copying() {
        let stream = |length| Stream {
            suffix: Vec::new(),
            length,
        };
        assert!(check_stream_limits(&[stream(MAX_STREAM_BYTES - 1), stream(1)]).is_ok());
        assert!(check_stream_limits(&[stream(MAX_STREAM_BYTES), stream(1)]).is_err());
        assert!(check_stream_limits(&[stream(u64::MAX), stream(1)]).is_err());
        let empty = (0..=MAX_STREAM_COUNT)
            .map(|_| stream(0))
            .collect::<Vec<_>>();
        assert!(check_stream_limits(&empty).is_err());
    }

    #[test]
    fn overwrite_preserves_the_named_data_streams() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("document.md");
        let note = directory.path().join("document.md:review-note");
        let empty = directory.path().join("document.md:空");
        fs::write(&path, b"original").unwrap();
        fs::write(&note, b"retained metadata").unwrap();
        fs::write(&empty, b"").unwrap();
        let destination = save_path(&path).unwrap();
        write_atomic(&destination, &directory.path().join("recovery"), |writer| {
            writer.write_all(b"updated")
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"updated");
        assert_eq!(fs::read(&note).unwrap(), b"retained metadata");
        assert_eq!(fs::read(&empty).unwrap(), b"");
    }

    #[test]
    fn a_new_named_stream_during_save_keeps_the_original_document() {
        let directory = tempfile::tempdir().unwrap();
        let folder = open_directory(directory.path()).unwrap();
        let path = opened_path(&folder).unwrap();
        let name = OsStr::new("document.md");
        let document = path.join(name);
        let note = path.join("document.md:comment");
        fs::write(&document, b"before").unwrap();
        let expected = capture_destination(&folder, &path, name).unwrap().unwrap();
        let (mut staged, original) =
            StagedFile::begin_document(&folder, &path, name, Some(&expected)).unwrap();
        staged.file_mut().write_all(b"after").unwrap();

        {
            let mut handle = OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(SHARE_ALL)
                .open(&note)
                .unwrap();
            handle.write_all(b"new note").unwrap();
        }
        let store = path.join("recovery");
        let result = staged.persist_document(&folder, &path, name, original, &store);
        assert!(result.is_err());
        assert_eq!(fs::read(document).unwrap(), b"before");
        assert_eq!(fs::read(note).unwrap(), b"new note");
        assert_eq!(fs::read_dir(store).unwrap().count(), 0);
    }

    #[test]
    fn encrypted_originals_are_ineligible_for_overwriting() {
        assert!(check_overwrite_attributes(0).is_ok());
        let error = check_overwrite_attributes(FILE_ATTRIBUTE_ENCRYPTED).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("EFS暗号化"));
    }
}
