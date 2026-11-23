use std::io::{self, Read, Seek, SeekFrom};
use std::{
    fs::File,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
};
use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READONLY, SEC_IMAGE_NO_EXECUTE};

const I386: u16 = 0x014c;
pub(super) const AMD64: u16 = 0x8664;
pub(super) const ARM64: u16 = 0xaa64;
const DOS_SIZE: usize = 64;
const PE_FILE_HEADER_SIZE: usize = 24;
const SECTION_HEADER_SIZE: usize = 40;
const MAX_SECTIONS: u16 = 96;
const NSIS_HEADER_SIZE: usize = 28;
const NSIS_ALIGNMENT: u64 = 512;
const NSIS_MAGIC: &[u8; 16] = b"\xef\xbe\xad\xdeNullsoftInst";

fn invalid_image() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Malformed Windows executable")
}

fn invalid_installer() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Malformed NSIS installer archive",
    )
}

// All reads use fixed layout offsets in the checked header buffers below.
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub(super) fn validate_file(file: &mut File, target: &str, installer: bool) -> io::Result<()> {
    validate(file, file.metadata()?.len(), target, installer)?;
    // SAFETY: file owns a locked live handle. No view is mapped or executed.
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            null(),
            PAGE_READONLY | SEC_IMAGE_NO_EXECUTE,
            0,
            0,
            null(),
        )
    };
    if mapping.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful creation transfers ownership of the mapping handle.
    let _mapping = unsafe { OwnedHandle::from_raw_handle(mapping) };
    Ok(())
}

pub(super) fn validate(
    reader: &mut (impl Read + Seek),
    length: u64,
    target: &str,
    installer: bool,
) -> io::Result<()> {
    let expected_machine = match target {
        "x86_64-pc-windows-msvc" => AMD64,
        "aarch64-pc-windows-msvc" => ARM64,
        _ => return Err(invalid_image()),
    };
    let mut dos = [0; DOS_SIZE];
    reader.read_exact(&mut dos)?;
    let pe_offset = u64::from(dword(&dos, 0x3c));
    if &dos[..2] != b"MZ"
        || pe_offset < DOS_SIZE as u64
        || pe_offset + PE_FILE_HEADER_SIZE as u64 > length
    {
        return Err(invalid_image());
    }

    reader.seek(SeekFrom::Start(pe_offset))?;
    let mut file_header = [0; PE_FILE_HEADER_SIZE];
    reader.read_exact(&mut file_header)?;
    let machine = word(&file_header, 4);
    let section_count = word(&file_header, 6);
    let optional_size = word(&file_header, 20);
    let characteristics = word(&file_header, 22);
    if &file_header[..4] != b"PE\0\0"
        || !(1..=MAX_SECTIONS).contains(&section_count)
        || characteristics & 0x0002 == 0
        || characteristics & (0x1000 | 0x2000) != 0
    {
        return Err(invalid_image());
    }
    // Tauri's NSIS launcher is currently PE32 even for native 64-bit payloads.
    if machine != expected_machine && !(installer && machine == I386) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Executable architecture does not match {target}"),
        ));
    }

    let (magic, fixed_size) = if machine == I386 {
        (0x10b, 96)
    } else {
        (0x20b, 112)
    };
    let headers_end = pe_offset
        + PE_FILE_HEADER_SIZE as u64
        + u64::from(optional_size)
        + u64::from(section_count) * SECTION_HEADER_SIZE as u64;
    if usize::from(optional_size) < fixed_size || headers_end > length {
        return Err(invalid_image());
    }
    let mut optional = vec![0; usize::from(optional_size)];
    reader.read_exact(&mut optional)?;
    let size_of_headers = u64::from(dword(&optional, 60));
    let entry_point = u64::from(dword(&optional, 16));
    let size_of_image = u64::from(dword(&optional, 56));
    let section_alignment = u64::from(dword(&optional, 32));
    let file_alignment = u64::from(dword(&optional, 36));
    if !section_alignment.is_power_of_two()
        || !file_alignment.is_power_of_two()
        || section_alignment < file_alignment
        || file_alignment > 65536
        || (section_alignment < 4096 && file_alignment != section_alignment)
        || (section_alignment >= 4096 && file_alignment < 512)
        || !size_of_image.is_multiple_of(section_alignment)
        || !size_of_headers.is_multiple_of(file_alignment)
    {
        return Err(invalid_image());
    }
    if word(&optional, 0) != magic
        || !matches!(word(&optional, 68), 2 | 3)
        || size_of_headers < headers_end
        || size_of_headers > length
        || entry_point == 0
        || entry_point >= size_of_image
        || u64::from(dword(&optional, fixed_size - 4)) > ((optional.len() - fixed_size) / 8) as u64
    {
        return Err(invalid_image());
    }

    let mut image_end = size_of_headers;
    let mut entry_has_code = false;
    let mut previous_virtual_end = size_of_headers.next_multiple_of(section_alignment);
    let mut previous_raw_end = size_of_headers;
    for _ in 0..section_count {
        let mut section = [0; SECTION_HEADER_SIZE];
        reader.read_exact(&mut section)?;
        let virtual_size = u64::from(dword(&section, 8));
        let virtual_start = u64::from(dword(&section, 12));
        let raw_size = u64::from(dword(&section, 16));
        let raw_start = u64::from(dword(&section, 20));
        let virtual_end = virtual_start + virtual_size.max(raw_size);
        if !virtual_start.is_multiple_of(section_alignment)
            || virtual_start < previous_virtual_end
            || virtual_end > size_of_image
        {
            return Err(invalid_image());
        }
        previous_virtual_end = virtual_end.next_multiple_of(section_alignment);
        if raw_size > 0 {
            let raw_end = raw_start + raw_size;
            if raw_start < previous_raw_end
                || raw_end > length
                || !raw_start.is_multiple_of(file_alignment)
                || !raw_size.is_multiple_of(file_alignment)
            {
                return Err(invalid_image());
            }
            previous_raw_end = raw_end;
            image_end = image_end.max(raw_end);
        }
        if dword(&section, 36) & 0x2000_0000 != 0
            && (virtual_start..virtual_start + virtual_size.max(raw_size)).contains(&entry_point)
            && entry_point - virtual_start < raw_size
        {
            entry_has_code = true;
        }
    }
    if !entry_has_code {
        return Err(invalid_image());
    }
    if installer {
        verify_nsis(reader, length, image_end)?;
    }
    Ok(())
}

