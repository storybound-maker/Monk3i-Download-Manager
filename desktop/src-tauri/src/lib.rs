use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tauri::Emitter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
    id: Option<String>,
    title: Option<String>,
    ext: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
}

struct Control {
    paused: AtomicBool,
    cancelled: AtomicBool,
    child: Mutex<Option<tokio::process::Child>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CONTROLS: std::sync::OnceLock<Mutex<HashMap<String, Arc<Control>>>> =
    std::sync::OnceLock::new();

fn controls() -> &'static Mutex<HashMap<String, Arc<Control>>> {
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_id() -> String {
    format!("download-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

fn get_control(id: &str) -> Option<Arc<Control>> {
    controls().lock().ok()?.get(id).cloned()
}

fn remove_control(id: &str) {
    if let Ok(mut map) = controls().lock() {
        map.remove(id);
    }
}

fn emit(app: &tauri::AppHandle, progress: DownloadProgress) {
    let _ = app.emit("download-progress", progress);
}

fn sanitize_filename(filename: &str) -> String {
    let mut cleaned: String = filename
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    cleaned = cleaned.trim().trim_matches('.').to_string();

    // Windows also rejects these reserved device names even when they have no extension.
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        cleaned.insert(0, '_');
    }

    if cleaned.is_empty() {
        "download".into()
    } else {
        cleaned
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

// Gopeed uses a bounded, UTF-8-safe filename before touching the filesystem.
// Monk3i applies the same principle to media titles, while preserving extensions.
fn safe_media_filename(title: &str, extension: &str) -> String {
    let title = sanitize_filename(title);
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    let extension = if extension.is_empty() {
        "bin".to_string()
    } else {
        sanitize_filename(&extension)
    };

    let suffix = format!(".{extension}");
    let max_bytes = 100usize;
    let base_limit = max_bytes.saturating_sub(suffix.len()).max(1);
    let base = truncate_utf8(&title, base_limit);
    format!("{base}{suffix}")
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segment = parsed
        .path_segments()?
        .filter(|value| !value.is_empty())
        .next_back()?;
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

fn extension_for_content_type(value: &str) -> Option<&'static str> {
    let media_type = value
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
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
    if Path::new(&filename).extension().is_some() {
        filename
    } else if let Some(extension) = extension_for_content_type(content_type) {
        format!("{filename}.{extension}")
    } else {
        filename
    }
}

fn is_known_media_page(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    [
        "youtube.com",
        "youtu.be",
        "youtube-nocookie.com",
        "vimeo.com",
        "dailymotion.com",
        "tiktok.com",
        "instagram.com",
        "facebook.com",
        "soundcloud.com",
    ]
    .iter()
    .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn resource_kind(content_type: Option<&str>, url: &reqwest::Url) -> &'static str {
    if is_known_media_page(url) {
        return "media_page";
    }

    let media_type = content_type
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if media_type == "text/html" || media_type == "application/xhtml+xml" {
        return "webpage";
    }
    if media_type.starts_with("image/") {
        return "image";
    }
    if media_type.starts_with("video/") {
        return "video";
    }
    if media_type.starts_with("audio/") {
        return "audio";
    }
    if media_type.starts_with("text/")
        || media_type == "application/json"
        || media_type == "application/javascript"
    {
        return "webpage";
    }

    let path = url.path().to_ascii_lowercase();
    if ["jpg", "jpeg", "png", "gif", "webp", "svg"]
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
    {
        return "image";
    }
    if ["mp4", "webm", "mov", "mkv", "avi"]
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
    {
        return "video";
    }
    if ["mp3", "m4a", "wav", "flac", "ogg"]
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
    {
        return "audio";
    }
    "file"
}

fn total_size_from_response(response: &reqwest::Response) -> Option<u64> {
    if let Some(value) = response.headers().get(reqwest::header::CONTENT_RANGE) {
        if let Ok(value) = value.to_str() {
            if let Some(total) = value.rsplit('/').next() {
                if total != "*" {
                    if let Ok(total) = total.parse() {
                        return Some(total);
                    }
                }
            }
        }
    }
    response.content_length()
}

fn available_path(directory: &Path, filename: &str) -> PathBuf {
    let candidate = directory.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());

    for index in 1..10000 {
        let name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    directory.join("download.bin")
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

fn yt_dlp_available() -> bool {
    Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn inspect_media_with_yt_dlp(url: &str) -> Result<YtDlpInfo, String> {
    if !yt_dlp_available() {
        return Err(
            "YouTube/media extraction requires yt-dlp. Install yt-dlp, restart Monk3i, and try again."
                .into(),
        );
    }

    let output = Command::new("yt-dlp")
        .env("PYTHONIOENCODING", "utf-8:replace")
        .env("PYTHONUTF8", "1")
        .args(["--dump-single-json", "--no-warnings", "--no-playlist", url])
        .output()
        .map_err(|error| format!("Could not start yt-dlp: {error}"))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            "yt-dlp could not inspect this media URL.".into()
        } else {
            message
        });
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Could not read media information: {error}"))
}

#[tauri::command]
async fn inspect_url(url: String) -> Result<ResourceInfo, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| "Please enter a valid URL.".to_string())?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Only HTTP and HTTPS URLs are supported.".into());
    }

    if is_known_media_page(&parsed) {
        let info = inspect_media_with_yt_dlp(&url)?;
        let filename = info.title.clone().map(|title| {
            let extension = info.ext.clone().unwrap_or_else(|| "mp4".into());
            safe_media_filename(&title, &extension)
        });

        return Ok(ResourceInfo {
            url: url.clone(),
            kind: "media_page".into(),
            content_type: None,
            filename,
            size: info.filesize.or(info.filesize_approx),
            final_url: url,
            status_code: 200,
        });
    }

    let client = reqwest::Client::builder()
        .user_agent("Monk3i Download Manager/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|error| error.to_string())?;

    let response = match client.head(parsed.clone()).send().await {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => {
            response
        }
        _ => client
            .get(parsed)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|error| error.to_string())?,
    };

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let filename = filename_from_response(&response)
        .map(|name| ensure_extension(name, content_type.as_deref().unwrap_or("")));
    let size = total_size_from_response(&response);
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

fn parse_speed(value: &str) -> u64 {
    let value = value.trim().trim_end_matches("/s").trim();
    if let Ok(number) = value.parse::<f64>() {
        return number.max(0.0) as u64;
    }

    let lower = value.to_ascii_lowercase();
    let units = [
        ("gib", 1024.0 * 1024.0 * 1024.0),
        ("mib", 1024.0 * 1024.0),
        ("kib", 1024.0),
        ("gb", 1000.0 * 1000.0 * 1000.0),
        ("mb", 1000.0 * 1000.0),
        ("kb", 1000.0),
    ];

    for (unit, multiplier) in units {
        if let Some(number) = lower.strip_suffix(unit) {
            if let Ok(number) = number.trim().parse::<f64>() {
                return (number * multiplier).max(0.0) as u64;
            }
        }
    }
    0
}

fn parse_progress(line: &str) -> Option<(u64, Option<u64>, u64, Option<f64>)> {
    let marker = line.find("download:")?;
    let data = &line[marker + 9..];
    let parts: Vec<&str> = data.split('|').collect();
    if parts.len() < 5 {
        return None;
    }

    let downloaded = parts[0].trim().parse().ok()?;
    let mut total = parts[1]
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0);
    if total.is_none() {
        total = parts[2]
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0);
    }

