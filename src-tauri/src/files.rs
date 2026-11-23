use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use serde::Serialize;
use tauri::{State, WebviewWindow};
use tauri_plugin_dialog::{
    DialogExt, FileDialogBuilder, FilePath, MessageDialogButtons, MessageDialogKind,
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use crate::{
    file_io::{
        canonical_path, open_document_file, open_selected_file, save_path, validate_markdown,
        write_atomic, AppResult, OpenedFolder, SaveDestination, MAX_DOCUMENT_BYTES, MAX_LINK_BYTES,
    },
    images,
};

const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "txt"];
const BOM: &[u8] = b"\xEF\xBB\xBF";
const MAX_APPROVED_FOLDERS: usize = 32;

#[derive(Clone)]
pub struct Session(Arc<SessionState>);

struct SessionState {
    documents: Mutex<Documents>,
    file_action: Arc<AsyncMutex<()>>,
    image_reads: Arc<Semaphore>,
    recovery_dir: PathBuf,
}

impl Session {
    pub fn new(recovery_dir: PathBuf) -> Self {
        Self(Arc::new(SessionState {
            documents: Mutex::new(Documents::default()),
            file_action: Arc::new(AsyncMutex::new(())),
            image_reads: Arc::new(Semaphore::new(4)),
            recovery_dir,
        }))
    }

    pub(crate) fn recovery_dir(&self) -> &Path {
        &self.0.recovery_dir
    }
}

#[derive(Default)]
struct Documents {
    current: Option<CurrentDocument>,
    save_to: Option<PendingSave>,
    serial: u32,
    startup_checked: bool,
}

impl Documents {
    fn next_id(&mut self) -> AppResult<u32> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or("栞を再起動してください。")?;
        Ok(self.serial)
    }

    fn revision(&self) -> Option<u32> {
        self.current.as_ref().map(|document| document.revision)
    }

    fn selected_save(&self, ticket: u32) -> AppResult<&PendingSave> {
        self.save_to
            .as_ref()
            .filter(|selected| selected.ticket == ticket && selected.revision == self.revision())
            .ok_or_else(|| "保存先をもう一度選択してください。".into())
    }
}

struct PendingSave {
    ticket: u32,
    revision: Option<u32>,
    destination: SaveDestination,
}

#[derive(Clone)]
struct CurrentDocument {
    destination: SaveDestination,
    revision: u32,
    encoding: Encoding,
    approved_folders: Vec<OpenedFolder>,
}

impl CurrentDocument {
    fn path(&self) -> &Path {
        &self.destination.path
    }

    fn folder(&self) -> &Path {
        self.destination.folder.path()
    }

    fn approve_folder(&mut self, folder: OpenedFolder) -> AppResult<()> {
        if folder.path().starts_with(self.folder())
            || self
                .approved_folders
                .iter()
                .any(|root| folder.path().starts_with(root.path()))
        {
            return Ok(());
        }
        self.approved_folders
            .retain(|root| !root.path().starts_with(folder.path()));
        if self.approved_folders.len() >= MAX_APPROVED_FOLDERS {
            return Err("参照フォルダーが上限に達しました。".into());
        }
        self.approved_folders.push(folder);
        Ok(())
    }

    fn read_image(&self, source: &str) -> AppResult<String> {
        let path = images::reference_path(self.folder(), source)?;
        images::read_reference(&path, &self.destination.folder, &self.approved_folders)
    }
}

#[derive(Clone, Copy, Default)]
struct Encoding {
    bom: bool,
    crlf: bool,
}

#[derive(Serialize)]
pub struct OpenDocument {
    path: String,
    markdown: String,
    revision: u32,
}

#[derive(Serialize)]
pub struct SavedDocument {
    path: String,
    revision: u32,
}

#[derive(Serialize)]
pub struct ChosenImage {
    source: String,
    alt: String,
}

