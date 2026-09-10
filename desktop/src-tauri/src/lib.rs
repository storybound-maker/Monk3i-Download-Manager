use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;
use tauri::Emitter;

#[derive(Clone, Serialize)]
struct ResourceInfo {
    url: String,
    kind: String,
    content_type: Option<String>,
    filename: Option<String>,
    size: Option<u64>,
    final_url: String,
    status_code: u16,
}

#[derive(Clone, Serialize)]
struct DownloadProgress {
    id: String,
    url: String,
    filename: String,
    downloaded: u64,
    total: Option<u64>,
    percent: Option<f64>,
    speed: u64,
    status: String,
    path: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct YtDlpInfo {
    title: Option<String>,
    ext: Option<String>,
    duration: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
}

fn sanitize_filename(filename: &str) -> String {
    let cleaned: String = filename.chars().map(|c| match c {
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
        c if c.is_control() => '_',
        c => c,
    }).collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() { "download".to_string() } else { cleaned }
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segment = parsed.path_segments()?.filter(|value| !value.is_empty()).next_back()?;
    if segment.is_empty() { return None; }
    let decoded = percent_encoding::percent_decode_str(segment).decode_utf8().ok()?.to_string();
    let decoded = sanitize_filename(&decoded);
    if decoded == "download" { None } else { Some(decoded) }
}

fn filename_from_content_disposition(value: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let part = part.trim();
        if let Some(filename) = part.strip_prefix("filename*=UTF-8''") {
            let decoded = percent_encoding::percent_decode_str(filename).decode_utf8().ok()?.to_string();
            if !decoded.is_empty() { return Some(sanitize_filename(&decoded)); }
        }
        if let Some(filename) = part.strip_prefix("filename=") {
            let filename = filename.trim().trim_matches('"');
            if !filename.is_empty() { return Some(sanitize_filename(filename)); }
        }
    }
    None
}

fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/jpeg" => Some("jpg"), "image/png" => Some("png"), "image/gif" => Some("gif"),
        "image/webp" => Some("webp"), "image/svg+xml" => Some("svg"), "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"), "video/quicktime" => Some("mov"), "video/x-matroska" => Some("mkv"),
        "audio/mpeg" => Some("mp3"), "audio/mp4" => Some("m4a"), "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"), "application/pdf" => Some("pdf"), "application/zip" => Some("zip"),
        "application/gzip" => Some("gz"), "application/x-rar-compressed" => Some("rar"),
        "application/json" => Some("json"), "text/plain" => Some("txt"), _ => None,
    }
}

fn ensure_extension(filename: String, content_type: &str) -> String {
    if std::path::Path::new(&filename).extension().is_some() { return filename; }
    if let Some(extension) = extension_for_content_type(content_type) { return format!("{filename}.{extension}"); }
    filename
}

fn is_known_media_page(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    host == "youtube.com" || host.ends_with(".youtube.com") || host == "youtu.be" || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com" || host.ends_with(".youtube-nocookie.com")
        || host == "vimeo.com" || host.ends_with(".vimeo.com")
        || host == "dailymotion.com" || host.ends_with(".dailymotion.com")
        || host == "tiktok.com" || host.ends_with(".tiktok.com")
        || host == "instagram.com" || host.ends_with(".instagram.com")
        || host == "facebook.com" || host.ends_with(".facebook.com")
        || host == "soundcloud.com" || host.ends_with(".soundcloud.com")
}

fn resource_kind(content_type: Option<&str>, url: &reqwest::Url) -> &'static str {
    if is_known_media_page(url) { return "media_page"; }
    let mime = content_type.and_then(|value| value.split(';').next()).unwrap_or("").trim().to_ascii_lowercase();
    if mime == "text/html" || mime == "application/xhtml+xml" { return "webpage"; }
    if mime.starts_with("image/") { return "image"; }
    if mime.starts_with("video/") { return "video"; }
    if mime.starts_with("audio/") { return "audio"; }
    if mime.starts_with("text/") || mime == "application/json" || mime == "application/javascript" { return "webpage"; }
    let path = url.path().to_ascii_lowercase();
    if ["jpg", "jpeg", "png", "gif", "webp", "svg"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) { return "image"; }
    if ["mp4", "webm", "mov", "mkv", "avi"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) { return "video"; }
    if ["mp3", "m4a", "wav", "flac", "ogg"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) { return "audio"; }
    "file"
}

