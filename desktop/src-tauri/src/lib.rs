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
    time::{Duration, Instant},
};
use tauri::Emitter;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;

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
#[derive(Clone, Deserialize, Default)]
struct DownloadOptions {
    headers: Option<Vec<String>>,
    proxy: Option<String>,
    speed_limit: Option<u64>,
}
#[derive(Deserialize)]
struct YtDlpFormatInfo {
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
}
#[derive(Deserialize)]
struct YtDlpInfo {
    id: Option<String>,
    title: Option<String>,
    ext: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    requested_formats: Option<Vec<YtDlpFormatInfo>>,
}
struct Control {
    url: String,
    paused: AtomicBool,
    cancelled: AtomicBool,
    speed_limit: AtomicU64,
    child: Mutex<Option<tokio::process::Child>>,
    child_pid: AtomicU64,
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CONTROLS: std::sync::OnceLock<Mutex<HashMap<String, Arc<Control>>>> =
    std::sync::OnceLock::new();
static STOP_ALL: AtomicBool = AtomicBool::new(false);
static ACTIVE_URLS: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
fn active_urls() -> &'static Mutex<HashMap<String, String>> {
    ACTIVE_URLS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn controls() -> &'static Mutex<HashMap<String, Arc<Control>>> {
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}
async fn stop_all_active() {
    let targets: Vec<Arc<Control>> = controls().lock().map(|m| m.values().cloned().collect()).unwrap_or_default();
    for c in &targets {
        c.cancelled.store(true, Ordering::SeqCst);
        c.paused.store(false, Ordering::SeqCst);
    }
    futures_util::future::join_all(targets.iter().map(kill_tree)).await;
}
fn new_id() -> String {
    format!("download-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}
fn get_control(id: &str) -> Option<Arc<Control>> {
    controls().lock().ok()?.get(id).cloned()
}
fn remove_control(id: &str) {
    let removed = controls().lock().ok().and_then(|mut m| m.remove(id));
    if let Some(c) = removed {
        if let Ok(mut m) = active_urls().lock() {
            if m.get(&c.url).map(|v| v == id).unwrap_or(false) {
                m.remove(&c.url);
            }
        }
    }
}
fn emit(app: &tauri::AppHandle, p: DownloadProgress) {
    let _ = app.emit("download-progress", p);
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
            status: status.into(),
            speed,
            path,
            error,
        },
    )
}
fn sanitize_filename(filename: &str) -> String {
    let mut c: String = filename
        .chars()
        .map(|x| match x {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            x if x.is_control() => '_',
            x => x,
        })
        .collect();
    c = c.trim().trim_matches('.').to_string();
    let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
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
        c.insert(0, '_')
    }
    if c.is_empty() {
        "download".into()
    } else {
        c
    }
}
fn truncate_utf8(v: &str, n: usize) -> String {
    if v.len() <= n {
        return v.into();
    }
    let mut e = n;
    while e > 0 && !v.is_char_boundary(e) {
        e -= 1;
    }
    v[..e].into()
}
fn safe_media_filename(title: &str, ext: &str) -> String {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    let e = if e.is_empty() {
        "bin".into()
    } else {
        sanitize_filename(&e)
    };
    let s = format!(".{e}");
    format!(
        "{}{}",
        truncate_utf8(
            &sanitize_filename(title),
            100usize.saturating_sub(s.len()).max(1)
        ),
        s
    )
}
fn filename_from_url(url: &str) -> Option<String> {
    let p = reqwest::Url::parse(url).ok()?;
    let s = p.path_segments()?.filter(|v| !v.is_empty()).next_back()?;
    let d = percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .ok()?
        .to_string();
    let d = sanitize_filename(&d);
    if d == "download" {
        None
    } else {
        Some(d)
    }
}
fn filename_from_content_disposition(v: &str) -> Option<String> {
    for p in v.split(';').skip(1) {
        let p = p.trim();
        if let Some(f) = p.strip_prefix("filename*=UTF-8''") {
            let d = percent_encoding::percent_decode_str(f)
                .decode_utf8()
                .ok()?
                .to_string();
            if !d.is_empty() {
                return Some(sanitize_filename(&d));
            }
        }
        if let Some(f) = p.strip_prefix("filename=") {
            let f = f.trim().trim_matches('"');
            if !f.is_empty() {
                return Some(sanitize_filename(f));
            }
        }
    }
    None
}
fn extension_for_content_type(v: &str) -> Option<&'static str> {
    match v.split(';').next()?.trim().to_ascii_lowercase().as_str() {
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
fn ensure_extension(f: String, ct: &str) -> String {
    if Path::new(&f).extension().is_some() {
        f
    } else if let Some(e) = extension_for_content_type(ct) {
        format!("{f}.{e}")
    } else {
        f
    }
}
fn is_known_media_page(u: &reqwest::Url) -> bool {
    let h = u.host_str().unwrap_or("").to_ascii_lowercase();
    let h = h.strip_prefix("www.").unwrap_or(&h);
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
    .any(|d| h == *d || h.ends_with(&format!(".{d}")))
}
fn resource_kind(ct: Option<&str>, u: &reqwest::Url) -> &'static str {
    if is_known_media_page(u) {
        return "media_page";
    }
    let m = ct
        .and_then(|v| v.split(';').next())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if m == "text/html"
        || m == "application/xhtml+xml"
        || m.starts_with("text/")
        || m == "application/json"
        || m == "application/javascript"
    {
        return "webpage";
    }
    if m.starts_with("image/") {
        return "image";
    }
    if m.starts_with("video/") {
        return "video";
    }
    if m.starts_with("audio/") {
        return "audio";
    }
    let p = u.path().to_ascii_lowercase();
    if ["jpg", "jpeg", "png", "gif", "webp", "svg"]
        .iter()
        .any(|e| p.ends_with(&format!(".{e}")))
    {
        return "image";
    }
    if ["mp4", "webm", "mov", "mkv", "avi"]
        .iter()
        .any(|e| p.ends_with(&format!(".{e}")))
    {
        return "video";
    }
    if ["mp3", "m4a", "wav", "flac", "ogg"]
        .iter()
        .any(|e| p.ends_with(&format!(".{e}")))
    {
        return "audio";
    }
    "file"
}
fn total_size(r: &reqwest::Response) -> Option<u64> {
    r.headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| if v == "*" { None } else { v.parse().ok() })
        .or_else(|| r.content_length())
}
fn available_path(d: &Path, f: &str) -> PathBuf {
    let p = d.join(f);
    if !p.exists() {
        return p;
    }
    let x = Path::new(f);
    let s = x.file_stem().and_then(|v| v.to_str()).unwrap_or("download");
    let e = x.extension().and_then(|v| v.to_str());
    for i in 1..10000 {
        let n = match e {
            Some(e) => format!("{s} ({i}).{e}"),
            None => format!("{s} ({i})"),
        };
        let p = d.join(n);
        if !p.exists() {
            return p;
        }
    }
    d.join("download.bin")
}
fn filename_from_response(r: &reqwest::Response) -> Option<String> {
    r.headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_content_disposition)
        .or_else(|| filename_from_url(r.url().as_str()))
}
async fn yt_dlp_available() -> bool {
    TokioCommand::new("yt-dlp")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
async fn inspect_media(
    url: &str,
    headers: &[String],
    proxy: Option<&str>,
    control: Option<&Arc<Control>>,
) -> Result<YtDlpInfo, String> {
    if !yt_dlp_available().await {
        return Err("YouTube/media extraction requires yt-dlp.".into());
    }

    let mut cmd = TokioCommand::new("yt-dlp");
    cmd.env("PYTHONIOENCODING", "utf-8:replace")
        .env("PYTHONUTF8", "1")
        .args(["--dump-single-json", "--no-warnings", "--no-playlist"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(p) = proxy.filter(|p| !p.is_empty()) {
        cmd.args(["--proxy", p]);
    }
    for h in headers {
        cmd.args(["--add-header", h]);
    }

    // Metadata extraction is a real child process. Keep the same child handle
    // in the task Control so Cancel/STOP ALL can terminate it too. The old
    // implementation used std::process::Command::output() inside an async
    // task, which could occupy a Tokio worker while yt-dlp was resolving a
    // page and make cancellation appear to hang.
    let child = cmd.arg(url).spawn().map_err(|e| e.to_string())?;

    let output = if let Some(c) = control {
        let pid = child.id().unwrap_or(0);
        c.child_pid.store(pid as u64, Ordering::SeqCst);
        if let Ok(mut slot) = c.child.lock() {
            *slot = Some(child);
        }
        if c.cancelled.load(Ordering::SeqCst) {
            kill_tree(c).await;
        }
        let child = c.child.lock().ok().and_then(|mut slot| slot.take());
        match child {
            Some(x) => {
                let out = x.wait_with_output().await.map_err(|e| e.to_string());
                c.child_pid.store(0, Ordering::SeqCst);
                out.map_err(|e| e.to_string())?
            }
            None => return Err("Media inspection was cancelled.".into()),
        }
    } else {
        child_wait_output(child).await?
    };

    if control.map(|c| c.cancelled.load(Ordering::SeqCst)).unwrap_or(false) {
        return Err("Media inspection was cancelled.".into());
    }

    if !output.status.success() {
        let m = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if m.is_empty() {
            "yt-dlp could not inspect this media URL.".into()
        } else {
            m
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}

async fn child_wait_output(mut child: tokio::process::Child) -> Result<std::process::Output, String> {
    child.wait_with_output().await.map_err(|e| e.to_string())
}

fn media_expected_total(i: &YtDlpInfo) -> Option<u64> {
    // For YouTube's usual video+audio downloads, yt-dlp downloads multiple
    // streams and merges them. Progress must use the transfer total (the sum
    // of the selected streams), not the final merged file size, otherwise the
    // UI can legitimately pass 100% while yt-dlp is still downloading.
    if let Some(fs) = i.requested_formats.as_ref() {
        let mut n: u64 = 0;
        let mut known = false;
        for f in fs {
            if let Some(size) = f.filesize.or(f.filesize_approx) {
                n = n.saturating_add(size);
                known = true;
            }
        }
        if known && n > 0 {
            return Some(n);
        }
    }
    i.filesize.or(i.filesize_approx)
}
#[tauri::command]
async fn inspect_url(url: String) -> Result<ResourceInfo, String> {
    let u = url.trim().to_string();
    let p = reqwest::Url::parse(&u).map_err(|_| "Please enter a valid URL.".to_string())?;
    if !matches!(p.scheme(), "http" | "https") {
        return Err("Only HTTP and HTTPS URLs are supported.".into());
    }
    if is_known_media_page(&p) {
        let i = inspect_media(&u, &[], None, None).await?;
        return Ok(ResourceInfo {
            url: u.clone(),
            kind: "media_page".into(),
            content_type: None,
            filename: i
                .title
                .as_ref()
                .map(|t| safe_media_filename(t, i.ext.as_deref().unwrap_or("mp4"))),
            size: media_expected_total(&i),
            final_url: u,
            status_code: 200,
        });
    }
    let c = reqwest::Client::builder()
        .user_agent("Monk3i Download Manager/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;
    let r = match c.head(p.clone()).send().await {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => r,
        _ => c
            .get(p)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|e| e.to_string())?,
    };
    let ct = r
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let f = filename_from_response(&r).map(|x| ensure_extension(x, ct.as_deref().unwrap_or("")));
    let size = total_size(&r);
    let final_url = r.url().to_string();
    let kind = resource_kind(ct.as_deref(), r.url()).into();
    Ok(ResourceInfo {
        url: u,
        kind,
        content_type: ct,
        filename: f,
        size,
        final_url,
        status_code: r.status().as_u16(),
    })
}
async fn media_partial(d: &Path) -> u64 {
    let mut e = match tokio::fs::read_dir(d).await {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let mut n = 0;
    while let Ok(Some(x)) = e.next_entry().await {
        let p = x.path();
        let f = p.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if p.is_file() && (f.ends_with(".part") || f.ends_with(".ytdl")) {
            n += x.metadata().await.map(|m| m.len()).unwrap_or(0)
        }
    }
    n
}
async fn find_media(d: &Path, id: &str) -> Option<PathBuf> {
    // Never treat an arbitrary file in the staging directory as the finished
    // media. A stale sidecar or intermediate file can otherwise make Monk3i
    // believe the job completed and then launch another yt-dlp pass.
    let mut e = tokio::fs::read_dir(d).await.ok()?;
    while let Ok(Some(x)) = e.next_entry().await {
        let p = x.path();
        if !p.is_file() {
            continue;
        }
        let f = p.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if f.ends_with(".part") || f.ends_with(".ytdl") {
            continue;
        }
        if p.file_stem().and_then(|v| v.to_str()) == Some(id) {
            return Some(p);
        }
    }
    None
}
async fn wait_paused(c: &Arc<Control>) -> bool {
    while c.paused.load(Ordering::Relaxed) && !c.cancelled.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await
    }
    !c.cancelled.load(Ordering::Relaxed)
}
fn kill(c: &Arc<Control>) {
    if let Ok(mut s) = c.child.lock() {
        if let Some(x) = s.as_mut() {
            let _ = x.start_kill();
        }
    }
}
async fn kill_tree(c: &Arc<Control>) {
    let pid = c.child.lock().ok().and_then(|s| s.as_ref().and_then(|x| x.id())).or_else(|| { let p=c.child_pid.load(Ordering::SeqCst); (p>0).then_some(p as u32) });
    if let Some(pid) = pid {
        #[cfg(windows)]
        {
            let _ = TokioCommand::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output().await;
        }
    }
    kill(c);
}
async fn run_media(
    app: tauri::AppHandle,
    id: String,
    url: String,
    dir: PathBuf,
    c: Arc<Control>,
    o: DownloadOptions,
) {
    let h = o.headers.unwrap_or_default();
    let i = match inspect_media(&url, &h, o.proxy.as_deref(), Some(&c)).await {
        Ok(v) => v,
        Err(e) => {
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
                Some(e),
            );
            remove_control(&id);
            return;
        }
    };
    let mid = i.id.clone().unwrap_or_else(|| id.clone());
    let title = i.title.clone().unwrap_or_else(|| "Downloaded media".into());
    let mut total = media_expected_total(&i);
    let td = std::env::temp_dir().join(format!(".monk3i-media-{id}"));
    if let Err(e) = tokio::fs::create_dir_all(&td).await {
        emit_progress(
            &app,
            &id,
            &url,
            "Media download",
            0,
            total,
            None,
            0,
            "error",
            None,
            Some(e.to_string()),
        );
        remove_control(&id);
        return;
    }
    let template = td.join("%(id)s.%(ext)s");
    if c.cancelled.load(Ordering::Relaxed) {
        kill(&c);
        let _ = tokio::fs::remove_dir_all(&td).await;
        emit_progress(&app, &id, &url, "Media download", 0, total, None, 0, "cancelled", None, None);
        remove_control(&id);
        return;
    }
    loop {
        // A completed output may already exist if the process finished but
        // Windows briefly held the file open during post-processing. Finalize
        // it before ever starting another yt-dlp process.
        if let Some(existing) = find_media(&td, &mid).await {
            let ext = existing.extension().and_then(|x| x.to_str()).or(i.ext.as_deref()).unwrap_or("mp4");
            let path = available_path(&dir, &safe_media_filename(&title, ext));
            let mut moved = false;
            for _ in 0..20 {
                if tokio::fs::rename(&existing, &path).await.is_ok() {
                    moved = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if moved {
                let n = tokio::fs::metadata(&path).await.map(|m| m.len()).ok().or(total);
                let _ = tokio::fs::remove_dir_all(&td).await;
                emit_progress(&app, &id, &url, path.file_name().and_then(|x| x.to_str()).unwrap_or("media"), n.unwrap_or(0), n, Some(100.), 0, "completed", Some(path.to_string_lossy().to_string()), None);
                remove_control(&id);
                return;
            }
        }
        if c.cancelled.load(Ordering::Relaxed) {
            kill(&c);
            let _ = tokio::fs::remove_dir_all(&td).await;
            emit_progress(&app, &id, &url, "Media download", 0, total, None, 0, "cancelled", None, None);
            remove_control(&id);
            return;
        }
        if c.paused.load(Ordering::Relaxed) {
            let raw_d = media_partial(&td).await;
            if let Some(t) = total.as_ref() {
                if raw_d > *t {
                    total = Some(raw_d);
                }
            }
            let d = total.map(|t| raw_d.min(t)).unwrap_or(raw_d);
            emit_progress(&app, &id, &url, "Media download", d, total, total.map(|t| d as f64 * 100.0 / t as f64), 0, "paused", None, None);
            if !wait_paused(&c).await {
                kill(&c);
                let _ = tokio::fs::remove_dir_all(&td).await;
                emit_progress(&app, &id, &url, "Media download", 0, total, None, 0, "cancelled", None, None);
                remove_control(&id);
                return;
            }
        }
        let mut cmd = tokio::process::Command::new("yt-dlp");
        cmd.env("PYTHONIOENCODING", "utf-8:replace")
            .env("PYTHONUTF8", "1")
            .args(["--no-playlist", "--newline", "--continue", "--no-overwrites", "--retries", "3", "--fragment-retries", "3", "-f", "bv*+ba/b", "--merge-output-format", "mp4", "--print", "after_move:filepath", "-o"])
            .arg(template.to_string_lossy().to_string());
        if let Some(p) = o.proxy.as_deref().filter(|p| !p.is_empty()) { cmd.args(["--proxy", p]); }
        for x in &h { cmd.args(["--add-header", x]); }
        let lim = c.speed_limit.load(Ordering::Relaxed);
        if lim > 0 { cmd.args(["--limit-rate", &lim.to_string()]); }
        let child = match cmd.arg(&url).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(v) => v,
            Err(e) => {
                emit_progress(&app, &id, &url, "Media download", 0, total, None, 0, "error", None, Some(e.to_string()));
                remove_control(&id);
                return;
            }
        };
        c.child_pid.store(child.id().unwrap_or(0) as u64, Ordering::SeqCst);
        if let Ok(mut s) = c.child.lock() { *s = Some(child) }
        // If cancellation raced with process creation, terminate the newly
        // spawned process immediately. This closes the small start-up window
        // where STOP ALL could otherwise leave a fresh yt-dlp running.
        if c.cancelled.load(Ordering::Relaxed) {
            if let Ok(mut s) = c.child.lock() {
                if let Some(x) = s.as_mut() {
                    let _ = x.start_kill();
                }
            }
            kill(&c);
        }
        let mut last = media_partial(&td).await;
        let mut last_t = Instant::now();
        let mut killed = false;
        loop {
            if c.cancelled.load(Ordering::SeqCst) || c.paused.load(Ordering::SeqCst) { killed = true; kill_tree(&c).await; }
            tokio::time::sleep(Duration::from_millis(350)).await;
            let now = Instant::now();
            let raw_d = media_partial(&td).await;
            // Clamp only the displayed progress. The filesystem byte count is
            // still used for speed, while the transfer total accounts for
            // separate video/audio streams.
            let d = total.map(|t| raw_d.min(t)).unwrap_or(raw_d);
            let speed = ((raw_d.saturating_sub(last)) as f64 / now.duration_since(last_t).as_secs_f64().max(0.001)) as u64;
            let status = if c.cancelled.load(Ordering::Relaxed) { "cancelled" } else if c.paused.load(Ordering::Relaxed) { "paused" } else { "downloading" };
            emit_progress(&app, &id, &url, "Downloading media...", d, total, total.map(|t| d as f64 * 100.0 / t as f64).map(|p| p.min(99.9)), speed, status, None, None);
            last = d;
            last_t = now;
            let done = {
                let mut s = c.child.lock().unwrap();
                match s.as_mut() { Some(x) => matches!(x.try_wait(), Ok(Some(_)) | Err(_)), None => true }
            };
            if done { break; }
        }
        let ch = { c.child.lock().unwrap().take() };
        // yt-dlp does not return until its post-processors (including ffmpeg)
        // have finished. Wait for that single process lifecycle instead of
        // taskkilling a PID after it has already exited; a reused Windows PID
        // must never be terminated accidentally. Cancellation/pause already
        // terminate the full process tree through kill().
        let output = if let Some(x) = ch {
            let out = x.wait_with_output().await.ok();
            c.child_pid.store(0, Ordering::SeqCst);
            out
        } else {
            c.child_pid.store(0, Ordering::SeqCst);
            None
        };
        let status_ok = output.as_ref().map(|x| x.status.success()).unwrap_or(false);
        let printed_path = output
            .as_ref()
            .and_then(|x| String::from_utf8_lossy(&x.stdout).lines().rev().find(|l| !l.trim().is_empty()).map(|l| PathBuf::from(l.trim())));
        if c.cancelled.load(Ordering::Relaxed) {
            let _ = tokio::fs::remove_dir_all(&td).await;
            emit_progress(&app, &id, &url, "Media download", 0, total, None, 0, "cancelled", None, None);
            remove_control(&id);
            return;
        }
        if c.paused.load(Ordering::Relaxed) || killed { continue; }

        // yt-dlp's --print after_move:filepath is the authoritative output
        // path after merging/remuxing. Never start a second media download just
        // because the final move is temporarily blocked by Windows.
        let src = if let Some(p) = printed_path.filter(|p| p.is_file()) {
            Some(p)
        } else {
            find_media(&td, &mid).await
        };

        if status_ok || src.is_some() {
            if let Some(src) = src {
                let ext = src.extension().and_then(|x| x.to_str()).or(i.ext.as_deref()).unwrap_or("mp4");
                let path = available_path(&dir, &safe_media_filename(&title, ext));
                let mut moved = false;
                for _ in 0..20 {
                    if tokio::fs::rename(&src, &path).await.is_ok() {
                        moved = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if !moved {
                    // Same-filesystem rename can be temporarily blocked by
                    // Defender/indexing. Copy the already completed output
                    // rather than downloading the media again.
                    if tokio::fs::copy(&src, &path).await.is_ok() {
                        let _ = tokio::fs::remove_file(&src).await;
                        moved = true;
                    }
                }
                if moved {
                    let n = tokio::fs::metadata(&path).await.map(|m| m.len()).ok().or(total);
                    let _ = tokio::fs::remove_dir_all(&td).await;
                    emit_progress(&app, &id, &url, path.file_name().and_then(|x| x.to_str()).unwrap_or("media"), n.unwrap_or(0), n, Some(100.), 0, "completed", Some(path.to_string_lossy().to_string()), None);
                    remove_control(&id);
                    return;
                }

                let _ = tokio::fs::remove_dir_all(&td).await;
                emit_progress(&app, &id, &url, "Media download", last, total, None, 0, "error", None, Some("The media finished downloading, but Windows would not release the completed file.".into()));
                remove_control(&id);
                return;
            }

            // yt-dlp reported success but did not leave a final output file.
            // Do not blindly download the whole media again.
            let _ = tokio::fs::remove_dir_all(&td).await;
            emit_progress(&app, &id, &url, "Media download", last, total, None, 0, "error", None, Some("yt-dlp finished without producing a final media file.".into()));
            remove_control(&id);
            return;
        }
        // yt-dlp already performs its own network/fragment retries. Do not
        // launch a second top-level yt-dlp job here: that was the source of
        // the repeated full-media downloads after a failed lifecycle.
        let detail = output
            .as_ref()
            .map(|x| String::from_utf8_lossy(&x.stderr).trim().to_string())
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| "yt-dlp did not produce a completed media file.".into());
        let _ = tokio::fs::remove_dir_all(&td).await;
        emit_progress(&app, &id, &url, "Media download", last, total, None, 0, "error", None, Some(detail));
        remove_control(&id);
        return;
    }
}
#[tauri::command]
async fn inspect_url_with_options(url: String, options: Option<DownloadOptions>) -> Result<ResourceInfo, String> {
    let o = options.unwrap_or_default();
    let u = url.trim().to_string();
    let p = reqwest::Url::parse(&u).map_err(|_| "Please enter a valid URL.".to_string())?;
    if !matches!(p.scheme(), "http" | "https") { return Err("Only HTTP and HTTPS URLs are supported.".into()); }
    if is_known_media_page(&p) {
        let i = inspect_media(&u, o.headers.as_deref().unwrap_or(&[]), o.proxy.as_deref(), None).await?;
        return Ok(ResourceInfo { url: u.clone(), kind: "media_page".into(), content_type: None, filename: i.title.as_ref().map(|t| safe_media_filename(t, i.ext.as_deref().unwrap_or("mp4"))), size: media_expected_total(&i), final_url: u, status_code: 200 });
    }
    inspect_url(u).await
}
async fn build_http_client(o: &DownloadOptions) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder().user_agent("Monk3i Download Manager/0.2").redirect(reqwest::redirect::Policy::limited(10));
    if let Some(p) = o.proxy.as_deref().filter(|x| !x.is_empty()) { b = b.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?) }
    b.build().map_err(|e| e.to_string())
}
fn apply_headers(mut req: reqwest::RequestBuilder, headers: Option<&Vec<String>>) -> reqwest::RequestBuilder {
    if let Some(hs) = headers { for x in hs { if let Some((k, v)) = x.split_once(':') { req = req.header(k.trim(), v.trim()) } } }
    req
}
#[tauri::command]
async fn start_download(app: tauri::AppHandle, url: String, options: Option<DownloadOptions>) -> Result<String, String> {
    let u = url.trim().to_string();
    let p = reqwest::Url::parse(&u).map_err(|_| "Please enter a valid URL.".to_string())?;
    if !matches!(p.scheme(), "http" | "https") { return Err("Only HTTP and HTTPS URLs are supported.".into()); }
    let o = options.unwrap_or_default();
    if STOP_ALL.load(Ordering::SeqCst) {
        return Err("Downloads are stopped. Start a new download explicitly to continue.".into());
    }
    if active_urls().lock().ok().and_then(|m| m.get(&u).cloned()).is_some() {
        return Err("This URL is already downloading.".into());
    }
    let id = new_id();
    let c = Arc::new(Control { url: u.clone(), paused: AtomicBool::new(false), cancelled: AtomicBool::new(false), speed_limit: AtomicU64::new(o.speed_limit.unwrap_or(0)), child: Mutex::new(None), child_pid: AtomicU64::new(0) });
    controls().lock().unwrap().insert(id.clone(), c.clone());
    active_urls().lock().unwrap().insert(u.clone(), id.clone());
    if is_known_media_page(&p) {
        let d = dirs::download_dir().or_else(dirs::home_dir).ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        emit_progress(&app, &id, &u, "Preparing media...", 0, None, Some(0.), 0, "downloading", None, None);
        if STOP_ALL.load(Ordering::SeqCst) {
        c.cancelled.store(true, Ordering::Relaxed);
        remove_control(&id);
        emit_progress(&app, &id, &u, "Media download", 0, None, None, 0, "cancelled", None, None);
        return Ok(id);
    }
    tokio::spawn(run_media(app, id.clone(), u, d, c, o));
        return Ok(id);
    }
    let client = match build_http_client(&o).await { Ok(v) => v, Err(e) => { remove_control(&id); return Err(e); } };
    let d = dirs::download_dir().or_else(dirs::home_dir).ok_or_else(|| "Could not find a Downloads folder.".to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let guessed = filename_from_url(&u).unwrap_or_else(|| "download.bin".into());
    let guessed_path = d.join(&guessed);
    let mut final_path = if guessed_path.with_file_name(format!("{}.part", guessed_path.file_name().and_then(|x| x.to_str()).unwrap_or("download"))).exists() { guessed_path } else { available_path(&d, &guessed) };
    let mut part_path = final_path.with_file_name(format!("{}.part", final_path.file_name().and_then(|x| x.to_str()).unwrap_or("download")));
    if !part_path.exists() {
        let req = apply_headers(client.get(p.clone()).header(reqwest::header::RANGE, "bytes=0-0"), o.headers.as_ref());
        let r = req.send().await.map_err(|e| e.to_string())?;
        let ct = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if resource_kind(Some(&ct), r.url()) == "webpage" || resource_kind(Some(&ct), r.url()) == "media_page" { remove_control(&id); return Err("This URL points to a webpage, not a direct file.".into()); }
        if let Some(name) = filename_from_response(&r).map(|x| ensure_extension(x, &ct)) {
            final_path = available_path(&d, &name);
            part_path = final_path.with_file_name(format!("{}.part", final_path.file_name().and_then(|x| x.to_str()).unwrap_or("download")))
        }
        drop(r)
    }
    let filename = final_path.file_name().and_then(|x| x.to_str()).unwrap_or("download").to_string();
    let mut retries = 0u8;
    loop {
        if c.cancelled.load(Ordering::Relaxed) {
            let _ = tokio::fs::remove_file(&part_path).await;
            emit_progress(&app, &id, &u, &filename, 0, None, None, 0, "cancelled", None, None);
            remove_control(&id); return Ok(id);
        }
        if c.paused.load(Ordering::Relaxed) {
            let downloaded = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);
            emit_progress(&app, &id, &u, &filename, downloaded, None, None, 0, "paused", Some(part_path.to_string_lossy().to_string()), None);
            if !wait_paused(&c).await { let _ = tokio::fs::remove_file(&part_path).await; continue; }
            continue;
        }
        let existing = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(p.clone());
        if existing > 0 { req = req.header(reqwest::header::RANGE, format!("bytes={existing}-")); }
        req = apply_headers(req, o.headers.as_ref());
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => { retries = retries.saturating_add(1); if retries <= 2 { tokio::time::sleep(Duration::from_secs(2u64.pow(retries as u32))).await; continue; } emit_progress(&app,&id,&u,&filename,existing,None,None,0,"error",Some(part_path.to_string_lossy().to_string()),Some(e.to_string())); remove_control(&id); return Ok(id); }
        };
        if existing > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT { let _ = tokio::fs::remove_file(&part_path).await; continue; }
        let response = match response.error_for_status() {
            Ok(r) => r,
            Err(e) => { retries = retries.saturating_add(1); if retries <= 2 { tokio::time::sleep(Duration::from_secs(2u64.pow(retries as u32))).await; continue; } emit_progress(&app,&id,&u,&filename,existing,None,None,0,"error",Some(part_path.to_string_lossy().to_string()),Some(e.to_string())); remove_control(&id); return Ok(id); }
        };
        let total = total_size(&response).map(|n| if existing > 0 { n.max(existing) } else { n });
        let mut downloaded = existing;
        let mut file = match tokio::fs::OpenOptions::new().create(true).write(true).append(existing > 0).truncate(existing == 0).open(&part_path).await { Ok(f) => f, Err(e) => { remove_control(&id); return Err(e.to_string()); } };
        emit_progress(&app,&id,&u,&filename,downloaded,total,total.map(|t| downloaded as f64*100.0/t as f64),0,"downloading",Some(part_path.to_string_lossy().to_string()),None);
        let mut stream = response.bytes_stream();
        let started = Instant::now();
        let mut last_error: Option<String> = None;
        let mut paused = false;
        let mut cancelled = false;
        loop {
            tokio::select! {
                _=tokio::time::sleep(Duration::from_millis(200))=>{if c.cancelled.load(Ordering::Relaxed){cancelled=true;break}if c.paused.load(Ordering::Relaxed){paused=true;break}}
                chunk=stream.next()=>match chunk{Some(Ok(bytes))=>{if c.cancelled.load(Ordering::Relaxed){cancelled=true;break}file.write_all(&bytes).await.map_err(|e|e.to_string())?;downloaded+=bytes.len() as u64;let lim=c.speed_limit.load(Ordering::Relaxed);if lim>0{let target=downloaded as f64/lim as f64;if target>started.elapsed().as_secs_f64(){tokio::time::sleep(Duration::from_secs_f64(target-started.elapsed().as_secs_f64())).await}}let speed=((downloaded.saturating_sub(existing)) as f64/started.elapsed().as_secs_f64().max(0.001)) as u64;let pct=total.map(|t|(downloaded as f64*100.0/t as f64).min(99.9));emit_progress(&app,&id,&u,&filename,downloaded,total,pct,speed,"downloading",Some(part_path.to_string_lossy().to_string()),None)}Some(Err(e))=>{last_error=Some(e.to_string());break}None=>break,}
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file); drop(stream);
        if cancelled { let _=tokio::fs::remove_file(&part_path).await; emit_progress(&app,&id,&u,&filename,0,total,None,0,"cancelled",None,None); remove_control(&id); return Ok(id); }
        if paused { emit_progress(&app,&id,&u,&filename,downloaded,total,total.map(|t|downloaded as f64*100.0/t as f64),0,"paused",Some(part_path.to_string_lossy().to_string()),None); if !wait_paused(&c).await { let _=tokio::fs::remove_file(&part_path).await; continue; } continue; }
        if let Some(err)=last_error { retries=retries.saturating_add(1); if retries<=2 { tokio::time::sleep(Duration::from_secs(2u64.pow(retries as u32))).await; continue; } emit_progress(&app,&id,&u,&filename,downloaded,total,None,0,"error",Some(part_path.to_string_lossy().to_string()),Some(format!("Download failed after automatic retries; partial file kept: {err}"))); remove_control(&id); return Ok(id); }
        let final_total=tokio::fs::metadata(&part_path).await.map(|m|m.len()).unwrap_or(downloaded);
        if let Some(t)=total { if final_total<t { retries=retries.saturating_add(1); if retries<=2 {continue} emit_progress(&app,&id,&u,&filename,final_total,Some(t),Some(final_total as f64*100.0/t as f64),0,"error",Some(part_path.to_string_lossy().to_string()),Some("Server closed the connection before the expected file size was reached; partial file kept.".into())); remove_control(&id); return Ok(id); } }
        tokio::fs::rename(&part_path,&final_path).await.map_err(|e|e.to_string())?;
        emit_progress(&app,&id,&u,&filename,final_total,total.or(Some(final_total)),Some(100.),0,"completed",Some(final_path.to_string_lossy().to_string()),None);
        remove_control(&id); return Ok(id);
    }
}
#[tauri::command]
async fn pause_download(id: String) -> Result<(), String> {
    let c=get_control(&id).ok_or_else(||"Download not found.".to_string())?;
    if c.cancelled.load(Ordering::SeqCst){return Err("Download has been cancelled.".into())}
    c.paused.store(true,Ordering::SeqCst);
    let target=c.clone();
    tokio::spawn(async move { kill_tree(&target).await; });
    Ok(())
}
#[tauri::command]
fn resume_download(id: String) -> Result<(), String> { let c=get_control(&id).ok_or_else(||"Download not found.".to_string())?; if c.cancelled.load(Ordering::Relaxed){return Err("Download has been cancelled.".into())} c.paused.store(false,Ordering::Relaxed); Ok(()) }
#[tauri::command]
async fn cancel_download(id: String) -> Result<(), String> {
    let c=get_control(&id).ok_or_else(||"Download not found.".to_string())?;
    c.cancelled.store(true,Ordering::SeqCst);
    c.paused.store(false,Ordering::SeqCst);
    let target=c.clone();
    tokio::spawn(async move { kill_tree(&target).await; });
    Ok(())
}
#[tauri::command]
async fn cancel_all_downloads() -> Result<(), String> {
    // Latch STOP ALL synchronously. Any start_download call arriving after
    // this point is rejected before it can create a Control.
    STOP_ALL.store(true, Ordering::SeqCst);
    tokio::spawn(async { stop_all_active().await; });
    Ok(())
}
#[tauri::command]
fn clear_stop_all() -> Result<(), String> {
    STOP_ALL.store(false, Ordering::SeqCst);
    Ok(())
}
#[tauri::command]
fn set_speed_limit(id: String, bytes_per_second: u64) -> Result<(), String> { let c=get_control(&id).ok_or_else(||"Download not found.".to_string())?; c.speed_limit.store(bytes_per_second,Ordering::Relaxed); Ok(()) }
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() { tauri::Builder::default().plugin(tauri_plugin_opener::init()).invoke_handler(tauri::generate_handler![inspect_url,inspect_url_with_options,start_download,pause_download,resume_download,cancel_download,set_speed_limit,clear_stop_all,cancel_all_downloads]).run(tauri::generate_context!()).expect("error while running Tauri application"); }
