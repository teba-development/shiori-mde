use std::{
    fs::File,
    path::{Component, Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use path_clean::PathClean;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};

use crate::file_io::{file_error, read_bounded, valid_file_component, AppResult, OpenedFolder};

pub const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif", "ico",
];
const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
const MAX_REFERENCE_BYTES: usize = 32 * 1024;
const MAX_SVG_DEPTH: usize = 128;
const MAX_SVG_ELEMENTS: usize = 100_000;
const FOLDER_PERMISSION: &str = "文書フォルダー外の画像です。ツールバーの「画像の参照フォルダーを許可」から参照先を選んでください。";

pub fn validate_reference(source: &str) -> AppResult<()> {
    if source.len() > MAX_REFERENCE_BYTES || source.chars().any(char::is_control) {
        return Err("画像パスの形式が無効です。".into());
    }
    Ok(())
}

pub fn reference_path(directory: &Path, source: &str) -> AppResult<PathBuf> {
    validate_reference(source)?;
    let raw = source.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| "画像パスの形式が無効です。")?;
    let relative = Path::new(decoded.as_ref());
    if raw.is_empty()
        || relative.has_root()
        || relative.file_name().is_none()
        || relative.components().any(|component| match component {
            Component::Normal(part) => !part.to_str().is_some_and(valid_file_component),
            Component::CurDir | Component::ParentDir => false,
            _ => true,
        })
    {
        return Err("文書からの有効な相対画像パスを指定してください。".into());
    }
    Ok(directory.join(relative).clean())
}

fn open_reference(
    path: &Path,
    document_folder: &OpenedFolder,
    approved_folders: &[OpenedFolder],
) -> AppResult<File> {
    let roots = std::iter::once(document_folder).chain(approved_folders);
    let root = roots
        .filter(|root| path.starts_with(root.path()))
        .max_by_key(|root| root.path().components().count())
        .ok_or(FOLDER_PERMISSION)?;
    let relative = path
        .strip_prefix(root.path())
        .map_err(|_| FOLDER_PERMISSION)?;
    root.open_relative(relative)
        .map_err(|error| file_error("画像を開けませんでした", path, error))
}

pub fn read_reference(
    path: &Path,
    document_folder: &OpenedFolder,
    approved_folders: &[OpenedFolder],
) -> AppResult<String> {
    let file = open_reference(path, document_folder, approved_folders)?;
    data_url(path, file)
}

pub fn data_url(path: &Path, file: File) -> AppResult<String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mime = match extension.to_ascii_lowercase().as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpeg",
        "gif" => "gif",
        "webp" => "webp",
        "svg" => "svg+xml",
        "bmp" => "bmp",
        "avif" => "avif",
        "ico" => "x-icon",
        _ => return Err("この画像形式には対応していません。".into()),
    };
    let bytes = read_bounded(&file, path, MAX_IMAGE_BYTES, "画像")?;
    let valid = match mime {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1A\n"),
        "jpeg" => bytes.starts_with(b"\xFF\xD8\xFF"),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        "svg+xml" => std::str::from_utf8(&bytes).is_ok_and(valid_svg),
        "bmp" => bytes.starts_with(b"BM"),
        "avif" => is_avif(&bytes),
        "x-icon" => bytes.starts_with(b"\0\0\x01\0"),
        _ => false,
    };
    if !valid {
        return Err(format!(
            "画像のファイル形式を確認してください。\n{}",
            path.display()
        ));
    }
    Ok(format!(
        "data:image/{mime};base64,{}",
        STANDARD.encode(bytes)
    ))
}

