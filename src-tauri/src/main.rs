#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]

use tauri::{webview::NewWindowResponse, Manager, WebviewWindowBuilder};

mod file_io;
mod files;
mod images;
#[allow(unsafe_code)] // WebView2 COM calls for PDF export.
mod pdf;
#[cfg(windows)]
#[allow(unsafe_code)] // Windows handle-relative file operations.
mod windows_fs;

fn main() {
    #[cfg(feature = "custom-protocol")]
    let context = tauri::generate_context!(assets = shiori_frontend_embed::embed!());
    #[cfg(not(feature = "custom-protocol"))]
    let context = tauri::generate_context!();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let recovery_dir = app.path().app_data_dir()?.join("save-recovery");
            app.manage(files::Session::new(recovery_dir));
            let window = app
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .ok_or("メインウィンドウの設定が見つかりません。")?;
            let dev_origin = app
                .config()
                .build
                .dev_url
                .as_ref()
                .filter(|_| tauri::is_dev())
                .map(tauri::Url::origin);
            WebviewWindowBuilder::from_config(app, window)?
                .on_navigation(move |url| {
                    if let Some(origin) = &dev_origin {
                        return url.origin() == *origin;
                    }
                    url.port().is_none()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && matches!(
                            (url.scheme(), url.host_str()),
                            ("http", Some("tauri.localhost")) | ("tauri", Some("localhost"))
                        )
                })
                .on_new_window(|_, _| NewWindowResponse::Deny)
                .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            files::startup_document,
            files::new_document,
            files::open_document,
            files::choose_save_path,
            files::cancel_save_path,
            files::save_document,
            files::read_image,
            files::rebase_image,
            files::approve_image_folder,
            files::choose_image,
            files::open_link,
            files::export_html,
            pdf::export_pdf,
        ])
        .run(context)
        .expect("栞の起動に失敗しました");
}
