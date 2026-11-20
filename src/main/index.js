'use strict';

const path = require('path');
const {
  app, Tray, Menu, nativeImage, shell, dialog, clipboard, Notification,
  BrowserWindow, ipcMain, screen,
} = require('electron');
const { autoUpdater } = require('electron-updater');

const { Store } = require('./store');
const { Fetcher, AuthError, ORIGIN } = require('./fetcher');
const { extractMeters, LABELS, CANONICAL_ORDER } = require('./parse');
const { History, forecast, untilReset, resetDateTime } = require('./forecast');
const { macTitle, tooltip, iconState, severity, chosenMeters, STYLE_PREVIEWS } = require('./format');
const { renderBadge } = require('./trayicon');
const { pace, paceLines, tipLines, updateBaseline } = require('./pace');
const { readContext } = require('./context');

const IS_MAC = process.platform === 'darwin';
const STALE_AFTER = 15 * 60 * 1000;
const ASSETS = path.join(__dirname, '..', '..', 'assets', 'tray');

// A tray app with one small popup gains nothing from GPU compositing.
// Measured on Windows 11: -50..75 MB private memory, pinned-gauge CPU 0.63% -> 0.23%.
app.disableHardwareAcceleration();

// Only one instance may own the tray icon, or the user gets duplicates.
if (!app.requestSingleInstanceLock()) app.quit();

// macOS: no Dock icon. Windows shows numbered usage badges in the taskbar.
if (IS_MAC && app.dock) app.dock.hide();

let tray = null;
const meterTrays = new Map();
let store = null;
let fetcher = null;
let pollTimer = null;
let gauge = null;
let gaugeHiddenAt = 0;
let contextTray = null;
let lastContextKey = '';
let lastIconState = null;
let contextLook = '';
const badgeLooks = new Map();

const state = {
  meters: [],
  histories: new Map(),
  lastOkAt: null,
  lastError: null,
  needsLogin: false,
  context: null,
};

const isWeekly = (meter) => meter.key === 'weekly' || meter.key === 'weekly_opus' || meter.key === 'fable';

function meterPace(meter) {
  const baselines = store.get('dayBaselines') || {};
  return pace(meter, baselines[meter.key]);
}

function resetText(meter) {
  return isWeekly(meter) ? resetDateTime(meter.resetsAt) : untilReset(meter.resetsAt);
}

const isStale = () => !state.lastOkAt || Date.now() - state.lastOkAt > STALE_AFTER;

function trayImage(name) {
  if (IS_MAC) {
    // The menu bar already shows "S:42% W:18%". An icon in front of that is
    // clutter for most people, so it's opt-in.
    if (store && store.get('macTrayIcon') !== 'logo') return nativeImage.createEmpty();
    const file = path.join(ASSETS, 'trayTemplate.png');
    const img = nativeImage.createFromPath(file);
    if (img.isEmpty()) {
      // Silently showing nothing here is what makes "no icon" hard to diagnose.
      console.error(`Tray icon missing or unreadable: ${file}`);
      return nativeImage.createEmpty();
    }
    // Template = black + alpha; macOS recolours it for light/dark/highlighted.
    img.setTemplateImage(true);
    return img;
  }

  // Windows has no text in the tray, so the icon is the entire reading and
  // must never be empty — an empty image means an invisible tray entry.
  const img = nativeImage.createFromPath(path.join(ASSETS, `${name}.png`));
  return img.isEmpty() ? nativeImage.createFromPath(path.join(ASSETS, 'normal.png')) : img;
}

function notify(title, body) {
  if (Notification.isSupported()) new Notification({ title, body }).show();
}

// --------------------------------------------------------------- polling

async function poll() {
  try {
    const { raw } = await fetcher.poll();
    applyResult(raw);
  } catch (err) {
    if (err instanceof AuthError) {
      state.needsLogin = true;
      state.lastError = err.message;
    } else {
      // A transient failure must not be reported as "signed out" — that flag is
      // only ever set by an actual server rejection.
      state.lastError = err.message;
    }
    render();
  }
}

