#![deny(unsafe_code)]

#[path = "../../../src-tauri/src/file_io.rs"]
#[allow(dead_code)]
mod file_io;
#[allow(unsafe_code, dead_code)]
mod windows_fs {
    include!("../../../src-tauri/src/windows_fs.rs");
    include!("../../frontend-fs/src/guarded_paths.rs");
}

use base64::{engine::general_purpose::STANDARD, Engine};
use brotli::{enc::BrotliEncoderParams, BrotliCompress};
use proc_macro2::{Literal, TokenStream};
use quote::quote;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    env,
    io::{self, Read},
    path::{Path, PathBuf},
};
use tauri_utils::{
    config::SecurityConfig,
    html2::{inject_nonce_token, normalize_script_for_csp, parse_doc, serialize_doc},
};
use windows_fs::guarded_paths::{open_snapshot, pin_directory, SNAPSHOT_FILE};

const MAGIC: &[u8] = b"SHIORI\0\x01";
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 4097;
const MAX_INDEX_BYTES: usize = 128 * 1024;
const STAGE_ENV: &str = "SHIORI_FRONTEND_SNAPSHOT";
const DIGEST_ENV: &str = "SHIORI_FRONTEND_SHA256";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn stage_name(id: &str) -> Result<String> {
    if !hex(id, 32) {
        return Err("Invalid frontend snapshot identifier".into());
    }
    Ok(format!(".frontend-package-{id}"))
}

fn read_snapshot(manifest: &Path, stage: &str) -> Result<Vec<u8>> {
    let project = manifest
        .parent()
        .filter(|_| manifest.file_name().is_some_and(|name| name == "src-tauri"))
        .ok_or("Expected the src-tauri manifest directory")?;
    let pins = pin_directory(project, false)?;
    let mut file = open_snapshot(
        pins.last().ok_or("Missing project directory")?,
        stage,
        SNAPSHOT_FILE,
    )?;
    let length = usize::try_from(file.metadata()?.len())?;
    if length > MAX_BYTES {
        return Err("Frontend snapshot exceeds its size limit".into());
    }
    let mut bytes = Vec::with_capacity(length);
    file.by_ref()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() != length {
        return Err("Frontend snapshot length changed".into());
    }
    Ok(bytes)
}

fn valid_name(name: &str) -> bool {
    name.len() <= 255
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && file_io::valid_file_component(name)
        && (name == "index.html"
            || [".js", ".css", ".png", ".txt"]
                .iter()
                .any(|ext| name.ends_with(ext)))
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8]> {
    let (head, tail) = bytes
        .split_at_checked(length)
        .ok_or("Truncated frontend snapshot")?;
    *bytes = tail;
    Ok(head)
}

struct Asset<'a> {
    name: &'a str,
    contents: &'a [u8],
}

fn verified_assets<'a>(bytes: &'a [u8], expected: &str) -> Result<Vec<Asset<'a>>> {
    if !hex(expected, 64) {
        return Err("Invalid frontend snapshot digest".into());
    }
    if bytes.len() > MAX_BYTES || format!("{:x}", Sha256::digest(bytes)) != expected {
        return Err("Frontend snapshot SHA-256 mismatch".into());
    }
    // The borrowed entries refer to the exact buffer that passed verification.
    let mut rest = bytes
        .strip_prefix(MAGIC)
        .ok_or("Unknown frontend snapshot format")?;
    let count = u32::from_le_bytes(take(&mut rest, 4)?.try_into()?) as usize;
    if !(1..=MAX_FILES).contains(&count) {
        return Err("Invalid frontend snapshot file count".into());
    }
    let mut assets = Vec::with_capacity(count);
    let mut names = BTreeSet::new();
    for _ in 0..count {
        let name_len = u16::from_le_bytes(take(&mut rest, 2)?.try_into()?) as usize;
        let file_len = u32::from_le_bytes(take(&mut rest, 4)?.try_into()?) as usize;
        let name = std::str::from_utf8(take(&mut rest, name_len)?)?;
        if !valid_name(name) || !names.insert(name.to_ascii_lowercase()) {
            return Err("Invalid or duplicate frontend snapshot filename".into());
        }
        let contents = take(&mut rest, file_len)?;
        if name == "index.html" && !(1..=MAX_INDEX_BYTES).contains(&file_len) {
            return Err("Invalid frontend index length".into());
        }
        assets.push(Asset { name, contents });
    }
    if !rest.is_empty() || !names.contains("index.html") {
        return Err("Missing index or unexpected trailing data in frontend snapshot".into());
    }
    Ok(assets)
}

fn script_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(normalize_script_for_csp(bytes));
    format!("'sha256-{}'", STANDARD.encode(digest))
}