impl Session {
    fn lock(&self) -> AppResult<MutexGuard<'_, Documents>> {
        self.0
            .documents
            .lock()
            .map_err(|_| "文書の状態を取得できませんでした。".into())
    }

    pub(crate) fn begin_action(&self) -> AppResult<OwnedMutexGuard<()>> {
        Arc::clone(&self.0.file_action)
            .try_lock_owned()
            .map_err(|_| "別のファイル操作が進行中です。".into())
    }

    fn begin_image_read(&self) -> AppResult<OwnedSemaphorePermit> {
        Arc::clone(&self.0.image_reads)
            .try_acquire_owned()
            .map_err(|_| "画像の読み込みが混み合っています。".into())
    }

    fn current_path(&self) -> AppResult<Option<PathBuf>> {
        Ok(self
            .lock()?
            .current
            .as_ref()
            .map(|doc| doc.path().to_path_buf()))
    }

    fn document(&self, revision: u32) -> AppResult<CurrentDocument> {
        self.lock()?
            .current
            .as_ref()
            .filter(|document| document.revision == revision)
            .cloned()
            .ok_or_else(|| "表示中の文書が切り替わりました。".into())
    }
}

pub(crate) async fn file_task<T: Send + 'static>(
    work: impl FnOnce() -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

fn pick_path(file: Option<FilePath>) -> AppResult<Option<PathBuf>> {
    file.map(|item| item.into_path().map_err(|error| error.to_string()))
        .transpose()
}

fn dialog(window: &WebviewWindow, path: Option<&Path>) -> FileDialogBuilder<tauri::Wry> {
    let builder = window.dialog().file().set_parent(window);
    match path.and_then(Path::parent) {
        Some(directory) => builder.set_directory(directory),
        None => builder,
    }
}

fn load_document(state: &Session, path: &Path) -> AppResult<OpenDocument> {
    let (destination, bytes) = open_document_file(path)?;
    let contents = bytes.strip_prefix(BOM).unwrap_or(&bytes);
    let text = std::str::from_utf8(contents).map_err(|_| {
        format!(
            "UTF-8のMarkdownファイルを指定してください。\n{}",
            destination.path.display()
        )
    })?;
    validate_markdown(text, bytes.len())?;
    let encoding = Encoding {
        bom: bytes.starts_with(BOM),
        crlf: text.contains("\r\n")
            && text.matches('\r').count() == text.matches('\n').count()
            && text.matches('\n').count() == text.matches("\r\n").count(),
    };
    let markdown = if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_owned()
    };
    let mut documents = state.lock()?;
    let revision = documents.next_id()?;
    let display_path = destination.path.to_string_lossy().into_owned();
    documents.current = Some(CurrentDocument {
        destination,
        revision,
        encoding,
        approved_folders: Vec::new(),
    });
    documents.save_to = None;
    Ok(OpenDocument {
        path: display_path,
        markdown,
        revision,
    })
}

#[tauri::command]
pub async fn startup_document(
    window: WebviewWindow,
    state: State<'_, Session>,
) -> AppResult<Option<OpenDocument>> {
    let action = state.begin_action()?;
    if std::mem::replace(&mut state.lock()?.startup_checked, true) {
        return Ok(None);
    }
    let session = state.inner().clone();
    file_task(move || {
        let _action = action;
        #[cfg(windows)]
        if let Err(error) = crate::windows_fs::recovery::recover_saves(session.recovery_dir()) {
            let confirmed = window
                .dialog()
                .message(error.to_string())
                .title("中断された保存の復元")
                .kind(MessageDialogKind::Warning)
                .parent(&window)
                .blocking_show();
            if confirmed {
                if let Err(acknowledgment) =
                    crate::windows_fs::recovery::acknowledge_recovery(&error)
                {
                    eprintln!("Could not acknowledge save recovery warning: {acknowledgment}");
                }
            }
        }
        #[cfg(not(windows))]
        let _ = window;
        std::env::args_os()
            .nth(1)
            .map(|path| load_document(&session, Path::new(&path)))
            .transpose()
    })
    .await
}

fn approve_overwrite(window: &WebviewWindow, destination: &SaveDestination) -> bool {
    // The file picker returns only a path. Bind the decision to the file
    // version obtained after the picker closed.
    !destination.exists()
        || window
            .dialog()
            .message(format!(
                "次のファイルを上書きしますか？\n{}",
                destination.path.display()
            ))
            .title("上書きの確認")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "上書き".into(),
                "キャンセル".into(),
            ))
            .parent(window)
            .blocking_show()
}

#[tauri::command]
pub fn new_document(state: State<'_, Session>) -> AppResult<()> {
    let _action = state.begin_action()?;
    let mut documents = state.lock()?;
    documents.current = None;
    documents.save_to = None;
    Ok(())
}

