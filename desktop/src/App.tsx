import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type DownloadProgress = {
  id: string;
  url: string;
  filename: string;
  downloaded: number;
  total: number | null;
  percent: number | null;
  speed: number;
  status: "downloading" | "completed" | "error";
  path: string | null;
  error: string | null;
};

type ResourceInfo = {
  url: string;
  kind: "file" | "image" | "video" | "audio" | "webpage";
  content_type: string | null;
  filename: string | null;
  size: number | null;
  final_url: string;
  status_code: number;
};

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatSpeed(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

function typeLabel(kind: ResourceInfo["kind"]): string {
  switch (kind) {
    case "image": return "Image";
    case "video": return "Video";
    case "audio": return "Audio";
    case "webpage": return "Webpage";
    default: return "Direct file";
  }
}

function App() {
  const [url, setUrl] = useState("");
  const [downloads, setDownloads] = useState<DownloadProgress[]>([]);
  const [error, setError] = useState("");
  const [resource, setResource] = useState<ResourceInfo | null>(null);
  const [inspecting, setInspecting] = useState(false);

  useEffect(() => {
    let unlisten: (() => void) | undefined;

    listen<DownloadProgress>("download-progress", (event) => {
      setDownloads((current) => {
        const incoming = event.payload;
        const existingIndex = current.findIndex((download) => download.id === incoming.id);

        if (existingIndex === -1) {
          return [incoming, ...current];
        }

        const next = [...current];
        next[existingIndex] = incoming;
        return next;
      });
    }).then((cleanup) => {
      unlisten = cleanup;
    });

    return () => {
      unlisten?.();
    };
  }, []);

  const inspectUrl = async () => {
    const trimmedUrl = url.trim();
    if (!trimmedUrl) return;

    setError("");
    setResource(null);
    setInspecting(true);

    try {
      const result = await invoke<ResourceInfo>("inspect_url", { url: trimmedUrl });
      setResource(result);
    } catch (inspectionError) {
      setError(String(inspectionError));
    } finally {
      setInspecting(false);
    }
  };

  const addDownload = async () => {
    const trimmedUrl = url.trim();
    if (!trimmedUrl) return;

    setError("");

    try {
      await invoke("start_download", { url: trimmedUrl });
      setUrl("");
      setResource(null);
    } catch (downloadError) {
      setError(String(downloadError));
    }
  };

  const activeDownloads = downloads.filter(
    (download) => download.status === "downloading" || download.status === "error",
  );

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">M</div>
          <div>
            <div className="brand-name">Monk3i</div>
            <div className="brand-subtitle">Download Manager</div>
          </div>
        </div>

        <nav className="sidebar-nav" aria-label="Main navigation">
          <button className="nav-item active" type="button">
            <span className="nav-icon">↓</span>
            <span>Downloads</span>
          </button>
          <button className="nav-item" type="button">
            <span className="nav-icon">◷</span>
            <span>Queue</span>
          </button>
          <button className="nav-item" type="button">
            <span className="nav-icon">✓</span>
            <span>Completed</span>
          </button>
          <button className="nav-item" type="button">
            <span className="nav-icon">⚙</span>
            <span>Settings</span>
          </button>
        </nav>

        <div className="sidebar-footer">
          <span>Monk3i Systems</span>
          <span>v0.1.0</span>
        </div>
      </aside>

      <main className="main-content">
        <header className="topbar">
          <div>
            <p className="eyebrow">MONK3I SYSTEMS</p>
            <h1>Downloads</h1>
            <p className="page-description">
              Manage and monitor your downloads in one place.
            </p>
          </div>

          <button className="icon-button" type="button" aria-label="Settings">
            ⚙
          </button>
        </header>

        <section className="add-card" aria-label="Add download">
          <div className="add-card-copy">
            <div className="add-icon">↓</div>
            <div>
              <h2>Add a download</h2>
              <p>Paste a URL and Monk3i will identify what it points to.</p>
            </div>
          </div>

          <div className="url-row">
            <input
              className="url-input"
              type="url"
              value={url}
              onChange={(event) => {
                setUrl(event.currentTarget.value);
                setResource(null);
                setError("");
              }}
              onKeyDown={(event) => {
                if (event.key === "Enter") inspectUrl();
              }}
              placeholder="https://example.com/file.zip"
              aria-label="Download URL"
            />
            <button className="secondary-button" type="button" onClick={inspectUrl} disabled={inspecting}>
              {inspecting ? "Checking..." : "Detect"}
            </button>
            <button className="primary-button" type="button" onClick={addDownload}>
              + Add Download
            </button>
          </div>

          {resource && (
            <div className="resource-result">
              <div>
                <strong>{typeLabel(resource.kind)}</strong>
                <span>{resource.filename ?? "No filename detected"}</span>
              </div>
              {resource.content_type && <span>{resource.content_type}</span>}
              {resource.size !== null && <span>{formatBytes(resource.size)}</span>}
            </div>
          )}

          {resource?.kind === "webpage" && (
            <p className="info-message">
              This is a webpage, not a direct file. Media extraction will be added next.
            </p>
          )}

          {error && <p className="error-message">{error}</p>}
        </section>

        <section className="downloads-panel">
          <div className="section-heading">
            <div>
              <h2>Active Downloads</h2>
              <p>Downloads currently in progress.</p>
            </div>
            <span className="count-badge">{activeDownloads.length}</span>
          </div>

          {activeDownloads.length === 0 ? (
            <div className="empty-state">
              <div className="empty-icon">↓</div>
              <h3>No active downloads</h3>
              <p>
                Your active downloads will appear here once you add a download.
              </p>
            </div>
          ) : (
            <div className="download-list">
              {activeDownloads.map((download) => {
                const percent = Math.min(100, Math.max(0, download.percent ?? 0));

                return (
                  <article className="download-item" key={download.id}>
                    <div className="download-item-top">
                      <div className="download-file-info">
                        <div className="download-file-icon">↓</div>
                        <div>
                          <h3>{download.filename}</h3>
                          <p>
                            {formatBytes(download.downloaded)}
                            {download.total !== null && ` / ${formatBytes(download.total)}`}
                          </p>
                        </div>
                      </div>
                      <div className="download-status">
                        {download.status === "error" ? "Error" : `${percent.toFixed(0)}%`}
                      </div>
                    </div>

                    <div className="progress-track" aria-label="Download progress">
                      <div className="progress-fill" style={{ width: `${percent}%` }} />
                    </div>

                    <div className="download-item-bottom">
                      <span>
                        {download.status === "error"
                          ? download.error ?? "Download failed."
                          : formatSpeed(download.speed)}
                      </span>
                      <span>{download.status === "error" ? "" : "Downloading"}</span>
                    </div>
                  </article>
                );
              })}
            </div>
          )}
        </section>
      </main>
    </div>
  );
}

export default App;