function applyResult(raw) {
  const meters = extractMeters(raw);
  const windowMinutes = store.get('forecastWindowMinutes');
  const now = Date.now() / 1000;

  for (const meter of meters) {
    let history = state.histories.get(meter.key);
    if (!history) {
      history = new History();
      state.histories.set(meter.key, history);
    }
    if (history.add(meter.pct, meter.resetsAt, windowMinutes, now)) {
      notify('Claude Usage Meter', `${meter.label} window reset — allowance refreshed.`);
    }
  }

  const baselines = { ...(store.get('dayBaselines') || {}) };
  for (const meter of meters) updateBaseline(baselines, meter, now);
  store.set('dayBaselines', baselines);

  state.meters = meters;
  state.lastOkAt = Date.now();
  state.needsLogin = false;
  state.lastError = meters.length
    ? null
    : 'No usage windows in the response. Free plans do not expose /usage.';
  render();
}

function schedulePolling() {
  if (pollTimer) clearInterval(pollTimer);
  const seconds = Math.max(20, Number(store.get('pollSeconds')) || 120);
  pollTimer = setInterval(poll, seconds * 1000);
}

// ------------------------------------------------------------------ menu

function meterSubmenu(meter) {
  const history = state.histories.get(meter.key) || new History();
  const weekly = isWeekly(meter);
  const items = [
    { label: `Resets ${resetText(meter)}`, enabled: false },
    { label: forecast(meter.pct, history, meter.resetsAt), enabled: false },
    ...paceLines(meterPace(meter)).map((label) => ({ label, enabled: false })),
  ];
  if (meter.resetsAt && !weekly) {
    items.push({
      label: new Date(meter.resetsAt * 1000).toLocaleString(),
      enabled: false,
    });
  }
  items.push({ type: 'separator' }, { label: `source: ${meter.path}`, enabled: false });
  return items;
}

function buildMenu() {
  const items = [
    { label: '⚡ Show energy gauge', click: showGauge },
    {
      label: '📌 Keep gauge on screen (always on top)',
      type: 'checkbox',
      checked: !!store.get('gaugePinned'),
      click: (item) => { setPinned(item.checked); if (item.checked) showGauge(); },
    },
    { type: 'separator' },
  ];

  if (state.needsLogin) {
    items.push(
      { label: 'Not signed in', enabled: false },
      { label: 'Sign in to Claude…', click: signIn },
      { type: 'separator' },
    );
  } else if (!state.meters.length) {
    items.push({ label: 'Waiting for first reading…', enabled: false }, { type: 'separator' });
  } else {
    for (const meter of state.meters) {
      items.push({
        label: `${meter.label}   ${Math.round(meter.pct)}%`,
        submenu: meterSubmenu(meter),
      });
    }
    items.push({ type: 'separator' });
  }
  const ctx = state.context;
  if (store.get('showContext') && ctx && ctx.available) {
    items.push({
      label: `Claude Code context   ${Math.round(ctx.pct)}%  (${Math.round(ctx.tokens / 1000)}k/${Math.round(ctx.limit / 1000)}k${ctx.active ? '' : ', idle'})`,
      enabled: false,
    }, { type: 'separator' });
  }

  items.push({
    label: state.lastOkAt
      ? `Updated ${new Date(state.lastOkAt).toLocaleTimeString()}`
      : 'Updated never',
    enabled: false,
  });
  if (state.lastError) {
    items.push({ label: `⚠ ${state.lastError.slice(0, 60)}`, enabled: false });
  }
  items.push({ label: 'Refresh now', click: poll }, { type: 'separator' });

  const display = [];
  if (IS_MAC) {
    display.push({
      label: 'Title style',
      submenu: Object.entries(STYLE_PREVIEWS).map(([key, preview]) => ({
        label: preview,
        type: 'radio',
        checked: store.get('titleStyle') === key,
        click: () => { store.set('titleStyle', key); render(); },
      })),
    });
  }
  if (IS_MAC) {
    display.push({
      label: 'Show logo in menu bar',
      type: 'checkbox',
      checked: store.get('macTrayIcon') === 'logo',
      click: (item) => {
        store.set('macTrayIcon', item.checked ? 'logo' : 'none');
        render();
      },
    });
    display.push({ type: 'separator' });
  }
  if (process.platform === 'win32') {
    display.push({
      label: 'Taskbar badges',
      submenu: [
        {
          label: 'One badge per meter',
          type: 'radio',
          checked: store.get('windowsTrayMode') !== 'single',
          click: () => { store.set('windowsTrayMode', 'per-meter'); destroyMeterTrays(); render(); },
        },
        {
          label: 'Single badge (closest to limit)',
          type: 'radio',
          checked: store.get('windowsTrayMode') === 'single',
          click: () => { store.set('windowsTrayMode', 'single'); destroyMeterTrays(); render(); },
        },
      ],
    });
    display.push({ type: 'separator' });
  }
  display.push(
    {
      label: 'Red badge when over daily pace',
      type: 'checkbox',
      checked: store.get('paceColours') !== false,
      click: (item) => { store.set('paceColours', item.checked); render(); },
    },
    {
      label: 'Claude Code context gauge',
      type: 'checkbox',
      checked: !!store.get('showContext'),
      click: (item) => { store.set('showContext', item.checked); pollContext(true); },
    },
    {
      label: 'Context window size',
      submenu: [['auto', 'Auto (200k, 1M if larger)'], [200000, '200k'], [1000000, '1M']].map(([v, label]) => ({
        label,
        type: 'radio',
        checked: String(store.get('contextLimit')) === String(v),
        click: () => { store.set('contextLimit', v); pollContext(true); },
      })),
    },
    { type: 'separator' },
  );
  for (const key of CANONICAL_ORDER) {
    display.push({
      // No data for this limit on the account -> greyed out, not a dead tick.
      label: `Show ${LABELS[key]}` + (state.meters.length && !state.meters.some((m) => m.key === key)
        ? '  (no separate limit on your account)' : ''),
      enabled: !state.meters.length || state.meters.some((m) => m.key === key),
      type: 'checkbox',
      checked: (store.get('show') || []).includes(key),
      click: () => toggleShow(key),
    });
  }
  items.push({ label: 'Display', submenu: display });

  items.push(
    { label: 'Open Claude Desktop', click: openClaudeDesktop },
    { label: 'Open claude.ai', click: () => shell.openExternal(ORIGIN) },
    { label: 'Sign in again…', click: signIn },
    { label: 'Copy diagnostics', click: copyDiagnostics },
    {
      label: 'Start at login',
      type: 'checkbox',
      checked: app.getLoginItemSettings().openAtLogin,
      click: (item) => setLoginItem(item.checked),
    },
    { type: 'separator' },
    { label: 'About Claude Usage Meter…', click: showAbout },
    { label: 'Quit', click: () => app.quit() },
  );

  return Menu.buildFromTemplate(items);
}