#[tauri::command]
pub async fn open_document(
    window: WebviewWindow,
    state: State<'_, Session>,
) -> AppResult<Option<OpenDocument>> {
    let action = state.begin_action()?;
    let current = state.current_path()?;
    let chosen = dialog(&window, current.as_deref())
        .set_title("Markdownファイルを開く")
        .add_filter("Markdown / テキスト", MARKDOWN_EXTENSIONS)
        .add_filter("すべてのファイル", &["*"])
        .blocking_pick_file();
    let Some(path) = pick_path(chosen)? else {
        return Ok(None);
    };
    let session = state.inner().clone();
    file_task(move || {
        let _action = action;
        load_document(&session, &path)
    })
    .await
    .map(Some)
}

#[tauri::command]
pub async fn choose_save_path(
    window: WebviewWindow,
    state: State<'_, Session>,
) -> AppResult<Option<u32>> {
    let _action = state.begin_action()?;
    state.lock()?.save_to = None;
    let current = state.current_path()?;
    let name = current
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("無題.md");
    let chosen = dialog(&window, current.as_deref())
        .set_title("名前を付けて保存")
        .set_file_name(name)
        .add_filter("Markdown", MARKDOWN_EXTENSIONS)
        .blocking_save_file();
    let Some(path) = pick_path(chosen)? else {
        return Ok(None);
    };
    let destination = file_task(move || save_path(&path)).await?;
    if !approve_overwrite(&window, &destination) {
        return Ok(None);
    }
    let mut documents = state.lock()?;
    let ticket = documents.next_id()?;
    documents.save_to = Some(PendingSave {
        ticket,
        revision: documents.revision(),
        destination,
    });
    Ok(Some(ticket))
}

#[tauri::command]
pub fn cancel_save_path(state: State<'_, Session>, ticket: u32) -> AppResult<()> {
    let mut documents = state.lock()?;
    if documents
        .save_to
        .as_ref()
        .is_some_and(|selected| selected.ticket == ticket)
    {
        documents.save_to = None;
    }
    Ok(())
}

fn save_markdown(
    state: &Session,
    markdown: &str,
    ticket: Option<u32>,
    revision: Option<u32>,
) -> AppResult<SavedDocument> {
    let mut document = {
        let mut documents = state.lock()?;
        if documents.revision() != revision {
            return Err("表示中の文書が切り替わりました。".into());
        }
        match ticket {
            Some(ticket) => {
                let destination = documents.selected_save(ticket)?.destination.clone();
                documents.save_to = None;
                let revision = documents.next_id()?;
                match &documents.current {
                    Some(current) => {
                        let mut next = current.clone();
                        let previous = std::mem::replace(&mut next.destination, destination);
                        next.revision = revision;
                        next.approve_folder(previous.folder)?;
                        next
                    }
                    None => CurrentDocument {
                        destination,
                        revision,
                        encoding: Encoding::default(),
                        approved_folders: Vec::new(),
                    },
                }
            }
            None => documents
                .current
                .clone()
                .ok_or("先に保存先を選択してください。")?,
        }
    };
    if markdown.contains('\r') {
        return Err("保存するMarkdownの改行形式が無効です。".into());
    }
    let encoding = document.encoding;
    let extra = if encoding.crlf {
        markdown.bytes().filter(|byte| *byte == b'\n').count()
    } else {
        0
    };
    let encoded_bytes = markdown
        .len()
        .checked_add(extra)
        .and_then(|length| length.checked_add(if encoding.bom { BOM.len() } else { 0 }))
        .ok_or("Markdownファイルは64 MiBまでです。")?;
    validate_markdown(markdown, encoded_bytes)?;
    document.destination = write_atomic(&document.destination, state.recovery_dir(), |writer| {
        if encoding.bom {
            writer.write_all(BOM)?;
        }
        if encoding.crlf {
            for segment in markdown.split_inclusive('\n') {
                if let Some(line) = segment.strip_suffix('\n') {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\r\n")?;
                } else {
                    writer.write_all(segment.as_bytes())?;
                }
            }
            Ok(())
        } else {
            writer.write_all(markdown.as_bytes())
        }
    })?;
    let folder = document.folder().to_path_buf();
    document
        .approved_folders
        .retain(|approved| !approved.path().starts_with(&folder));
    let saved = SavedDocument {
        path: document.path().to_string_lossy().into_owned(),
        revision: document.revision,
    };
    let mut documents = state.lock()?;
    documents.current = Some(document);
    documents.save_to = None;
    Ok(saved)
}