fn embed_assets(assets: Vec<Asset<'_>>, security: &SecurityConfig) -> Result<TokenStream> {
    let mut files = Vec::with_capacity(assets.len());
    let mut scripts = BTreeSet::new();
    let mut inline_scripts = BTreeSet::new();
    let modifications = &security.dangerous_disable_asset_csp_modification;
    let hash_scripts = modifications.can_modify("script-src");
    let compression = BrotliEncoderParams {
        quality: 4,
        ..Default::default()
    };
    for Asset { name, contents } in assets {
        let key = format!("/{name}");
        let html;
        let mut input = if name == "index.html" && security.csp.is_some() {
            let document = parse_doc(std::str::from_utf8(contents)?.to_owned());
            inject_nonce_token(&document, modifications);
            if hash_scripts {
                inline_scripts.extend(
                    document
                        .select("script:not(:empty)")
                        .iter()
                        .map(|node| script_hash(node.text().as_bytes())),
                );
            }
            html = serialize_doc(&document);
            html.as_slice()
        } else {
            if hash_scripts && name.ends_with(".js") {
                scripts.insert(script_hash(contents));
            }
            contents
        };
        let mut compressed = Vec::new();
        BrotliCompress(&mut input, &mut compressed, &compression)?;
        let literal = Literal::byte_string(&compressed);
        files.push(quote!(#key => #literal as &'static [u8]));
    }
    Ok(quote! {{
        use ::tauri::utils::assets::{CspHash, EmbeddedAssets, phf, phf::phf_map};
        EmbeddedAssets::new(
            phf_map! { #(#files,)* },
            &[#(CspHash::Script(#scripts),)*],
            phf_map! { "/index.html" => &[#(CspHash::Script(#inline_scripts),)*] as &'static [CspHash<'static>] },
        )
    }})
}

fn build_tokens() -> Result<TokenStream> {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("Missing Cargo manifest directory")?);
    let stage = stage_name(
        &env::var(STAGE_ENV)
            .map_err(|_| "Use an npm build script to prepare the frontend snapshot")?,
    )?;
    let digest = env::var(DIGEST_ENV)?;
    if !hex(&digest, 64) {
        return Err("Invalid frontend snapshot digest".into());
    }
    let bytes = read_snapshot(&manifest, &stage)?;
    let assets = verified_assets(&bytes, &digest)?;
    let (config, _) = tauri_codegen::get_config(&manifest.join("tauri.conf.json"))
        .map_err(|error| io::Error::other(format!("Read Tauri configuration: {error}")))?;
    embed_assets(assets, &config.app.security)
}

#[proc_macro]
pub fn embed(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    if !input.is_empty() {
        return quote!(compile_error!("embed! takes no arguments")).into();
    }
    match build_tokens() {
        Ok(tokens) => tokens.into(),
        Err(error) => {
            let message = format!("Cannot embed the verified frontend: {error}");
            quote!(compile_error!(#message)).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = MAGIC.to_vec();
        output.extend((files.len() as u32).to_le_bytes());
        for (name, contents) in files {
            output.extend((name.len() as u16).to_le_bytes());
            output.extend((contents.len() as u32).to_le_bytes());
            output.extend(name.as_bytes());
            output.extend_from_slice(contents);
        }
        output
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn checked_bytes_are_the_bytes_passed_to_embedding() {
        let mut bytes = snapshot(&[("index.html", b"index"), ("main.js", b"script")]);
        let expected = digest(&bytes);
        let assets = verified_assets(&bytes, &expected).unwrap();
        assert_eq!(assets[1].contents, b"script");
        *bytes.last_mut().unwrap() = b'!';
        assert!(verified_assets(&bytes, &expected).is_err());
    }

    #[test]
    fn rejects_paths_duplicates_and_wrong_framing_even_with_a_valid_digest() {
        let bad = [
            snapshot(&[("index.html", b"index"), ("../main.js", b"script")]),
            snapshot(&[
                ("index.html", b"index"),
                ("MAIN.js", b"one"),
                ("main.js", b"two"),
            ]),
            snapshot(&[("index.html", b"index"), ("COM1.js", b"script")]),
            snapshot(&[("index.html", b"index"), ("a.js:stream", b"script")]),
            snapshot(&[("main.js", b"script")]),
        ];
        for bytes in bad {
            assert!(verified_assets(&bytes, &digest(&bytes)).is_err());
        }
        let mut bytes = snapshot(&[("index.html", b"index")]);
        bytes.push(0);
        assert!(verified_assets(&bytes, &digest(&bytes)).is_err());
        bytes.truncate(bytes.len() - 2);
        assert!(verified_assets(&bytes, &digest(&bytes)).is_err());
        assert!(stage_name("../stage").is_err());
    }

    #[test]
    fn snapshot_reader_refuses_hard_links() {
        use std::fs;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let fixture = tempfile::tempdir_in(workspace.join(".build-tools")).unwrap();
        let stage = stage_name("0123456789abcdef0123456789abcdef").unwrap();
        let manifest = fixture.path().join("src-tauri");
        let package = fixture.path().join(&stage);
        fs::create_dir(&manifest).unwrap();
        fs::create_dir(&package).unwrap();
        let origin = fixture.path().join("fixture.txt");
        fs::write(&origin, b"test input").unwrap();
        fs::hard_link(&origin, package.join(SNAPSHOT_FILE)).unwrap();
        assert!(read_snapshot(&manifest, &stage).is_err());
        fs::remove_file(package.join(SNAPSHOT_FILE)).unwrap();
        fs::write(package.join(SNAPSHOT_FILE), b"test input").unwrap();
        assert_eq!(read_snapshot(&manifest, &stage).unwrap(), b"test input");
    }
}
