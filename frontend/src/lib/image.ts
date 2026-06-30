// Image preprocessing for chat attachments.
//
// Cloud vision endpoints cap request size — Groq rejects base64 images over
// ~4 MB with HTTP 413 "request_too_large". KinAI used to send the raw,
// full-resolution photo as base64, so any real phone photo silently failed
// (the turn errored with nothing shown). We now downscale images to a
// vision-friendly longest edge before base64-encoding. 1568 px is the size
// Anthropic / OpenAI / Groq all treat as "full detail", so quality for the
// model is unchanged; local backends (llama.cpp etc.) are unaffected by the
// smaller payload.

/** Longest-edge cap. 1568 px is the documented full-resolution ceiling for
 *  the major vision APIs; anything larger just costs tokens and latency. */
const MAX_EDGE = 1568;
/** JPEG quality for downscaled output — high enough to keep text/detail
 *  legible, low enough to keep base64 comfortably under provider caps. */
const JPEG_QUALITY = 0.85;

/**
 * Read a file to a data URL. Images whose longest side exceeds MAX_EDGE are
 * downscaled and re-encoded as JPEG so the base64 payload stays well under
 * cloud vision-API request limits. Non-images (PDFs) and already-small
 * images pass through untouched, preserving their original bytes (and any
 * PNG transparency). Falls back to a raw read if the browser can't decode
 * the image.
 */
export async function fileToDataUrl(file: File): Promise<string> {
  if (!file.type.startsWith('image/')) return rawDataUrl(file);

  let bitmap: ImageBitmap;
  try {
    bitmap = await createImageBitmap(file);
  } catch {
    // Exotic/unsupported image format — send the original bytes as-is.
    return rawDataUrl(file);
  }

  const longest = Math.max(bitmap.width, bitmap.height);
  if (longest <= MAX_EDGE) {
    bitmap.close();
    return rawDataUrl(file); // already small enough; keep original (lossless)
  }

  const scale = MAX_EDGE / longest;
  const w = Math.max(1, Math.round(bitmap.width * scale));
  const h = Math.max(1, Math.round(bitmap.height * scale));
  const canvas = document.createElement('canvas');
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext('2d');
  if (!ctx) {
    bitmap.close();
    return rawDataUrl(file);
  }
  ctx.drawImage(bitmap, 0, 0, w, h);
  bitmap.close();
  return canvas.toDataURL('image/jpeg', JPEG_QUALITY);
}

function rawDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error ?? new Error('read failed'));
    reader.readAsDataURL(file);
  });
}
