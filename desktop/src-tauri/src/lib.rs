use futures_util::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;
use tauri::Emitter;

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

fn filename_from_url(url: &str) -> String {
    url.split('?')
        .next()
        .and_then(|value| value.rsplit('/').next())
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().filter(|c| !c.is_control()).collect())
        .filter(|value: &String| !value.is_empty())
        .unwrap_or_else(|| "download".to_string())
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

    directory.join(format!("download-{}.bin", uuid_like_suffix()))
}

fn uuid_like_suffix() -> String {
    format!("{}", Instant::now().elapsed().as_nanos())
}

#[tauri::command]
async fn start_download(app: tauri::AppHandle, url: String) -> Result<String, String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| "Please enter a valid URL.".to_string())?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only HTTP and HTTPS downloads are supported.".to_string());
    }

    let id = format!("download-{}", Instant::now().elapsed().as_nanos());
    let client = reqwest::Client::new();
    let response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;

    let filename = response
        .url()
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|value| !value.is_empty())
        .map(|value| filename_from_url(value))
        .unwrap_or_else(|| filename_from_url(&url));

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
        .invoke_handler(tauri::generate_handler![greet, start_download])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
