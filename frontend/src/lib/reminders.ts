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

/** Heading for a day group: "Today", "Tomorrow", else a short locale
 *  date such as "Wed 11 Sep" (the year is appended once it differs). */
export function dayLabel(key: string, now: Date = new Date()): string {
  if (key === localDayKey(now)) return 'Today';
  const tomorrow = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1);
  if (key === localDayKey(tomorrow)) return 'Tomorrow';
  const [y, m, d] = key.split('-').map(Number);
  if (!y || !m || !d) return key;
  const opts: Intl.DateTimeFormatOptions = { weekday: 'short', day: 'numeric', month: 'short' };
  if (y !== now.getFullYear()) opts.year = 'numeric';
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