fn total_size_from_response(response: &reqwest::Response) -> Option<u64> {
    if let Some(value) = response.headers().get(reqwest::header::CONTENT_RANGE) {
        if let Ok(value) = value.to_str() {
            if let Some(total) = value.rsplit('/').next() {
                if total != "*" { if let Ok(total) = total.parse::<u64>() { return Some(total); } }
            }
        }
    }
    response.content_length()
}

fn available_path(directory: &PathBuf, filename: &str) -> PathBuf {
    let initial = directory.join(filename);
    if !initial.exists() { return initial; }
    let path = std::path::Path::new(filename);
    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("download");
    let extension = path.extension().and_then(|v| v.to_str());
    for index in 1..10000 {
        let candidate_name = match extension { Some(ext) => format!("{stem} ({index}).{ext}"), None => format!("{stem} ({index})") };
        let candidate = directory.join(candidate_name);
        if !candidate.exists() { return candidate; }
    }
    directory.join(format!("download-{}.bin", Instant::now().elapsed().as_nanos()))
}

fn filename_from_response(response: &reqwest::Response) -> Option<String> {
    if let Some(value) = response.headers().get(reqwest::header::CONTENT_DISPOSITION) {
        if let Ok(value) = value.to_str() { if let Some(filename) = filename_from_content_disposition(value) { return Some(filename); } }
    }
    filename_from_url(response.url().as_str())
}

fn yt_dlp_available() -> bool {
    Command::new("yt-dlp").arg("--version").output().map(|output| output.status.success()).unwrap_or(false)
}

fn inspect_media_with_yt_dlp(url: &str) -> Result<YtDlpInfo, String> {
    if !yt_dlp_available() {
        return Err("YouTube/media extraction requires yt-dlp. Install yt-dlp, restart Monk3i, and try again.".to_string());
    }
    let output = Command::new("yt-dlp")
        .args(["--dump-single-json", "--no-warnings", "--no-playlist", url])
        .output()
        .map_err(|error| format!("Could not start yt-dlp: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() { "yt-dlp could not inspect this media URL.".to_string() } else { message });
    }
    serde_json::from_slice::<YtDlpInfo>(&output.stdout).map_err(|error| format!("Could not read media information: {error}"))
}

#[tauri::command]
async fn inspect_url(url: String) -> Result<ResourceInfo, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Please enter a valid URL.".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" { return Err("Only HTTP and HTTPS URLs are supported.".to_string()); }

    if is_known_media_page(&parsed) {
        let info = inspect_media_with_yt_dlp(&url)?;
        let filename = info.title.map(|title| sanitize_filename(&title)).map(|title| {
            let ext = info.ext.clone().unwrap_or_else(|| "mp4".to_string());
            format!("{title}.{ext}")
        });
        return Ok(ResourceInfo {
            url: url.clone(),
            kind: "media_page".to_string(),
            content_type: None,
            filename,
            size: info.filesize.or(info.filesize_approx),
            final_url: url,
            status_code: 200,
        });
    }

    let client = reqwest::Client::builder().user_agent("Monk3i Download Manager/0.1").redirect(reqwest::redirect::Policy::limited(10)).build().map_err(|error| error.to_string())?;
    let response = match client.head(parsed.clone()).send().await {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => response,
        _ => client.get(parsed).header(reqwest::header::RANGE, "bytes=0-0").send().await.map_err(|error| error.to_string())?,
    };
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(ToString::to_string);
    let filename = filename_from_response(&response).map(|name| ensure_extension(name, content_type.as_deref().unwrap_or("")));
    let size = total_size_from_response(&response);
    let final_url = response.url().to_string();
    let kind = resource_kind(content_type.as_deref(), response.url()).to_string();
    Ok(ResourceInfo { url, kind, content_type, filename, size, final_url, status_code: response.status().as_u16() })
}