#[tauri::command]
pub async fn save_document(
    state: State<'_, Session>,
    markdown: String,
    ticket: Option<u32>,
    revision: Option<u32>,
) -> AppResult<SavedDocument> {
    let action = state.begin_action()?;
    let session = state.inner().clone();
    file_task(move || {
        let _action = action;
        save_markdown(&session, &markdown, ticket, revision)
    })
    .await
}

#[tauri::command]
pub async fn read_image(
    state: State<'_, Session>,
    source: String,
    revision: u32,
) -> AppResult<String> {
    let document = state.document(revision)?;
    let permit = state.begin_image_read()?;
    file_task(move || {
        let _permit = permit;
        document.read_image(&source)
    })
    .await
}

#[tauri::command]
pub async fn rebase_image(
    state: State<'_, Session>,
    source: String,
    ticket: u32,
) -> AppResult<String> {
    images::validate_reference(&source)?;
    let (document, target) = {
        let documents = state.lock()?;
        let document = documents
            .current
            .clone()
            .ok_or("元の文書フォルダーが見つかりません。")?;
        (
            document,
            documents.selected_save(ticket)?.destination.folder.clone(),
        )
    };
    let target_folder = target.path();
    if document.folder() == target_folder {
        return Ok(source);
    }
    let Ok(path) = images::reference_path(document.folder(), &source) else {
        return Ok(source);
    };
    if let Some(relative) = images::relative_url(&path, target_folder) {
        let suffix = source
            .find(['?', '#'])
            .map(|index| &source[index..])
            .unwrap_or_default();
        return Ok(format!("{relative}{suffix}"));
    }
    let permit = state.begin_image_read()?;
    file_task(move || {
        let _permit = permit;
        document.read_image(&source)
    })
    .await
}

#[tauri::command]
pub async fn approve_image_folder(
    window: WebviewWindow,
    state: State<'_, Session>,
    revision: u32,
) -> AppResult<bool> {
    let _action = state.begin_action()?;
    let document = state.document(revision)?;
    let chosen = dialog(&window, Some(document.path()))
        .set_title("この文書の画像の参照フォルダーを許可")
        .blocking_pick_folder();
    let Some(folder) = pick_path(chosen)? else {
        return Ok(false);
    };
    let folder = file_task(move || OpenedFolder::open_selected(&folder)).await?;
    let mut documents = state.lock()?;
    let current = documents
        .current
        .as_mut()
        .filter(|current| current.revision == revision)
        .ok_or("表示中の文書が切り替わりました。")?;
    current.approve_folder(folder)?;
    Ok(true)
}

#[tauri::command]
pub async fn choose_image(
    window: WebviewWindow,
    state: State<'_, Session>,
) -> AppResult<Option<ChosenImage>> {
    let _action = state.begin_action()?;
    let current = state.lock()?.current.clone();
    let chosen = dialog(&window, current.as_ref().map(CurrentDocument::path))
        .set_title("画像を挿入")
        .add_filter("画像", images::EXTENSIONS)
        .blocking_pick_file();
    let Some(path) = pick_path(chosen)? else {
        return Ok(None);
    };
    file_task(move || {
        let (path, file) = open_selected_file(&path)?;
        let alt = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let relative = current
            .filter(|document| {
                document
                    .destination
                    .folder
                    .contains_selected_file(&path, &file)
            })
            .and_then(|document| images::relative_url(&path, document.folder()));
        let data = images::data_url(&path, file)?;
        let source = relative.unwrap_or(data);
        Ok(Some(ChosenImage { source, alt }))
    })
    .await
}

#[tauri::command]
pub fn open_link(url: String) -> AppResult<()> {
    if url.len() > MAX_LINK_BYTES
        || url
            .chars()
            .any(|character| character.is_control() || character == '\\')
    {
        return Err("リンクのURLが無効です。".into());
    }
    let parsed = tauri::Url::parse(&url).map_err(|_| "リンクのURLが無効です。")?;
    let allowed = match parsed.scheme() {
        "http" | "https" => {
            parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
        }
        "mailto" => !parsed.path().is_empty(),
        _ => false,
    };
    if !allowed {
        return Err("このリンク形式には対応していません。".into());
    }
    tauri_plugin_opener::open_url(parsed.as_str(), None::<&str>).map_err(|error| error.to_string())
}

