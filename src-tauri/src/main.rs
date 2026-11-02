#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod imgproc;
mod impose;
mod pages;
mod pdftools;
use imgproc::{Fail, Opts, PreviewOut, RunOut};
use serde_json::json;
use tauri::Emitter;

#[tauri::command]
fn expand(paths: Vec<String>) -> Vec<String> {
    imgproc::expand(paths)
}

#[tauri::command]
async fn preview(path: String, opts: Opts) -> Result<PreviewOut, String> {
    tauri::async_runtime::spawn_blocking(move || imgproc::preview(&path, &opts))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn run(app: tauri::AppHandle, files: Vec<String>, opts: Opts) -> Result<RunOut, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let n = files.len();
        let mut out = RunOut::default();
        for (i, f) in files.iter().enumerate() {
            match imgproc::convert(f, i, &opts) {
                Ok(true) => out.done += 1,
                Ok(false) => out.skipped += 1,
                Err(e) => out.failed.push(Fail { file: f.clone(), error: e }),
            }
            let _ = app.emit("progress", json!({ "i": i + 1, "n": n }));
        }
        out
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal(path: String) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
}

#[tauri::command]
async fn pdf_info(paths: Vec<String>) -> Result<Vec<pdftools::PdfInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || pdftools::info(paths))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn pdf_merge(paths: Vec<String>, out: String) -> Result<pdftools::MergeOut, String> {
    tauri::async_runtime::spawn_blocking(move || pdftools::merge(&paths, &out))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pdf_split(
    app: tauri::AppHandle,
    path: String,
    mode: String,
    spec: String,
    out_dir: Option<String>,
) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        pdftools::split(&path, &mode, &spec, out_dir.as_deref(), &|i, n| {
            let _ = app.emit("pdf-progress", json!({ "i": i, "n": n }));
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pdf_compress(
    app: tauri::AppHandle,
    files: Vec<String>,
    level: String,
    out_dir: Option<String>,
) -> Result<Vec<pdftools::CompressRes>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        pdftools::compress(&files, &level, out_dir.as_deref(), &|i, n| {
            let _ = app.emit("pdf-progress", json!({ "i": i, "n": n }));
        })
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn impose_sizes(path: String) -> Result<impose::SizesOut, String> {
    tauri::async_runtime::spawn_blocking(move || impose::sizes(&path))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn impose_plan(sizes: Vec<[f64; 2]>, opts: impose::Opts, limit: usize) -> Result<impose::Plan, String> {
    impose::plan(sizes, &opts, limit)
}

#[tauri::command]
async fn pdf_impose(
    app: tauri::AppHandle,
    path: String,
    opts: impose::Opts,
    out: String,
) -> Result<impose::ImposeOut, String> {
    tauri::async_runtime::spawn_blocking(move || {
        impose::impose(&path, &opts, &out, &|i, n| {
            let _ = app.emit("pdf-progress", json!({ "i": i, "n": n }));
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pdf_arrange(
    path: String,
    pages: Vec<pages::PageSpec>,
    out: String,
) -> Result<pages::ArrangeOut, String> {
    tauri::async_runtime::spawn_blocking(move || pages::arrange(&path, &pages, &out))
        .await
        .map_err(|e| e.to_string())?
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            expand,
            preview,
            run,
            reveal,
            pdf_info,
            pdf_merge,
            pdf_split,
            pdf_compress,
            impose_sizes,
            impose_plan,
            pdf_impose,
            pdf_arrange
        ])
        .run(tauri::generate_context!())
        .expect("error while running Photo and PDF Toolkit");
}