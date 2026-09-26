// Images pasted into a composer.
//
// On macOS and Windows a paste carries an image as a file in
// `clipboardData`, and the composers attach it from there. WebKitGTK — the
// engine behind the Linux client — hands over neither a file nor text for
// an image on the clipboard: a screenshot copied with KDE's "Copy" button
// pasted nothing (field report 2026-09-26), while text and the attach
// button worked. When the paste event carries nothing usable, the OS
// clipboard is read directly through the native clipboard plugin the app
// already ships (Wayland included), and the image becomes a PNG file the
// attach path takes like any other.

import type { Image } from '@tauri-apps/api/image';

/** The files the web engine handed over with the paste. */
export function pastedFiles(e: ClipboardEvent): File[] {
  const items = e.clipboardData?.items;
  if (!items) return [];
  const files: File[] = [];
  for (const item of Array.from(items)) {
    if (item.kind === 'file') {
      const f = item.getAsFile();
      if (f) files.push(f);
    }
  }
  return files;
}

/** Whether the paste has text to insert — then the native paste must run. */
export function pasteHasText(e: ClipboardEvent): boolean {
  const types = e.clipboardData?.types;
  return !!types && Array.from(types).some((t) => t === 'text/plain' || t === 'text/html');
}

/** The image on the OS clipboard as a PNG file — null when there is none,
 *  when it cannot be read, or outside the Tauri shell. */
export async function imageFromNativeClipboard(): Promise<File | null> {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return null;
  let img: Image | null = null;
  try {
    const { readImage } = await import('@tauri-apps/plugin-clipboard-manager');
    img = await readImage();
    const size = await img.size();
    if (!size?.width || !size?.height) return null;
    const rgba = await img.rgba();
    if (rgba.length < size.width * size.height * 4) return null;
    const blob = await rgbaToPng(rgba, size.width, size.height);
    if (!blob) return null;
    const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
    return new File([blob], `screenshot-${stamp}.png`, { type: 'image/png' });
  } catch {
    // No image on the clipboard (the plugin rejects), or a platform
    // without the plugin — the paste simply does nothing, as before.
    return null;
  } finally {
    if (img) {
      try {
        await img.close();
      } catch {
        // Freed with the webview at the latest.
      }
    }
  }
}

function rgbaToPng(rgba: Uint8Array, width: number, height: number): Promise<Blob | null> {
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext('2d');
  if (!ctx) return Promise.resolve(null);
  // A fresh array, not a view over the plugin's buffer: ImageData wants a
  // plain ArrayBuffer behind it, and a copy of a screenshot is cheap.
  const pixels = new Uint8ClampedArray(width * height * 4);
  pixels.set(rgba.subarray(0, pixels.length));
  ctx.putImageData(new ImageData(pixels, width, height), 0, 0);
  return new Promise((resolve) => canvas.toBlob(resolve, 'image/png'));
}