fn verify_nsis(reader: &mut (impl Read + Seek), length: u64, image_end: u64) -> io::Result<()> {
    let offset = image_end.next_multiple_of(NSIS_ALIGNMENT);
    if offset + NSIS_HEADER_SIZE as u64 > length {
        return Err(invalid_installer());
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut header = [0; NSIS_HEADER_SIZE];
    reader.read_exact(&mut header)?;
    let flags = dword(&header, 0);
    let decoded_header_size = dword(&header, 20);
    let archive_size = u64::from(dword(&header, 24));
    let has_crc = flags & 4 == 0 || flags & 8 != 0;
    let crc_size = if has_crc { 4 } else { 0 };
    if &header[4..20] != NSIS_MAGIC
        || flags & !0x0e != 0
        || !(1..=i32::MAX as u32).contains(&decoded_header_size)
        || archive_size <= NSIS_HEADER_SIZE as u64 + crc_size
        || archive_size > length - offset
    {
        return Err(invalid_installer());
    }
    if has_crc {
        // Standard NSIS CRC excludes the first 512 bytes of the PE and ends
        // at the CRC field, before any appended Authenticode certificate.
        let crc_offset = offset + archive_size - crc_size;
        let mut remaining = crc_offset - NSIS_ALIGNMENT;
        reader.seek(SeekFrom::Start(NSIS_ALIGNMENT))?;
        let mut buffer = [0; 16 * 1024];
        let mut crc = crc32fast::Hasher::new();
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            reader.read_exact(&mut buffer[..count])?;
            crc.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let mut expected = [0; 4];
        reader.read_exact(&mut expected)?;
        if crc.finalize() != u32::from_le_bytes(expected) {
            return Err(invalid_installer());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn application_fixture(machine: u16, marker: &[u8]) -> Vec<u8> {
    let mut bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let header = dword(&bytes, 0x3c) as usize;
    bytes[header + 4..header + 6].copy_from_slice(&machine.to_le_bytes());
    bytes.extend_from_slice(marker);
    bytes
}

#[cfg(test)]
pub(super) fn fixture(machine: u16, installer: bool, marker: &[u8]) -> Vec<u8> {
    let (machine, fixed_size, magic) = if installer {
        (I386, 96u16, 0x10bu16)
    } else {
        (machine, 112u16, 0x20bu16)
    };
    let mut bytes = vec![0; 1024];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    bytes[68..70].copy_from_slice(&machine.to_le_bytes());
    bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
    bytes[84..86].copy_from_slice(&fixed_size.to_le_bytes());
    bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
    let optional = &mut bytes[88..88 + usize::from(fixed_size)];
    optional[..2].copy_from_slice(&magic.to_le_bytes());
    optional[16..20].copy_from_slice(&4096u32.to_le_bytes());
    optional[32..36].copy_from_slice(&4096u32.to_le_bytes());
    optional[36..40].copy_from_slice(&512u32.to_le_bytes());
    optional[56..60].copy_from_slice(&8192u32.to_le_bytes());
    optional[60..64].copy_from_slice(&512u32.to_le_bytes());
    optional[68..70].copy_from_slice(&2u16.to_le_bytes());
    let section = &mut bytes[88 + usize::from(fixed_size)..][..SECTION_HEADER_SIZE];
    section[8..12].copy_from_slice(&512u32.to_le_bytes());
    section[12..16].copy_from_slice(&4096u32.to_le_bytes());
    section[16..20].copy_from_slice(&512u32.to_le_bytes());
    section[20..24].copy_from_slice(&512u32.to_le_bytes());
    section[36..40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
    bytes[512..512 + marker.len()].copy_from_slice(marker);
    if installer {
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(NSIS_MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(NSIS_HEADER_SIZE as u32 + 5).to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&crc32fast::hash(&bytes[512..]).to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const TARGET: &str = "aarch64-pc-windows-msvc";

    fn check(bytes: &[u8], installer: bool) -> io::Result<()> {
        validate(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            TARGET,
            installer,
        )
    }

    #[test]
    fn rejects_truncated_pe_and_mismatched_machine() {
        let image = fixture(ARM64, false, b"image");
        assert!(check(&image, false).is_ok());
        assert!(check(b"MZ", false).is_err());
        assert!(check(&image[..image.len() - 1], false).is_err());
        assert!(check(&fixture(AMD64, false, b"image"), false).is_err());
    }

    #[test]
    fn rejects_invalid_section_alignment() {
        let mut image = fixture(ARM64, false, b"image");
        image[88 + 32..88 + 36].copy_from_slice(&0u32.to_le_bytes());
        assert!(check(&image, false).is_err());
    }

    #[test]
    fn validates_the_nsis_archive_bounds_and_crc() {
        let mut installer = fixture(ARM64, true, b"installer");
        assert!(check(&installer, true).is_ok());
        assert!(check(&installer[..installer.len() - 1], true).is_err());
        installer[1024 + NSIS_HEADER_SIZE] ^= 1;
        assert!(check(&installer, true).is_err());
    }
}
