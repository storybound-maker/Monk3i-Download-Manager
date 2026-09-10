import { useState } from "react";
import "./App.css";

function App() {
  const [url, setUrl] = useState("");

  const addDownload = () => {
    const trimmedUrl = url.trim();
    if (!trimmedUrl) return;

    console.log("Download requested:", trimmedUrl);
    setUrl("");
  };

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
              <p>Paste a direct download link to get started.</p>
            </div>
          </div>

          <div className="url-row">
            <input
              className="url-input"
              type="url"
              value={url}
              onChange={(event) => setUrl(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") addDownload();
              }}
              placeholder="https://example.com/file.zip"
              aria-label="Download URL"
            />
            <button className="primary-button" type="button" onClick={addDownload}>
              + Add Download
            </button>
          </div>
        </section>

        <section className="downloads-panel">
          <div className="section-heading">
            <div>
              <h2>Active Downloads</h2>
              <p>Downloads currently in progress.</p>
            </div>
            <span className="count-badge">0</span>
          </div>

          <div className="empty-state">
            <div className="empty-icon">↓</div>
            <h3>No active downloads</h3>
            <p>
              Your active downloads will appear here once you add a download.
            </p>
          </div>
        </section>
      </main>
    </div>
  );
}

export default App;