    let speed = parse_speed(parts[3]);
    let percent = parts[4]
        .trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .ok();

    if total.is_none() {
        if let Some(percent) = percent {
            if percent > 0.0 && percent <= 100.0 {
                total = Some((downloaded as f64 * 100.0 / percent) as u64);
            }
        }
    }

    Some((downloaded, total, speed, percent))
}

fn emit_progress(
    app: &tauri::AppHandle,
    id: &str,
    url: &str,
    filename: &str,
    downloaded: u64,
    total: Option<u64>,
    percent: Option<f64>,
    speed: u64,
    status: &str,
    path: Option<String>,
    error: Option<String>,
) {
    emit(
        app,
        DownloadProgress {
            id: id.into(),
            url: url.into(),
            filename: filename.into(),
            downloaded,
            total,
            percent,
            speed,
            status: status.into(),
            path,
            error,
        },
    );
}

async fn wait_if_paused(control: &Arc<Control>) -> bool {
    while control.paused.load(Ordering::Relaxed)
        && !control.cancelled.load(Ordering::Relaxed)
    {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    !control.cancelled.load(Ordering::Relaxed)
}

fn kill_media_child(control: &Arc<Control>) {
    if let Ok(mut slot) = control.child.lock() {
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn path_from_yt_dlp_output(line: &str, temp_dir: &Path) -> Option<PathBuf> {
    let candidate = line.trim().trim_matches('"');
    if candidate.is_empty() {
        return None;
    }

    let candidate_path = PathBuf::from(candidate);
    if candidate_path.is_file() && candidate_path.starts_with(temp_dir) {
        return Some(candidate_path);
    }

    None
}

async fn find_media_output(temp_dir: &Path, media_id: &str) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(temp_dir).await.ok()?;
    let mut fallback = None;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let filename = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
        if filename.ends_with(".part") || filename.ends_with(".ytdl") {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if stem == media_id {
            return Some(path);
        }
        if fallback.is_none() && !filename.starts_with('.') {
            fallback = Some(path);
        }
    }

    fallback
}

async fn run_media(
    app: tauri::AppHandle,
    id: String,
    url: String,
    directory: PathBuf,
    control: Arc<Control>,
) {
    let info = match inspect_media_with_yt_dlp(&url) {
        Ok(info) => info,
        Err(error) => {
            emit_progress(
                &app,
                &id,
                &url,
                "Media download",
                0,
                None,
                None,
                0,
                "error",
                None,
                Some(error),
            );
            remove_control(&id);
            return;
        }
    };

    let media_id = info.id.clone().unwrap_or_else(|| id.clone());
    let title = info
        .title
        .clone()
        .unwrap_or_else(|| "Downloaded media".into());
    let temp_dir = directory.join(format!(".monk3i-{id}"));

    if let Err(error) = tokio::fs::create_dir_all(&temp_dir).await {
        emit_progress(
            &app,
            &id,
            &url,
            "Media download",
            0,
            None,
            None,
            0,
            "error",
            None,
            Some(format!("Could not create temporary download folder: {error}")),
        );
        remove_control(&id);
        return;
    }

    // The filesystem never sees the YouTube title. yt-dlp writes to an ID-based
    // temporary path and Monk3i applies the final title only after success.
    let template = temp_dir.join("%(id)s.%(ext)s");
    let mut error_text = String::new();

    loop {
        if control.cancelled.load(Ordering::Relaxed) {
            kill_media_child(&control);
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            emit_progress(
                &app,
                &id,
                &url,
                "Media download",
                0,
                None,
                None,
                0,
                "cancelled",
                None,
                None,
            );
            remove_control(&id);
            return;
        }

        if control.paused.load(Ordering::Relaxed) {
            emit_progress(
                &app,
                &id,
                &url,
                "Media download",
                0,
                None,
                None,
                0,
                "paused",
                None,
                None,
            );
            if !wait_if_paused(&control).await {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                emit_progress(
                    &app,
                    &id,
                    &url,
                    "Media download",
                    0,
                    None,
                    None,
                    0,
                    "cancelled",
                    None,
                    None,
                );
                remove_control(&id);
                return;
            }
        }

        // Gopeed treats pause/resume as a downloader state transition rather
        // than changing the final destination. For yt-dlp, killing the child
        // and restarting with --continue gives us the same durable behavior.
        let args = [
            "--no-playlist",
            "--newline",
            "--progress",
            "--progress-delta",
            "0.25",
            "--progress-template",
            "download:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress._percent_str)s",
            "--print",
            "after_move:filepath",
            "--continue",
            "-f",
            "bv*+ba/b",
            "--merge-output-format",
            "mp4",
            "-o",
        ];

        let child = match tokio::process::Command::new("yt-dlp")
            .env("PYTHONIOENCODING", "utf-8:replace")
            .env("PYTHONUTF8", "1")
            .args(args)
            .arg(template.to_string_lossy().to_string())
            .arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                emit_progress(
                    &app,
                    &id,
                    &url,
                    "Media download",
                    0,
                    None,
                    None,
                    0,
                    "error",
                    None,
                    Some(format!("Could not start yt-dlp: {error}")),
                );
                remove_control(&id);
                return;
            }
        };

        if let Ok(mut slot) = control.child.lock() {
            *slot = Some(child);
        }

        let (stdout, stderr) = {
            let mut slot = control.child.lock().unwrap();
            let child = slot.as_mut().unwrap();
            (child.stdout.take(), child.stderr.take())
        };

        let mut stdout = stdout.map(|stream| tokio::io::BufReader::new(stream).lines());
        let mut stderr = stderr.map(|stream| tokio::io::BufReader::new(stream).lines());
        let mut printed_path: Option<PathBuf> = None;
        let mut child_was_killed = false;

        loop {
            if control.cancelled.load(Ordering::Relaxed) {
                child_was_killed = true;
                kill_media_child(&control);
            } else if control.paused.load(Ordering::Relaxed) {
                child_was_killed = true;
                kill_media_child(&control);
            }

            if stdout.is_none() && stderr.is_none() {
                break;
            }

            tokio::select! {
                result = async {
                    if let Some(lines) = &mut stdout {
                        lines.next_line().await
                    } else {
                        Ok(None)
                    }
                } => {
                    match result {
                        Ok(Some(line)) => {
                            if let Some(path) = path_from_yt_dlp_output(&line, &temp_dir) {
                                printed_path = Some(path);
                            }
                            if let Some((downloaded, total, speed, percent)) = parse_progress(&line) {
                                emit_progress(
                                    &app,
                                    &id,
                                    &url,
                                    "Downloading media...",
                                    downloaded,
                                    total,
                                    percent,
                                    speed,
                                    "downloading",
                                    None,
                                    None,
                                );
                            }
                        }
                        _ => stdout = None,
                    }
                }
                result = async {
                    if let Some(lines) = &mut stderr {
                        lines.next_line().await
                    } else {
                        Ok(None)
                    }
                } => {
                    match result {
                        Ok(Some(line)) => {
                            if let Some((downloaded, total, speed, percent)) = parse_progress(&line) {
                                emit_progress(
                                    &app,
                                    &id,
                                    &url,
                                    "Downloading media...",
                                    downloaded,
                                    total,
                                    percent,
                                    speed,
                                    "downloading",
                                    None,
                                    None,
                                );
                            } else if !line.trim().is_empty() && !line.contains("[download]") {
                                error_text.push_str(&line);
                                error_text.push('\n');
                            }
                        }
                        _ => stderr = None,
                    }
                }
            }
        }

        let child_to_wait = {
            let mut slot = control.child.lock().unwrap();
            slot.take()
        };
        let exit_status = if let Some(mut child) = child_to_wait {
            child.wait().await.ok()
        } else {
            None
        };

        if control.cancelled.load(Ordering::Relaxed) {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            emit_progress(
                &app,
                &id,
                &url,
                "Media download",
                0,
                None,
                None,
                0,
                "cancelled",
                None,
                None,
            );
            remove_control(&id);
            return;
        }

        if control.paused.load(Ordering::Relaxed) || child_was_killed {
            if control.paused.load(Ordering::Relaxed) {
                emit_progress(
                    &app,
                    &id,
                    &url,
                    "Media download",
                    0,
                    None,
                    None,
                    0,
                    "paused",
                    None,
                    None,
                );
                if !wait_if_paused(&control).await {
                    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                    emit_progress(
                        &app,
                        &id,
                        &url,
                        "Media download",
                        0,
                        None,
                        None,
                        0,
                        "cancelled",
                        None,
                        None,
                    );
                    remove_control(&id);
                    return;
                }
            }
            continue;
        }

        if exit_status.map(|status| status.success()).unwrap_or(false) {
            let source_path = match printed_path.or_else(|| {
                // yt-dlp normally prints the post-processing destination. The
                // directory scan is a fallback for older yt-dlp builds.
                futures_util::future::block_on(find_media_output(&temp_dir, &media_id))
            }) {
                Some(path) => path,
                None => {
                    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                    emit_progress(
                        &app,
                        &id,
                        &url,
                        "Media download",
                        0,
                        None,
                        None,
                        0,
                        "error",
                        None,
                        Some("yt-dlp completed, but Monk3i could not locate the downloaded media file.".into()),
                    );
                    remove_control(&id);
                    return;
                }
            };

            let actual_extension = source_path
                .extension()
                .and_then(|value| value.to_str())
                .or(info.ext.as_deref())
                .unwrap_or("mp4");
            let filename = safe_media_filename(&title, actual_extension);
            let final_path = available_path(&directory, &filename);

            if let Err(error) = tokio::fs::rename(&source_path, &final_path).await {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                emit_progress(
                    &app,
                    &id,
                    &url,
                    &filename,
                    0,
                    None,
                    None,
                    0,
                    "error",
                    None,
                    Some(format!("Could not finalize downloaded media: {error}")),
                );
                remove_control(&id);
                return;
            }

            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            let total = info.filesize.or(info.filesize_approx);
            emit_progress(
                &app,
                &id,
                &url,
                final_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Downloaded media"),
                total.unwrap_or(0),
                total,
                Some(100.0),
                0,
                "completed",
                Some(final_path.to_string_lossy().to_string()),
                None,
            );
            remove_control(&id);
            return;
        }

        let message = if error_text.trim().is_empty() {
            "Media download failed.".to_string()
        } else {
            error_text.trim().to_string()
        };
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        emit_progress(
            &app,
            &id,
            &url,
            "Media download",
            0,
            None,
            None,
            0,
            "error",
            None,
            Some(message),
        );
        remove_control(&id);
        return;
    }
}

