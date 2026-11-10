'use strict';

/**
 * Pace model: is usage on track to last until the window resets?
 *
 * Target rate = 100% / window length (7 days -> 14.29%/day, 5h -> 20%/h).
 * Expected now = elapsed * target. Over pace = actual clearly above expected.
 * "Today" is measured against a baseline taken at the first reading of the
 * local day (or right after a reset), so it is the real burn since midnight.
 */

const DAY = 86400;
const WINDOWS = {
  session: { seconds: 5 * 3600, unit: 'h', unitSeconds: 3600 },
  weekly: { seconds: 7 * DAY, unit: 'day', unitSeconds: DAY },
  weekly_opus: { seconds: 7 * DAY, unit: 'day', unitSeconds: DAY },
  fable: { seconds: 7 * DAY, unit: 'day', unitSeconds: DAY },
};
// Small slack so 1% after 10 minutes doesn't flash red.
const SLACK_PTS = 2;

function localDay(ts) {
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/**
 * Keep per-meter day baselines. Mutates and returns `baselines`.
 * A new local day, a reset (resetsAt jumped), or pct dropping below the
 * baseline all restart the baseline at the current reading.
 */
function updateBaseline(baselines, meter, now = Date.now() / 1000) {
  const day = localDay(now);
  const b = baselines[meter.key];
  const reset = b && meter.resetsAt && b.resetsAt && meter.resetsAt - b.resetsAt >= 1800;
  if (!b || b.day !== day || reset || meter.pct < b.pct) {
    baselines[meter.key] = { day, pct: meter.pct, at: now, resetsAt: meter.resetsAt };
  } else if (meter.resetsAt && Math.abs(meter.resetsAt - (b.resetsAt || 0)) >= 60) {
    // resets_at jitters by under a second per poll; ignore that noise.
    b.resetsAt = meter.resetsAt;
  }
  return baselines;
}

function pace(meter, baseline, now = Date.now() / 1000) {
  const win = WINDOWS[meter.key];
  if (!win || !meter.resetsAt) return null;
  const start = meter.resetsAt - win.seconds;
  const elapsed = Math.min(win.seconds, Math.max(0, now - start));
  const left = Math.max(0, meter.resetsAt - now);
  const units = win.seconds / win.unitSeconds;
  const target = 100 / units;                                 // % per unit
  const elapsedUnits = elapsed / win.unitSeconds;
  const leftUnits = left / win.unitSeconds;
  const expected = Math.min(100, elapsedUnits * target);      // % by now
  const avg = meter.pct / Math.max(win.unit === 'day' ? 0.1 : 0.25, elapsedUnits);
  const budget = leftUnits > 0.01 ? (100 - meter.pct) / leftUnits : null; // % per unit to finish at 100
  const overPace = meter.pct > expected + SLACK_PTS;

  // Today's allowance = what was spendable per day at the start of today so
  // the window lasts to reset ("not get stopped"). Falls back to 100%/7.
  let today = null;
  let todayOver = false;
  let todaySince = null;
  let allowance = target;
  if (win.unit === 'day' && baseline && baseline.day === localDay(now)) {
    today = Math.max(0, meter.pct - baseline.pct);
    todaySince = baseline.at;
    const daysLeftAtStart = (meter.resetsAt - baseline.at) / DAY;
    if (daysLeftAtStart > 0.05) allowance = Math.min(100, (100 - baseline.pct) / Math.max(1, daysLeftAtStart));
    todayOver = today > allowance;
  }

  return {
    unit: win.unit,
    target,
    expected,
    avg,
    budget,
    leftUnits,
    leftPct: 100 - meter.pct,
    leftText: dur(left),
    delta: meter.pct - expected,
    overPace,
    today,
    todaySince,
    todayOver,
    allowance,
    hot: overPace || todayOver,
  };
}

function dur(sec) {
  const m = Math.max(0, Math.round(sec / 60));
  const d = Math.floor(m / 1440);
  const h = Math.floor((m % 1440) / 60);
  if (d) return `${d}d ${h}h`;
  return h ? `${h}h ${m % 60}m` : `${m % 60}m`;
}

/** "Left 11% for 6h 51m", plus a per-day rate only when >1 day remains. */
function leftLine(p) {
  const rate = p.unit === 'day' && p.leftUnits >= 1 ? ` (~${f1(p.budget)}%/day)` : '';
  return `Left ${f1(p.leftPct)}% for ${p.leftText}${rate}`;
}

const f1 = (n) => (Math.round(n * 10) / 10).toFixed(1);

/** Plain-text lines for tooltips/menus. Emoji carry the colour Windows can't. */
function paceLines(p) {
  if (!p) return [];
  const u = p.unit;
  const lines = [
    `${p.overPace ? '🔴' : '🟢'} Avg ${f1(p.avg)}%/${u} vs target ${f1(p.target)}%/${u}`
      + ` (${p.delta >= 0 ? '+' : ''}${f1(p.delta)} pts vs pace)`,
  ];
  if (p.today !== null) {
    lines.push(`${p.todayOver ? '🔴' : '🟢'} Today +${f1(p.today)}% · allowed ${f1(p.allowance)}% (100/7 = ${f1(p.target)}%)`);
  }
  if (p.budget !== null) {
    lines.push(leftLine(p));
  }
  return lines;
}

/** Compact lines for the 127-char Windows tray tooltip. */
function tipLines(p) {
  if (!p) return [];
  const u = p.unit === 'day' ? 'd' : 'h';
  const lines = [`${p.overPace ? '🔴' : '🟢'} Avg ${f1(p.avg)}%/${u} · target ${f1(p.target)}`];
  if (p.today !== null) lines.push(`${p.todayOver ? '🔴' : '🟢'} Today +${f1(p.today)}% (max ${f1(p.allowance)})`);
  if (p.budget !== null) {
    const rate = p.unit === 'day' && p.leftUnits >= 1 ? ` (${f1(p.budget)}%/d)` : '';
    lines.push(`Left ${f1(p.leftPct)}% · ${p.leftText}${rate}`);
  }
  return lines;
}

module.exports = { leftLine, pace, paceLines, tipLines, updateBaseline, localDay, WINDOWS };
