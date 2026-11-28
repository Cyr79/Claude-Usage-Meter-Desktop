'use strict';

/**
 * Weekly bar split by usage, not time:
 * each past day = what it really used, today = today's budget,
 * future days = what is left, shared evenly.
 * The day log keeps each local day's last pct for this window.
 */

const { localDay, WINDOWS } = require('./pace');

const DAY = 86400;

/** Keep the highest pct seen per local day. Mutates and returns `logs`. */
function updateDayLog(logs, meter, now = Date.now() / 1000) {
  const win = WINDOWS[meter.key];
  if (!win || win.unit !== 'day' || !meter.resetsAt) return logs;
  let log = logs[meter.key];
  if (!log || Math.abs(meter.resetsAt - log.resetsAt) >= 1800 || meter.pct < log.top - 0.5) {
    log = logs[meter.key] = { resetsAt: meter.resetsAt, days: {}, top: meter.pct };
  }
  const day = localDay(now);
  log.days[day] = Math.max(log.days[day] || 0, meter.pct);
  log.top = Math.max(log.top, meter.pct);
  return logs;
}

/** Local days of the window, oldest first (midpoint of each 24 h slice). */
function windowDays(resetsAt, n) {
  const start = resetsAt - n * DAY;
  const out = [];
  for (let i = 0; i < n; i++) {
    const ts = start + (i + 0.5) * DAY;
    out.push({ key: localDay(ts), ts });
  }
  return out;
}

/**
 * Segments in % of the weekly limit: [{ days: [ts], from, to, kind }].
 * null when there is no day baseline yet (caller falls back to 1/7 slices).
 */
function weekSegments(meter, p, log, now = Date.now() / 1000) {
  const win = WINDOWS[meter.key];
  if (!win || win.unit !== 'day' || !meter.resetsAt || !p || p.today === null) return null;
  const n = Math.round(win.seconds / DAY);
  const days = windowDays(meter.resetsAt, n);
  const today = localDay(now);
  const t = days.findIndex((d) => d.key === today);
  if (t < 0) return null;
  const b = Math.max(0, meter.pct - p.today);           // used before today
  const valid = log && Math.abs(meter.resetsAt - log.resetsAt) < 1800;
  const segs = [];
  // One segment per past day; a day with no reading is a 0-width noData segment.
  let from = 0;
  for (let i = 0; i < t; i++) {
    const last = i === t - 1;
    const end = last ? b : valid ? log.days[days[i].key] : undefined;
    const to = end === undefined ? from : Math.min(b, Math.max(from, end));
    segs.push({ days: [days[i].ts], from, to, kind: 'past', noData: end === undefined });
    from = to;
  }
  const budgetEnd = Math.min(100, b + p.allowance);
  segs.push({ days: [days[t].ts], from: b, to: budgetEnd, kind: 'today' });
  const nf = n - t - 1;
  const each = nf > 0 ? Math.max(0, 100 - budgetEnd) / nf : 0;
  for (let i = 0; i < nf; i++) {
    const a = budgetEnd + i * each;
    segs.push({ days: [days[t + 1 + i].ts], from: a, to: a + each, kind: 'future' });
  }
  return segs;
}

module.exports = { updateDayLog, weekSegments };