#[tauri::command]
async fn start_download(app: tauri::AppHandle, url: String) -> Result<String, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| "Please enter a valid URL.".to_string())?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Only HTTP and HTTPS URLs are supported.".into());
    }

    let id = new_id();
    let control = Arc::new(Control {
        paused: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        child: Mutex::new(None),
    });
    controls().lock().unwrap().insert(id.clone(), control.clone());

    if is_known_media_page(&parsed) {
        if !yt_dlp_available() {
            remove_control(&id);
            return Err(
                "YouTube/media extraction requires yt-dlp. Install yt-dlp, restart Monk3i, and try again."
                    .into(),
            );
        }

        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;

        emit_progress(
            &app,
            &id,
            &url,
            "Preparing media...",
            0,
            None,
            Some(0.0),
            0,
            "downloading",
            None,
            None,
        );
        tokio::spawn(run_media(app, id.clone(), url, directory, control));
        return Ok(id);
    }

    let client = reqwest::Client::builder()
        .user_agent("Monk3i Download Manager/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
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
        .unwrap_or("")
        .to_string();
    let kind = resource_kind(Some(&content_type), response.url());

    if kind == "webpage" || kind == "media_page" {
        remove_control(&id);
        return Err("This URL points to a webpage, not a direct file.".into());
    }

    let filename = filename_from_response(&response)
        .map(|name| ensure_extension(name, &content_type))
        .unwrap_or_else(|| {
            extension_for_content_type(&content_type)
                .map(|extension| format!("download.{extension}"))
                .unwrap_or_else(|| "download.bin".into())
        });

    let directory = dirs::download_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;

    let path = available_path(&directory, &filename);
    let total = total_size_from_response(&response);

    emit_progress(
        &app,
        &id,
        &url,
        &filename,
        0,
        total,
        total.map(|_| 0.0),
        0,
        "downloading",
        Some(path.to_string_lossy().to_string()),
        None,
    );

    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    let started = Instant::now();

    while let Some(chunk) = stream.next().await {
        if control.cancelled.load(Ordering::Relaxed) {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            emit_progress(
                &app,
                &id,
                &url,
                &filename,
                downloaded,
                total,
                total.map(|size| downloaded as f64 * 100.0 / size as f64),
                0,
                "cancelled",
                None,
                None,
            );
            remove_control(&id);
            return Ok(id);
        }

        if control.paused.load(Ordering::Relaxed) {
            emit_progress(
                &app,
                &id,
                &url,
                &filename,
                downloaded,
                total,
                total.map(|size| downloaded as f64 * 100.0 / size as f64),
                0,
                "paused",
                Some(path.to_string_lossy().to_string()),
                None,
            );
            if !wait_if_paused(&control).await {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                emit_progress(
                    &app,
                    &id,
                    &url,
                    &filename,
                    downloaded,
                    total,
                    total.map(|size| downloaded as f64 * 100.0 / size as f64),
                    0,
                    "cancelled",
                    None,
                    None,
                );
                remove_control(&id);
                return Ok(id);
            }
        }

        let chunk = chunk.map_err(|error| error.to_string())?;
        file.write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        downloaded += chunk.len() as u64;

        let speed = (downloaded as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64;
        let percent = total.map(|size| downloaded as f64 * 100.0 / size as f64);
        emit_progress(
            &app,
            &id,
            &url,
            &filename,
            downloaded,
            total,
            percent,
            speed,
            "downloading",
            Some(path.to_string_lossy().to_string()),
            None,
        );
    }

    file.flush().await.map_err(|error| error.to_string())?;
    emit_progress(
        &app,
        &id,
        &url,
        &filename,
        downloaded,
        total,
        Some(100.0),
        0,
        "completed",
        Some(path.to_string_lossy().to_string()),
        None,
    );
    remove_control(&id);
    Ok(id)
}

#[tauri::command]
fn pause_download(id: String) -> Result<(), String> {
    let control = get_control(&id).ok_or_else(|| "Download not found.".to_string())?;
    if control.cancelled.load(Ordering::Relaxed) {
        return Err("Download is being cancelled.".into());
    }
    control.paused.store(true, Ordering::Relaxed);
    kill_media_child(&control);
    Ok(())
}

#[tauri::command]
fn resume_download(id: String) -> Result<(), String> {
    let control = get_control(&id).ok_or_else(|| "Download not found.".to_string())?;
    if control.cancelled.load(Ordering::Relaxed) {
        return Err("Download has been cancelled.".into());
    }
    control.paused.store(false, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn cancel_download(id: String) -> Result<(), String> {
    let control = get_control(&id).ok_or_else(|| "Download not found.".to_string())?;
    control.cancelled.store(true, Ordering::Relaxed);
    control.paused.store(false, Ordering::Relaxed);
    kill_media_child(&control);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            inspect_url,
            start_download,
            pause_download,
            resume_download,
            cancel_download
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
