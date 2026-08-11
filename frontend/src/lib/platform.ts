/**
 * Which desktop OS this window is running on.
 *
 * KinAI ships one Svelte frontend to three very different machines, and
 * for a long time the copy assumed all of them were Macs: a Windows user
 * following a "No hosts found" hint was told to open System Settings →
 * Privacy & Security, a pane that does not exist on their computer, and
 * the microphone errors named macOS panes and offered buttons that were
 * only rendered on macOS. Every one of those strings now asks here first.
 *
 * `navigator.userAgentData.platform` is the modern signal but exists only
 * in Chromium — that covers Windows (WebView2), not WebKitGTK or WKWebView
 * — so `navigator.platform` remains the fallback. It is deprecated and
 * still populated everywhere KinAI runs ("MacIntel", "Win32",
 * "Linux x86_64"), and the userAgent string is the last resort.
 *
 * Unknown platforms resolve to 'linux': of the three it makes the fewest
 * promises about what the user's settings look like.
 */

export type Platform = 'macos' | 'windows' | 'linux';

/**
 * Pure so it can be exercised against the real strings the three
 * WebViews report, instead of only ever being observable on whichever
 * machine happens to be running the app.
 *
 * Order matters: macOS is checked first because "Macintosh" contains no
 * "win", but a Windows userAgent contains "Windows NT" AND the token
 * "like Gecko" — and checking /win/ first would misfile nothing, while
 * checking /mac/ first also correctly catches "MacIntel".
 */
export function detectPlatform(hint: string): Platform {
  if (/mac|iphone|ipad|ipod/i.test(hint)) return 'macos';
  if (/win/i.test(hint)) return 'windows';
  return 'linux';
}

function detect(): Platform {
  if (typeof navigator === 'undefined') return 'linux';

  const uaData = (navigator as Navigator & { userAgentData?: { platform?: string } })
    .userAgentData;
  return detectPlatform(
    `${uaData?.platform ?? ''} ${navigator.platform ?? ''} ${navigator.userAgent ?? ''}`,
  );
}

export const platform: Platform = detect();

export const isMac = platform === 'macos';
export const isWindows = platform === 'windows';
export const isLinux = platform === 'linux';

/**
 * Pick the variant for the current platform. Keeps three-way copy on one
 * screen instead of scattered `{#if}` ladders:
 *
 *   byPlatform({ macos: 'System Settings', windows: 'Settings', linux: 'your settings' })
 */
export function byPlatform<T>(variants: { macos: T; windows: T; linux: T }): T {
  return variants[platform];
}

/** The modifier key names this platform actually prints on its keys. */
export const MODIFIERS = byPlatform({
  macos: { cmd: '⌘', alt: '⌥', ctrl: '⌃', shift: '⇧' },
  windows: { cmd: 'Win', alt: 'Alt', ctrl: 'Ctrl', shift: 'Shift' },
  linux: { cmd: 'Super', alt: 'Alt', ctrl: 'Ctrl', shift: 'Shift' },
});

/** How to say "this computer" without claiming it is a Mac. */
export const THIS_COMPUTER = byPlatform({
  macos: 'this Mac',
  windows: 'this PC',
  linux: 'this computer',
});
