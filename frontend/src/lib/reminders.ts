/**
 * Pure helpers for the reminders UI (Calendar panel + due popup).
 *
 * Everything here works on `due_local` ("YYYY-MM-DDTHH:MM" in the zone
 * the member set the reminder in) — never on `due_at`, which is the UTC
 * instant the host's scheduler fires on and must not be re-rendered
 * through the device's zone.
 */

/** "HH:MM" from a `due_local` value. */
export function timeOf(dueLocal: string): string {
  return dueLocal.slice(11, 16) || dueLocal;
}

/** "YYYY-MM-DD" from a `due_local` value — the day-group key. */
export function dayOf(dueLocal: string): string {
  return dueLocal.slice(0, 10);
}

/** "YYYY-MM-DD" of a Date in the device's local zone. */
export function localDayKey(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** "YYYY-MM-DD" of an instant in `tz` — the zone the reminder was set in.
 *  Falls back to the device's zone when `tz` is empty or unknown. This
 *  converts NOW, never `due_at`: the comparison has to happen in the same
 *  zone the wall-clock was written in, or "Today" is a claim about the
 *  wrong day for anyone who has travelled. */
export function dayKeyIn(d: Date, tz: string): string {
  if (tz) {
    try {
      // en-CA renders as YYYY-MM-DD, which is the key format.
      return new Intl.DateTimeFormat('en-CA', {
        timeZone: tz,
        year: 'numeric',
        month: '2-digit',
        day: '2-digit',
      }).format(d);
    } catch {
      // Unknown zone name — fall through to the device's.
    }
  }
  return localDayKey(d);
}

/** Heading for a day group: "Today", "Tomorrow", else a short locale
 *  date such as "Wed 11 Sep" (the year is appended once it differs).
 *  `tz` is the zone the reminders in this group were set in. */
export function dayLabel(key: string, now: Date = new Date(), tz = ''): string {
  const todayKey = dayKeyIn(now, tz);
  if (key === todayKey) return 'Today';
  const [ty, tm, td] = todayKey.split('-').map(Number);
  if (ty && tm && td) {
    const shift = (days: number) => {
      const d = new Date(Date.UTC(ty, tm - 1, td + days));
      return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, '0')}-${String(d.getUTCDate()).padStart(2, '0')}`;
    };
    if (key === shift(1)) return 'Tomorrow';
    // A reminder that fired while the app was closed is usually overdue,
    // so yesterday is as common a heading as tomorrow.
    if (key === shift(-1)) return 'Yesterday';
  }
  const [y, m, d] = key.split('-').map(Number);
  if (!y || !m || !d) return key;
  const opts: Intl.DateTimeFormatOptions = { weekday: 'short', day: 'numeric', month: 'short' };
  if (y !== (ty || now.getFullYear())) opts.year = 'numeric';
  return new Date(y, m - 1, d).toLocaleDateString(undefined, opts);
}

/** Minutes from `now` until 09:00 tomorrow in the device's local zone —
 *  how "Tomorrow 9:00" is expressed to the host, which only speaks
 *  `snooze_minutes`. Rounded UP so it never lands before nine, and never
 *  less than one minute. */
export function minutesUntilTomorrowNine(now: Date = new Date()): number {
  const target = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1, 9, 0, 0, 0);
  return Math.max(1, Math.ceil((target.getTime() - now.getTime()) / 60_000));
}

/** A URL as it appears in reminder text. */
const URL_RE = /https?:\/\/[^\s<>"']+/gi;

/** Trailing characters that belong to the sentence, not the address. */
function trimUrlTail(url: string): string {
  return url.replace(/[.,;:!?)\]}'"]+$/, '');
}

/**
 * Split reminder text into the words to read and the links to tap.
 *
 * EVERY address comes out of the prose, wherever it sat. Leaving one
 * in-line was the first attempt, on the theory that removing a
 * mid-sentence URL leaves a hole in the reading — but a real reminder
 * showed why that is wrong: the model wrote the address into the middle
 * of the sentence, so it ate three of the card's four visible lines AND
 * appeared a second time in the tappable row below it. A raw URL is
 * never the thing a person wants to read.
 *
 * The connector that introduced the link ("… report — <url>. Pull the
 * three points …") goes with it, and the leftover spacing and
 * punctuation are tidied, so the sentence closes as if the link had
 * never been written inline.
 */
export function splitLinks(text: string): { prose: string; links: string[] } {
  const links: string[] = [];
  for (const m of text.matchAll(URL_RE)) {
    const clean = trimUrlTail(m[0]);
    if (clean && !links.includes(clean)) links.push(clean);
  }
  if (links.length === 0) return { prose: text.trim(), links };
  const prose = text
    // The address, plus any dash or colon that introduced it. The match
    // has to stop before sentence punctuation, or the full stop that
    // closes the clause is swallowed along with the link.
    .replace(/\s*[–—:-]?\s*https?:\/\/[^\s<>"']*[^\s<>"'.,;:!?)\]}]/g, '')
    // "…implementation) ." → "…implementation)."
    .replace(/\s+([.,;:!?])/g, '$1')
    // A clause that ended only because the link did.
    .replace(/([(,;:—–-])\s*([.;,])/g, '$2')
    .replace(/\s{2,}/g, ' ')
    .replace(/^[\s.,;:—–-]+/, '')
    .trim();
  return { prose, links };
}

/**
 * "merics.org/…/2020-04/report.pdf" — the host and the end of the path,
 * with the middle elided. The cut snaps to a path separator so the
 * result never starts mid-word, which reads as a typo rather than a
 * shortening.
 */
export function shortLink(url: string, max = 52): string {
  const bare = url.replace(/^https?:\/\//i, '').replace(/\/$/, '');
  if (bare.length <= max) return bare;
  const slash = bare.indexOf('/');
  const host = slash === -1 ? bare : bare.slice(0, slash);
  const path = slash === -1 ? '' : bare.slice(slash);
  // No path to elide — a bare host is shown whole rather than given a
  // trailing "/…" that points at nothing.
  if (!path) return bare;
  const budget = Math.max(10, max - host.length - 2);
  // Keep whole path segments from the end until the budget runs out; the
  // last segment (the file name) is always worth more than the ones
  // before it, so it is kept even when it alone exceeds the budget.
  const segments = path.split('/').filter(Boolean);
  let tail = '';
  for (let i = segments.length - 1; i >= 0; i--) {
    const next = `/${segments[i]}${tail}`;
    if (next.length > budget && tail) break;
    tail = next;
    if (tail.length >= budget) break;
  }
  if (!tail) tail = path.slice(-budget);
  // A single file name can still blow the budget on its own. Elide ITS
  // middle rather than letting the row clip the end, because the end is
  // where the extension lives and ".pdf" is the most useful character
  // the reader gets.
  if (tail.length > budget) {
    const keepEnd = Math.min(16, Math.max(8, Math.floor(budget / 2)));
    const keepStart = Math.max(4, budget - keepEnd - 1);
    tail = `${tail.slice(0, keepStart)}…${tail.slice(-keepEnd)}`;
  }
  return `${host}/…${tail}`;
}

/**
 * How overdue a reminder is, in words, or null when it is not yet due.
 * Compares the true instant (`due_at`), never the wall clock, so it stays
 * right for a member in another zone.
 */
export function lateness(dueAt: string, now: Date = new Date()): string | null {
  const due = new Date(dueAt).getTime();
  if (Number.isNaN(due)) return null;
  const mins = Math.floor((now.getTime() - due) / 60000);
  if (mins < 1) return null;
  if (mins < 60) return `${mins} min late`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return hours === 1 ? '1 hour late' : `${hours} hours late`;
  const days = Math.floor(hours / 24);
  return days === 1 ? '1 day late' : `${days} days late`;
}