#[tauri::command]
async fn start_download(app: tauri::AppHandle, url: String) -> Result<String, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Please enter a valid URL.".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" { return Err("Only HTTP and HTTPS downloads are supported.".to_string()); }

    if is_known_media_page(&parsed) {
        if !yt_dlp_available() {
            return Err("YouTube/media extraction requires yt-dlp. Install yt-dlp, restart Monk3i, and try again.".to_string());
        }
        let id = format!("download-{}", Instant::now().elapsed().as_nanos());
        let directory = dirs::download_dir().or_else(dirs::home_dir).ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let template = directory.join("%(title)s.%(ext)s");
        let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url: url.clone(), filename: "Preparing media...".to_string(), downloaded: 0, total: None, percent: Some(0.0), speed: 0, status: "downloading".to_string(), path: None, error: None });
        let output = Command::new("yt-dlp")
            .args(["--no-playlist", "--newline", "-f", "bv*+ba/b", "--merge-output-format", "mp4", "-o"])
            .arg(template.to_string_lossy().to_string())
            .arg(&url)
            .output()
            .map_err(|error| format!("Could not start yt-dlp: {error}"))?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if message.is_empty() { "Media download failed.".to_string() } else { message };
            let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url, filename: "Media download".to_string(), downloaded: 0, total: None, percent: None, speed: 0, status: "error".to_string(), path: None, error: Some(message.clone()) });
            return Err(message);
        }
        let info = inspect_media_with_yt_dlp(&url).ok();
        let title = info.as_ref().and_then(|value| value.title.clone()).unwrap_or_else(|| "Downloaded media".to_string());
        let ext = info.as_ref().and_then(|value| value.ext.clone()).unwrap_or_else(|| "mp4".to_string());
        let filename = format!("{}.{}", sanitize_filename(&title), ext);
        let output_path = directory.join(&filename);
        let total = info.as_ref().and_then(|value| value.filesize.or(value.filesize_approx));
        let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url, filename, downloaded: total.unwrap_or(0), total, percent: Some(100.0), speed: 0, status: "completed".to_string(), path: Some(output_path.to_string_lossy().to_string()), error: None });
        return Ok(id);
    }

    let id = format!("download-{}", Instant::now().elapsed().as_nanos());
    let client = reqwest::Client::builder().user_agent("Monk3i Download Manager/0.1").redirect(reqwest::redirect::Policy::limited(10)).build().map_err(|error| error.to_string())?;
    let response = client.get(parsed).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?;
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("");
    let kind = resource_kind(Some(content_type), response.url());
    if kind == "webpage" || kind == "media_page" { return Err(if kind == "media_page" { "This is a media webpage. Media extraction is not enabled yet.".to_string() } else { "This URL points to a webpage, not a direct file.".to_string() }); }
    let filename = filename_from_response(&response).map(|name| ensure_extension(name, content_type)).unwrap_or_else(|| extension_for_content_type(content_type).map(|extension| format!("download.{extension}")).unwrap_or_else(|| "download.bin".to_string()));
    let directory = dirs::download_dir().or_else(dirs::home_dir).ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let output_path = available_path(&directory, &filename);
    let total = total_size_from_response(&response);
    let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url: url.clone(), filename: filename.clone(), downloaded: 0, total, percent: total.map(|_| 0.0), speed: 0, status: "downloading".to_string(), path: None, error: None });
    let mut file = tokio::fs::File::create(&output_path).await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;
    let started = Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await.map_err(|error| error.to_string())?;
        downloaded += chunk.len() as u64;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let speed = (downloaded as f64 / elapsed) as u64;
        let percent = total.map(|value| (downloaded as f64 / value as f64) * 100.0);
        let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url: url.clone(), filename: filename.clone(), downloaded, total, percent, speed, status: "downloading".to_string(), path: None, error: None });
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await.map_err(|error| error.to_string())?;
    let path_string = output_path.to_string_lossy().to_string();
    let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), url, filename, downloaded, total, percent: Some(100.0), speed: 0, status: "completed".to_string(), path: Some(path_string), error: None });
    Ok(id)
}

#[tauri::command]
fn greet(name: &str) -> String { format!("Hello, {}! You've been greeted from Rust!", name) }

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default().plugin(tauri_plugin_opener::init()).invoke_handler(tauri::generate_handler![greet, inspect_url, start_download]).run(tauri::generate_context!()).expect("error while running tauri application");
}