fn is_avif(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let Some(size) = bytes
        .get(..4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
    else {
        return false;
    };
    let brand = |value: &[u8]| matches!(value, b"avif" | b"avis");
    bytes.get(16..size as usize).is_some_and(|compatible| {
        let (brands, remainder) = compatible.as_chunks::<4>();
        remainder.is_empty()
            && (bytes.get(8..12).is_some_and(brand) || brands.iter().any(|value| brand(value)))
    })
}

fn valid_svg(text: &str) -> bool {
    fn element_ok(element: &BytesStart<'_>, depth: usize, seen_root: &mut bool) -> bool {
        if depth == 0 {
            if *seen_root || element.local_name().as_ref() != "svg" {
                return false;
            }
            *seen_root = true;
        }
        element.attributes().all(|attribute| attribute.is_ok())
    }

    if text
        .bytes()
        .any(|byte| byte < b' ' && !matches!(byte, b'\r' | b'\n' | b'\t'))
    {
        return false;
    }
    let mut reader = Reader::from_str(text.trim_start_matches('\u{feff}'));
    reader.config_mut().check_comments = true;
    let mut seen_root = false;
    let mut depth = 0;
    let mut elements = 0;
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (element, opens) = match event {
                    Event::Start(ref element) => (element, true),
                    Event::Empty(ref element) => (element, false),
                    _ => unreachable!(),
                };
                elements += 1;
                if elements > MAX_SVG_ELEMENTS
                    || depth >= MAX_SVG_DEPTH
                    || !element_ok(element, depth, &mut seen_root)
                {
                    return false;
                }
                depth += usize::from(opens);
            }
            Ok(Event::End(_)) if depth > 0 => depth -= 1,
            Ok(Event::Text(value))
                if depth == 0
                    && !value
                        .as_ref()
                        .bytes()
                        .all(|byte| byte.is_ascii_whitespace()) =>
            {
                return false
            }
            Ok(Event::DocType(_) | Event::PI(_) | Event::End(_)) | Err(_) => return false,
            Ok(Event::GeneralRef(_) | Event::CData(_)) if depth == 0 => return false,
            Ok(Event::Eof) => return seen_root && depth == 0,
            _ => {}
        }
    }
}

pub fn relative_url(path: &Path, directory: &Path) -> Option<String> {
    let relative = pathdiff::diff_paths(path, directory)?;
    let parts: Vec<String> = relative
        .components()
        .map(|part| match part {
            Component::ParentDir => "..".into(),
            _ => utf8_percent_encode(&part.as_os_str().to_string_lossy(), NON_ALPHANUMERIC)
                .to_string()
                .replace("%2E", ".")
                .replace("%2D", "-")
                .replace("%5F", "_")
                .replace("%7E", "~"),
        })
        .collect();
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn relative_paths_stay_within_approved_folders() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let root = crate::file_io::canonical_path(tmp.path()).unwrap();
        let docs = root.join("docs");
        let assets = root.join("assets");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&assets).unwrap();
        let image = assets.join("画像 1.svg");
        fs::write(&image, "<svg/>").unwrap();

        let path = reference_path(&docs, "../assets/画像%201.svg#part").unwrap();
        assert_eq!(path, image);
        let document = OpenedFolder::open_selected(&docs).unwrap();
        let assets = OpenedFolder::open_selected(&assets).unwrap();
        assert!(read_reference(&path, &document, &[]).is_err());
        assert_eq!(
            read_reference(&path, &document, &[assets]).unwrap(),
            "data:image/svg+xml;base64,PHN2Zy8+"
        );
        assert!(reference_path(&docs, "image%00.svg").is_err());
        assert!(reference_path(&docs, "%5Cimage.svg").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn approved_folder_replacement_is_rejected() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        let assets = tmp.path().join("assets");
        fs::create_dir(&docs).unwrap();
        fs::create_dir(&assets).unwrap();
        fs::write(assets.join("example.svg"), "<svg/>").unwrap();
        let document = OpenedFolder::open_selected(&docs).unwrap();
        let approved = [OpenedFolder::open_selected(&assets).unwrap()];
        let path = reference_path(document.path(), "../assets/example.svg").unwrap();
        assert!(read_reference(&path, &document, &approved).is_ok());

        fs::rename(&assets, tmp.path().join("original")).unwrap();
        fs::create_dir(&assets).unwrap();
        fs::write(assets.join("example.svg"), "<svg><path/></svg>").unwrap();
        assert!(read_reference(&path, &document, &approved).is_err());
    }

    #[test]
    fn svg_requires_well_formed_document_with_svg_root() {
        assert!(valid_svg(
            r#"<?xml version="1.0"?><svg><path d="M0 0"/></svg>"#
        ));
        assert!(valid_svg("<svg/>"));
        assert!(!valid_svg("<svg><path/></other>"));
        assert!(!valid_svg("<svg>"));
        assert!(!valid_svg("<html><svg/></html>"));
        assert!(!valid_svg("<!DOCTYPE svg><svg/>"));
    }
}
