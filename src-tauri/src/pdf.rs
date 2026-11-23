use tauri::{State, WebviewWindow};

use crate::files::Session;

#[cfg(windows)]
use windows::Win32::System::Com::IStream;

#[cfg(windows)]
const MAX_PDF_BYTES: usize = 64 * 1024 * 1024;

#[cfg(windows)]
fn read_pdf_stream(stream: &IStream) -> Result<Vec<u8>, String> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut pdf = Vec::new();
    loop {
        let mut read = 0;
        // SAFETY: stream is used on the WebView2 callback thread. The output
        // buffer is writable for cb bytes and the byte count points to a u32.
        unsafe {
            stream.Read(
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                Some(&mut read),
            )
        }
        .ok()
        .map_err(|error| format!("PDFを読み取れませんでした。\n{error}"))?;
        let chunk = buffer
            .get(..read as usize)
            .ok_or("PDFの読み取りサイズが無効です。")?;
        if chunk.is_empty() {
            break;
        }
        if chunk.len() > MAX_PDF_BYTES - pdf.len() {
            return Err("PDFの出力サイズは64 MiBまでです。".into());
        }
        pdf.try_reserve(chunk.len())
            .map_err(|_| "PDFの読み取り用メモリを確保できませんでした。")?;
        pdf.extend_from_slice(chunk);
    }
    if !pdf.starts_with(b"%PDF-") {
        return Err("PDFとして認識できる出力がありません。".into());
    }
    Ok(pdf)
}

#[cfg(windows)]
#[tauri::command]
pub async fn export_pdf(window: WebviewWindow, state: State<'_, Session>) -> Result<bool, String> {
    use std::{io::Write, time::Duration};
    use webview2_com::{
        Microsoft::Web::WebView2::Win32::{
            ICoreWebView2Environment6, ICoreWebView2_16, ICoreWebView2_2,
        },
        PrintToPdfStreamCompletedHandler,
    };
    use windows_core::Interface;

    let action = state.begin_action()?;
    let Some(destination) = crate::files::export_path(&window, &state, "pdf").await? else {
        return Ok(false);
    };
    let (sender, receiver) = tokio::sync::oneshot::channel::<Result<Vec<u8>, String>>();

    window
        .with_webview(move |platform| {
            // SAFETY: with_webview runs on the WebView2 thread. The COM interfaces
            // and callback are used on its owning thread.
            let print = || unsafe {
                let webview = platform.controller().CoreWebView2()?;
                let environment: ICoreWebView2Environment6 =
                    webview.cast::<ICoreWebView2_2>()?.Environment()?.cast()?;
                let settings = environment.CreatePrintSettings()?;
                settings.SetPageWidth(210.0 / 25.4)?;
                settings.SetPageHeight(297.0 / 25.4)?;
                settings.SetMarginTop(14.0 / 25.4)?;
                settings.SetMarginBottom(14.0 / 25.4)?;
                settings.SetMarginLeft(16.0 / 25.4)?;
                settings.SetMarginRight(16.0 / 25.4)?;
                settings.SetShouldPrintBackgrounds(true)?;
                let callback =
                    PrintToPdfStreamCompletedHandler::create(Box::new(move |status, stream| {
                        if !sender.is_closed() {
                            let result = status.map_err(|error| error.to_string()).and_then(|()| {
                                let stream =
                                    stream.ok_or("WebView2がPDFを出力できませんでした。")?;
                                read_pdf_stream(&stream)
                            });
                            let _ = sender.send(result);
                        }
                        Ok(())
                    }));
                webview
                    .cast::<ICoreWebView2_16>()?
                    .PrintToPdfStream(&settings, &callback)
            };
            if let Err(error) = print() {
                eprintln!("WebView2 PrintToPdf: {error}");
            }
        })
        .map_err(|error| error.to_string())?;

    let pdf = tokio::time::timeout(Duration::from_secs(90), receiver)
        .await
        .map_err(|_| "PDFの出力がタイムアウトしました。")?
        .map_err(|_| "PDFの出力を開始できませんでした。")??;
    let session = state.inner().clone();
    crate::files::file_task(move || {
        let _action = action;
        crate::file_io::write_atomic(&destination, session.recovery_dir(), |writer| {
            writer.write_all(&pdf)
        })
        .map(|_| ())
    })
    .await?;
    Ok(true)
}

#[cfg(not(windows))]
#[tauri::command]
pub async fn export_pdf(
    _window: WebviewWindow,
    _state: State<'_, Session>,
) -> Result<bool, String> {
    Err("PDF出力にはWindows版の栞を使用してください。".into())
}