pub(crate) async fn export_path(
    window: &WebviewWindow,
    state: &Session,
    extension: &str,
) -> AppResult<Option<SaveDestination>> {
    let current = state.current_path()?;
    let name = current
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|name| name.to_str())
        .unwrap_or("無題");
    let kind = extension.to_uppercase();
    let chosen = dialog(window, current.as_deref())
        .set_title(format!("{kind}にエクスポート"))
        .set_file_name(format!("{name}.{extension}"))
        .add_filter(&kind, &[extension])
        .blocking_save_file();
    let Some(path) = pick_path(chosen)? else {
        return Ok(None);
    };
    let destination = file_task(move || save_path(&path)).await?;
    let path = &destination.path;
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    {
        return Err(format!(".{extension} のファイル名を指定してください。"));
    }
    if current.as_ref() == Some(path) || (current.is_some() && canonical_path(path).ok() == current)
    {
        return Err("編集中の文書とは別の出力先を指定してください。".into());
    }
    Ok(approve_overwrite(window, &destination).then_some(destination))
}

#[tauri::command]
pub async fn export_html(
    window: WebviewWindow,
    state: State<'_, Session>,
    html: String,
) -> AppResult<bool> {
    if html.len() > MAX_DOCUMENT_BYTES || html.contains('\0') {
        return Err("HTMLの出力サイズは64 MiBまでです。".into());
    }
    let action = state.begin_action()?;
    let Some(destination) = export_path(&window, &state, "html").await? else {
        return Ok(false);
    };
    let session = state.inner().clone();
    file_task(move || {
        let _action = action;
        write_atomic(&destination, session.recovery_dir(), |writer| {
            writer.write_all(html.as_bytes())
        })
        .map(|_| ())
    })
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(windows)]
    #[test]
    fn saving_a_loaded_document_uses_its_original_encoded_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("original.md");
        fs::write(&path, b"\xEF\xBB\xBF# original\r\n").unwrap();
        let session = Session::new(tmp.path().join("recovery"));
        let opened = load_document(&session, &path).unwrap();
        let saved = save_markdown(&session, "# authored\n", None, Some(opened.revision)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBF# authored\r\n");

        fs::write(&path, b"\xEF\xBB\xBF# external\r\n").unwrap();
        let error = save_markdown(&session, "# another edit\n", None, Some(saved.revision))
            .err()
            .expect("changes made outside the editor must be preserved");
        assert!(error.contains("内容が外部で変更"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBF# external\r\n");
    }

    #[test]
    fn save_ticket_cannot_be_reused_or_used_for_another_document() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        let destination = tmp.path().join("destination");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&destination).unwrap();
        let first = original.join("first.md");
        let second = destination.join("second.md");
        fs::write(&first, b"\xEF\xBB\xBF# first\r\n").unwrap();
        #[cfg(windows)]
        fs::write(original.join("image.svg"), "<svg/>").unwrap();
        let session = Session::new(tmp.path().join("recovery"));
        let opened = load_document(&session, &first).unwrap();
        let destination = save_path(&second).unwrap();
        let ticket = {
            let mut documents = session.lock().unwrap();
            let ticket = documents.next_id().unwrap();
            documents.save_to = Some(PendingSave {
                ticket,
                revision: Some(opened.revision),
                destination,
            });
            ticket
        };
        let saved =
            save_markdown(&session, "# updated\n", Some(ticket), Some(opened.revision)).unwrap();
        assert_ne!(saved.revision, opened.revision);
        assert_eq!(fs::read(&second).unwrap(), b"\xEF\xBB\xBF# updated\r\n");
        assert!(
            save_markdown(&session, "stale ticket", Some(ticket), Some(saved.revision)).is_err()
        );
        assert!(save_markdown(&session, "stale overwrite", None, Some(opened.revision)).is_err());
        assert_eq!(fs::read(&second).unwrap(), b"\xEF\xBB\xBF# updated\r\n");
        assert!(session.document(opened.revision).is_err());
        assert_eq!(fs::read(&first).unwrap(), b"\xEF\xBB\xBF# first\r\n");
        #[cfg(windows)]
        {
            let document = session.document(saved.revision).unwrap();
            assert!(document.read_image("../original/image.svg").is_ok());
            fs::rename(&original, tmp.path().join("previous")).unwrap();
            fs::create_dir(&original).unwrap();
            fs::write(original.join("image.svg"), "<svg/>").unwrap();
            assert!(document.read_image("../original/image.svg").is_err());
        }
    }
}
