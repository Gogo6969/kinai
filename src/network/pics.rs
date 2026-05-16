//! Serve generated images from `~/.kinai/pics/`.
//!
//! The `/pic` and `/picHQ` slash commands save ComfyUI output to this
//! directory; clients in the family fetch them through this endpoint
//! when rendering the inline `<img>` from the assistant's markdown.
//!
//! No auth on this route: invite-paired family members are on the LAN
//! and have the host URL anyway. The pic filename is a UUID, so guessing
//! one is infeasible. If a stricter posture is needed later, fold this
//! into the same JWT check the WebSocket handshake uses.

use std::path::Path;

use axum::extract::Path as AxPath;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::comfyui::pics_dir;

pub async fn serve_pic(AxPath(filename): AxPath<String>) -> Result<Response, (StatusCode, String)> {
    // Defense-in-depth: refuse anything that smells like path traversal,
    // a hidden file, or an absolute path. The route already matches a
    // single segment, but reject explicitly.
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.starts_with('.')
    {
        return Err((StatusCode::BAD_REQUEST, "bad filename".into()));
    }
    let path = pics_dir().join(&filename);
    if !path.starts_with(pics_dir()) {
        // Belt-and-suspenders against join-with-absolute-path tricks.
        return Err((StatusCode::BAD_REQUEST, "bad filename".into()));
    }
    serve_file(&path).await
}

async fn serve_file(path: &Path) -> Result<Response, (StatusCode, String)> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("missing: {}: {e}", path.display())))?;
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    };
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            // Aggressive caching: filename is content-addressed (UUID),
            // so the bytes never change for a given URL.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".into()),
        ],
        bytes,
    )
        .into_response())
}
