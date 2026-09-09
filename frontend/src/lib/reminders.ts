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
    const tomorrow = new Date(Date.UTC(ty, tm - 1, td + 1));
    const tomorrowKey = `${tomorrow.getUTCFullYear()}-${String(tomorrow.getUTCMonth() + 1).padStart(2, '0')}-${String(tomorrow.getUTCDate()).padStart(2, '0')}`;
    if (key === tomorrowKey) return 'Tomorrow';
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