function render() {
  if (!tray) return;
  const stale = isStale();

  if (IS_MAC) {
    tray.setTitle(state.needsLogin ? 'Claude — sign in' : macTitle(state.meters, store));
  }
  // One menu per render, shared by every tray icon (was rebuilt per badge).
  const menu = buildMenu();
  const icon = iconState(state.meters, store, stale);
  if (icon !== lastIconState) { tray.setImage(trayImage(icon)); lastIconState = icon; }
  if (process.platform === 'win32') {
    if (state.meters.length && !state.needsLogin) renderWindowsTrays(stale, menu);
    else destroyMeterTrays();
  }
  tray.setToolTip(
    state.needsLogin
      ? 'Claude Usage Meter — click to sign in'
      : tooltip(state.meters, store, stale),
  );
  tray.setContextMenu(menu);
  if (process.platform === 'win32') renderContextTray(menu);
  pushGauge();
}

// ------------------------------------------------- Claude Desktop / About

/**
 * Launch the Claude desktop app via its own claude:// link (registered by both
 * the Store/MSIX and the older installer). No paths, no shell commands.
 * Not installed -> open claude.ai in the browser instead.
 */
function openClaudeDesktop() {
  if (app.getApplicationNameForProtocol('claude://')) {
    shell.openExternal('claude://').catch(() => shell.openExternal(ORIGIN));
  } else {
    shell.openExternal(ORIGIN);
  }
}

const REPO_URL = 'https://github.com/Cyr79/Claude-Usage-Meter-Desktop';

function showAbout() {
  const v = process.versions;
  dialog.showMessageBox({
    type: 'info',
    title: 'About Claude Usage Meter',
    message: `Claude Usage Meter ${app.getVersion()}`,
    detail: [
      'Your Claude.ai usage limits in the tray, with daily pace and live Claude Code context.',
      '',
      `Fork maintained by Cyr79 — ${REPO_URL}`,
      'Based on the original Claude Usage Meter by SelectorsHub.',
      'Independent project, not affiliated with Anthropic.',
      '',
      'Privacy: talks only to claude.ai with your own sign-in and to GitHub for updates.',
      'Your login stays on this PC; nothing is sent anywhere else.',
      '',
      `Electron ${v.electron} · Chromium ${v.chrome} · Node ${v.node}`,
    ].join('\n'),
    buttons: ['OK', 'Open on GitHub'],
    defaultId: 0,
    cancelId: 0,
    noLink: true,
  }).then(({ response }) => { if (response === 1) shell.openExternal(REPO_URL); });
}

