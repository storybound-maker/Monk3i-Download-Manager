use futures_util::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
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

fn sanitize_filename(filename: &str) -> String {
    let cleaned: String = filename
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segment = parsed
        .path_segments()?
        .filter(|value| !value.is_empty())
        .next_back()?;

    if segment.is_empty() {
        return None;
    }

    let decoded = percent_encoding::percent_decode_str(segment)
        .decode_utf8()
        .ok()?
        .to_string();
    let decoded = sanitize_filename(&decoded);

    if decoded == "download" {
        None
    } else {
        Some(decoded)
    }
}

fn filename_from_content_disposition(value: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let part = part.trim();
        if let Some(filename) = part.strip_prefix("filename*=UTF-8''") {
            let decoded = percent_encoding::percent_decode_str(filename)
                .decode_utf8()
                .ok()?
                .to_string();
            if !decoded.is_empty() {
                return Some(sanitize_filename(&decoded));
            }
        }
        if let Some(filename) = part.strip_prefix("filename=") {
            let filename = filename.trim().trim_matches('"');
            if !filename.is_empty() {
                return Some(sanitize_filename(filename));
            }
        }
    }
    None
}

fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/quicktime" => Some("mov"),
        "video/x-matroska" => Some("mkv"),
        "audio/mpeg" => Some("mp3"),
        "audio/mp4" => Some("m4a"),
        "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        "application/gzip" => Some("gz"),
        "application/x-rar-compressed" => Some("rar"),
        "application/json" => Some("json"),
        "text/plain" => Some("txt"),
        _ => None,
    }
}

fn ensure_extension(filename: String, content_type: &str) -> String {
    if std::path::Path::new(&filename).extension().is_some() {
        return filename;
    }

    if let Some(extension) = extension_for_content_type(content_type) {
        return format!("{filename}.{extension}");
    }

    filename
}

fn resource_kind(content_type: Option<&str>, url: &reqwest::Url) -> &'static str {
    let mime = content_type
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if mime == "text/html" || mime == "application/xhtml+xml" {
        return "webpage";
    }
    if mime.starts_with("image/") {
        return "image";
    }
    if mime.starts_with("video/") {
        return "video";
    }
    if mime.starts_with("audio/") {
        return "audio";
    }
    if mime.starts_with("text/") || mime == "application/json" || mime == "application/javascript" {
        return "webpage";
    }

    let path = url.path().to_ascii_lowercase();
    if ["jpg", "jpeg", "png", "gif", "webp", "svg"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) {
        return "image";
    }
    if ["mp4", "webm", "mov", "mkv", "avi"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) {
        return "video";
    }
    if ["mp3", "m4a", "wav", "flac", "ogg"].iter().any(|ext| path.ends_with(&format!(".{ext}"))) {
        return "audio";
    }

    "file"
}

fn available_path(directory: &PathBuf, filename: &str) -> PathBuf {
    let initial = directory.join(filename);
    if !initial.exists() {
        return initial;
    }

    let path = std::path::Path::new(filename);
    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("download");
    let extension = path.extension().and_then(|v| v.to_str());

    for index in 1..10000 {
        let candidate_name = match extension {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = directory.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    directory.join(format!("download-{}.bin", Instant::now().elapsed().as_nanos()))
}

fn filename_from_response(response: &reqwest::Response) -> Option<String> {
    if let Some(value) = response.headers().get(reqwest::header::CONTENT_DISPOSITION) {
        if let Ok(value) = value.to_str() {
            if let Some(filename) = filename_from_content_disposition(value) {
                return Some(filename);
            }
        }
    }

    filename_from_url(response.url().as_str())
}

#[tauri::command]
async fn inspect_url(url: String) -> Result<ResourceInfo, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Please enter a valid URL.".to_string())?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only HTTP and HTTPS URLs are supported.".to_string());
    }

    let client = reqwest::Client::builder()
        .user_agent("Monk3i Download Manager/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|error| error.to_string())?;

    let response = client
        .head(parsed.clone())
        .send()
        .await
        .map_err(|error| error.to_string())?;

    let response = if response.status().is_success() || response.status().is_redirection() {
        response
    } else {
        client
            .get(parsed)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|error| error.to_string())?
    };

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let filename = filename_from_response(&response).map(|name| ensure_extension(name, content_type.as_deref().unwrap_or("")));
    let size = response.content_length();
    let final_url = response.url().to_string();
    let kind = resource_kind(content_type.as_deref(), response.url()).to_string();

    Ok(ResourceInfo {
        url,
        kind,
        content_type,
        filename,
        size,
        final_url,
        status_code: response.status().as_u16(),
    })
}

#[tauri::command]
async fn start_download(app: tauri::AppHandle, url: String) -> Result<String, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Please enter a valid URL.".to_string())?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only HTTP and HTTPS downloads are supported.".to_string());
    }

    let id = format!("download-{}", Instant::now().elapsed().as_nanos());
    let client = reqwest::Client::builder()
        .user_agent("Monk3i Download Manager/0.1")
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    let filename = filename_from_response(&response)
        .map(|name| ensure_extension(name, content_type))
        .unwrap_or_else(|| {
            extension_for_content_type(content_type)
                .map(|extension| format!("download.{extension}"))
                .unwrap_or_else(|| "download.bin".to_string())
        });

    let directory = dirs::download_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Could not find a Downloads folder.".to_string())?;

    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let output_path = available_path(&directory, &filename);
    let total = response.content_length();

    let _ = app.emit(
        "download-progress",
        DownloadProgress {
            id: id.clone(),
            url: url.clone(),
            filename: filename.clone(),
            downloaded: 0,
            total,
            percent: total.map(|_| 0.0),
            speed: 0,
            status: "downloading".to_string(),
            path: None,
            error: None,
        },
    );

    let mut file = tokio::fs::File::create(&output_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;
    let started = Instant::now();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|error| error.to_string())?;
        downloaded += chunk.len() as u64;

        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let speed = (downloaded as f64 / elapsed) as u64;
        let percent = total.map(|value| (downloaded as f64 / value as f64) * 100.0);

        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                id: id.clone(),
                url: url.clone(),
                filename: filename.clone(),
                downloaded,
                total,
                percent,
                speed,
                status: "downloading".to_string(),
                path: None,
                error: None,
            },
        );
    }

    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|error| error.to_string())?;

    let path_string = output_path.to_string_lossy().to_string();
    let _ = app.emit(
        "download-progress",
        DownloadProgress {
            id: id.clone(),
            url,
            filename,
            downloaded,
            total,
            percent: Some(100.0),
            speed: 0,
            status: "completed".to_string(),
            path: Some(path_string),
            error: None,
        },
    );

    Ok(id)
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet, inspect_url, start_download])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