// ----------------------------------------------------------- energy gauge

function gaugeData() {
  return {
    needsLogin: state.needsLogin,
    pinned: !!store.get('gaugePinned'),
    error: state.lastError,
    updated: state.lastOkAt,
    context: { enabled: !!store.get('showContext'), ...(state.context || {}) },
    meters: chosenMeters(state.meters, store).map((meter) => ({
      key: meter.key,
      label: meter.label,
      pct: meter.pct,
      resetText: meter.resetsAt ? resetText(meter) : 'unknown',
      forecast: forecast(meter.pct, state.histories.get(meter.key) || new History(), meter.resetsAt),
      pace: meterPace(meter),
    })),
  };
}

function pushGauge() {
  if (gauge && !gauge.isDestroyed() && gauge.isVisible()) {
    gauge.webContents.send('gauge:data', gaugeData());
    // Pinned gauge is live: content height changes (context bar appears etc.).
    if (store.get('gaugePinned')) fitGauge();
  }
}

function createGauge() {
  gauge = new BrowserWindow({
    width: 360,
    height: 420,
    show: false,
    frame: false,
    resizable: false,
    transparent: true,
    skipTaskbar: true,
    alwaysOnTop: true,
    fullscreenable: false,
    webPreferences: {
      preload: path.join(__dirname, 'gauge-preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  if (store.get('gaugePinned')) gauge.setAlwaysOnTop(true, 'screen-saver');
  gauge.loadFile(path.join(__dirname, '..', 'renderer', 'gauge.html'));
  gauge.on('blur', () => { if (!store.get('gaugePinned')) closeGauge(); });
  gauge.on('closed', () => { gauge = null; });
  gauge.webContents.on('did-finish-load', () => { fitGauge(); pushGauge(); });
  gauge.on('moved', () => {
    if (!store.get('gaugePinned')) return;
    const [x, y] = gauge.getPosition();
    store.set('gaugePos', { x, y });
  });
}

/** Unpinned gauge is destroyed, not hidden: a hidden renderer still costs ~90 MB. */
function closeGauge() {
  gaugeHiddenAt = Date.now();
  if (gauge && !gauge.isDestroyed()) gauge.destroy();
  gauge = null;
}

function setPinned(pinned) {
  store.set('gaugePinned', pinned);
  if (gauge && !gauge.isDestroyed()) {
    if (pinned) {
      const [x, y] = gauge.getPosition();
      store.set('gaugePos', { x, y });
    }
    gauge.setAlwaysOnTop(true, pinned ? 'screen-saver' : 'floating');
  }
  render();
}

function fitGauge() {
  gauge.webContents.executeJavaScript('document.querySelector(".card").getBoundingClientRect().height')
    .then((h) => {
      if (!gauge || gauge.isDestroyed()) return;
      const [w] = gauge.getSize();
      gauge.setSize(w, Math.min(720, Math.ceil(h) + 2));
      placeGauge();
    })
    .catch(() => {});
}

function placeGauge() {
  const saved = store.get('gaugePos');
  if (store.get('gaugePinned') && saved) {
    const { workArea } = screen.getDisplayNearestPoint(saved);
    const [w, h] = gauge.getSize();
    gauge.setPosition(
      Math.max(workArea.x, Math.min(saved.x, workArea.x + workArea.width - w)),
      Math.max(workArea.y, Math.min(saved.y, workArea.y + workArea.height - h)),
    );
    return;
  }
  const bounds = tray ? tray.getBounds() : { x: 0, y: 0, width: 0, height: 0 };
  const point = bounds.width ? { x: bounds.x, y: bounds.y } : screen.getCursorScreenPoint();
  const { workArea } = screen.getDisplayNearestPoint(point);
  const [w, h] = gauge.getSize();
  let x = Math.round(point.x + (bounds.width || 0) / 2 - w / 2);
  // Taskbar at the bottom (usual) -> above the icon; at the top -> below it.
  let y = point.y > workArea.y + workArea.height / 2 ? workArea.y + workArea.height - h - 8 : workArea.y + 8;
  x = Math.max(workArea.x + 8, Math.min(x, workArea.x + workArea.width - w - 8));
  y = Math.max(workArea.y, y);
  gauge.setPosition(x, y);
}

function showGauge() {
  if (!gauge || gauge.isDestroyed()) createGauge();
  placeGauge();
  // Pinned (incl. at startup): appear without stealing focus from your work.
  if (store.get('gaugePinned')) gauge.showInactive();
  else { gauge.show(); gauge.focus(); }
  pushGauge();
  fitGauge();
}

function toggleGauge() {
  // A click on the tray blurs the popup first; don't instantly reopen it.
  if (Date.now() - gaugeHiddenAt < 350) return;
  if (gauge && !gauge.isDestroyed() && gauge.isVisible()) closeGauge();
  else showGauge();
}

// --------------------------------------------------- Claude Code context

function pollContext(force = false) {
  if (!store.get('showContext')) {
    state.context = null;
    if (contextTray) { contextTray.destroy(); contextTray = null; }
    if (force) render();
    return;
  }
  const ctx = readContext(store.get('contextLimit'));
  state.context = ctx;
  const key = ctx.available ? `${Math.round(ctx.pct)}|${ctx.active}|${ctx.tokens}` : `x|${ctx.reason}`;
  if (force || key !== lastContextKey) {
    lastContextKey = key;
    render();
  }
}

function renderContextTray(menu) {
  const ctx = state.context;
  const want = store.get('showContext') && store.get('contextBadge') !== false && ctx && ctx.available && ctx.active;
  if (!want) {
    if (contextTray) { contextTray.destroy(); contextTray = null; }
    return;
  }
  const role = ctx.pct >= 90 ? 'danger' : ctx.pct >= 70 ? 'warn' : 'context';
  const look = `${Math.round(ctx.pct)}|${role}`;
  if (!contextTray || look !== contextLook) {
    const { buffer, width, height } = renderBadge(ctx.pct, role, 32);
    const image = nativeImage.createFromBitmap(buffer, { width, height, scaleFactor: 2 });
    if (!contextTray) {
      contextTray = new Tray(image);
      contextTray.on('click', toggleGauge);
    } else {
      contextTray.setImage(image);
    }
    contextLook = look;
  }
  contextTray.setToolTip(`Claude Code context — ${Math.round(ctx.pct)}%\n`
    + `${Math.round(ctx.tokens / 1000)}k / ${Math.round(ctx.limit / 1000)}k tokens`);
  contextTray.setContextMenu(menu || buildMenu());
}

// ------------------------------------------------------------- callbacks

function toggleShow(key) {
  const show = [...(store.get('show') || [])];
  const index = show.indexOf(key);
  if (index >= 0) show.splice(index, 1);
  else show.push(key);
  show.sort((a, b) => CANONICAL_ORDER.indexOf(a) - CANONICAL_ORDER.indexOf(b));
  store.set('show', show);
  render();
}

function setLoginItem(enabled) {
  store.set('startAtLogin', enabled);
  app.setLoginItemSettings({
    openAtLogin: enabled,
    openAsHidden: true,
    // Windows: start minimised to tray rather than flashing a window.
    args: IS_MAC ? [] : ['--hidden'],
  });
}

function chosenForTray() {
  const selected = chosenMeters(state.meters, store);
  if (store.get('windowsTrayMode') === 'single') {
    return selected.length
      ? [selected.reduce((a, b) => (b.pct > a.pct ? b : a))]
      : [];
  }
  return selected;
}

function renderWindowsTrays(stale, menu) {
  const selected = chosenForTray();
  const wanted = new Set(selected.map((meter) => meter.key));

  for (const [key, meterTray] of meterTrays) {
    if (!wanted.has(key)) {
      meterTray.destroy();
      meterTrays.delete(key);
      badgeLooks.delete(key);
    }
  }

  for (const meter of selected) {
    const p = meterPace(meter);
    let role = stale
      ? 'stale'
      : severity(meter.pct, store.get('warnAt'), store.get('dangerAt'));
    if (!stale && p && p.hot && store.get('paceColours') !== false) role = 'danger';
    // Redraw the badge only when its number or colour changed.
    const look = `${Math.round(meter.pct)}|${role}`;
    let meterTray = meterTrays.get(meter.key);
    if (!meterTray || badgeLooks.get(meter.key) !== look) {
      const { buffer, width, height } = renderBadge(meter.pct, role, 32);
      const image = nativeImage.createFromBitmap(buffer, { width, height, scaleFactor: 2 });
      if (!meterTray) {
        meterTray = new Tray(image);
        meterTray.on('click', toggleGauge);
        meterTrays.set(meter.key, meterTray);
      } else {
        meterTray.setImage(image);
      }
      badgeLooks.set(meter.key, look);
    }
    const head = `${meter.label} — ${Math.round(meter.pct)}%`
      + (meter.resetsAt ? `, resets ${isWeekly(meter) ? resetDateTime(meter.resetsAt, true) : untilReset(meter.resetsAt)}` : '');
    meterTray.setToolTip(fitTip([head, ...tipLines(p)]));
    meterTray.setContextMenu(menu);
  }
}

/** Windows truncates tray tooltips at 127 UTF-16 units; drop whole lines instead. */
function fitTip(lines, max = 127) {
  const out = [];
  for (const line of lines) {
    const next = [...out, line].join('\n');
    if (next.length > max) break;
    out.push(line);
  }
  return out.join('\n') || lines[0].slice(0, max);
}

function destroyMeterTrays() {
  for (const meterTray of meterTrays.values()) meterTray.destroy();
  meterTrays.clear();
  badgeLooks.clear();
}

function signIn() {
  state.lastError = 'Sign-in window open…';
  render();
  fetcher.signIn((ok) => {
    state.needsLogin = !ok;
    state.lastError = ok ? null : 'Sign-in was not confirmed. Try again.';
    render();
    if (ok) poll();
  });
}

function copyDiagnostics() {
  clipboard.writeText(JSON.stringify({
    version: app.getVersion(),
    platform: `${process.platform} ${process.arch}`,
    electron: process.versions.electron,
    needsLogin: state.needsLogin,
    lastError: state.lastError,
    meters: state.meters.map((m) => [m.key, m.pct, m.path]),
    lastFetch: fetcher.lastDiagnostic,
  }, null, 2));
  notify('Claude Usage Meter', 'Diagnostics copied to clipboard.');
}

function firstRun() {
  if (store.get('firstRunDone')) return;
  store.set('firstRunDone', true);
  setLoginItem(true);
  dialog.showMessageBox({
    type: 'info',
    title: 'Claude Usage Meter',
    message: 'Welcome',
    detail:
      'A window will open once so you can sign in to Claude. After that the '
      + 'meter runs in the background and you will not see it again.\n\n'
      + (IS_MAC
        ? 'Your usage appears in the menu bar.'
        : 'Your usage percentages appear in the system tray. Click a badge for the energy gauge, hover for pace, right-click for the menu.'),
    buttons: ['Sign in'],
  }).then(signIn);
}

// ------------------------------------------------------------------ boot

app.whenReady().then(() => {
  store = new Store();
  fetcher = new Fetcher(store);

  // store must exist first: trayImage() reads macTrayIcon from it.
  tray = new Tray(trayImage('stale'));
  tray.setToolTip('Claude Usage Meter');
  // Windows convention: left-click opens the menu too, since there's no title
  // to click and users won't guess that right-click is the only way in.
  if (!IS_MAC) tray.on('click', toggleGauge);
  ipcMain.on('gauge:refresh', () => { poll(); pollContext(true); });
  ipcMain.on('gauge:hide', () => { if (store.get('gaugePinned')) setPinned(false); closeGauge(); });
  ipcMain.on('gauge:pin', () => setPinned(!store.get('gaugePinned')));
  ipcMain.on('gauge:claude', openClaudeDesktop);
  if (store.get('gaugePinned')) setTimeout(showGauge, 1500);
  render();

  firstRun();
  if (store.get('firstRunDone') && !state.needsLogin) poll();
  schedulePolling();

  // Keeps the clock-relative labels ("resets in 2h 10m") honest between polls.
  setInterval(render, 30000);
  // Context changes every turn; a stat + 512KB tail read every 5s is cheap.
  pollContext(true);
  setInterval(() => pollContext(false), 5000);

  autoUpdater.checkForUpdatesAndNotify().catch(() => {});
});

app.on('second-instance', () => tray && showGauge());
// The tray app has no windows by design; closing them must not quit it.
app.on('window-all-closed', (e) => e.preventDefault());
app.on('before-quit', () => {
  destroyMeterTrays();
  if (contextTray) contextTray.destroy();
  if (fetcher) fetcher.destroy();
});
