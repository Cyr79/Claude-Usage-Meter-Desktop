'use strict';

/**
 * Holds a logged-in Claude session in a hidden BrowserWindow and fetches usage.
 *
 * This replaces Playwright entirely. Electron already ships Chromium, the
 * session persists in a named partition, and the fetch runs inside the page so
 * cookies, headers and origin are all the real thing.
 */

const { BrowserWindow, session } = require('electron');

const ORIGIN = 'https://claude.ai';
const PARTITION = 'persist:claude';

// Electron's default User-Agent contains "Electron/x.y.z", which bot filters
// flag. That produces a 403 on /api/* even with a perfectly valid cookie —
// indistinguishable from being signed out. Strip it to a plain Chrome UA.
function cleanUserAgent() {
  const raw = session.fromPartition(PARTITION).getUserAgent();
  return raw
    .replace(/ Electron\/[\d.]+/, '')
    .replace(/ ClaudeUsageMeter\/[\d.]+/, '');
}

const JS_ORGS = `
(async () => {
  const r = await fetch('/api/organizations', {
    credentials: 'include', headers: { Accept: 'application/json' }
  });
  const text = await r.text();
  let body = null;
  try { body = JSON.parse(text); } catch (e) {}
  return { status: r.status, body, sample: text.slice(0, 300) };
})()`;

const JS_USAGE = (orgId) => `
(async () => {
  const r = await fetch('/api/organizations/${orgId}/usage', {
    credentials: 'include', headers: { Accept: 'application/json' }
  });
  const text = await r.text();
  let body = null;
  try { body = JSON.parse(text); } catch (e) {}
  return { status: r.status, body, sample: text.slice(0, 300) };
})()`;

class AuthError extends Error {}

// After the direct path fails (bot check), use the browser window for a while.
const FALLBACK_MS = 30 * 60 * 1000;

class Fetcher {
  constructor(store) {
    this.store = store;
    this.window = null;
    this.orgUuid = store.get('orgUuid') || null;
    this.lastDiagnostic = {};
    this.fallbackUntil = 0;
  }

  /**
   * GET an /api path straight through the session's Chromium network stack:
   * same cookies, same TLS fingerprint, same UA as the window — but no page,
   * no renderer process. That renderer was ~250 MB just to hold the session.
   */
  async directGet(path) {
    const ses = session.fromPartition(PARTITION);
    const r = await ses.fetch(ORIGIN + path, {
      credentials: 'include',
      headers: {
        Accept: 'application/json',
        'User-Agent': cleanUserAgent(),
        Referer: ORIGIN + '/',
        Origin: ORIGIN,
      },
    });
    const text = await r.text();
    let body = null;
    try { body = JSON.parse(text); } catch (e) { /* challenge page or empty */ }
    return { status: r.status, body, sample: text.slice(0, 300), json: body !== null };
  }

  windowGet(win) {
    return (path) => this.run(win, path.endsWith('/usage')
      ? JS_USAGE(path.split('/')[3])
      : JS_ORGS);
  }

  createWindow({ show = false } = {}) {
    const win = new BrowserWindow({
      width: 1100,
      height: 800,
      show,
      skipTaskbar: !show,
      title: 'Claude Usage Meter',
      webPreferences: {
        partition: PARTITION,
        // The page is claude.ai, not our code. No node, no preload, isolated.
        nodeIntegration: false,
        contextIsolation: true,
        sandbox: true,
      },
    });
    win.setMenuBarVisibility(false);
    win.webContents.setUserAgent(cleanUserAgent());
    // A sign-in window the user closes shouldn't kill the app's only session.
    win.on('closed', () => {
      if (this.window === win) this.window = null;
    });
    return win;
  }

  async ensureWindow() {
    if (this.window && !this.window.isDestroyed()) return this.window;
    this.window = this.createWindow({ show: false });
    await this.window.loadURL(ORIGIN + '/');
    return this.window;
  }

  async run(win, script) {
    return win.webContents.executeJavaScript(script, true);
  }

  async resolveOrg(get, { force = false } = {}) {
    if (!force) {
      const pinned = this.store.get('orgUuid');
      if (pinned) return pinned;
      if (this.orgUuid) return this.orgUuid;
    }

    const res = await get('/api/organizations');
    this.lastDiagnostic.organizations = { status: res.status, sample: res.sample };

    if (res.status === 401 || res.status === 403) {
      throw new AuthError(`Session rejected by /api/organizations (HTTP ${res.status}).`);
    }
    if (res.status !== 200) {
      throw new Error(`/api/organizations returned HTTP ${res.status}.`);
    }
    const orgs = res.body;
    if (!Array.isArray(orgs) || orgs.length === 0) {
      throw new AuthError('No organizations returned — session looks signed out.');
    }

    const score = (org) => {
      const caps = (org.capabilities || []).map((c) => String(c).toLowerCase());
      return (caps.includes('chat') ? 2 : 0) - (caps.includes('api') ? 1 : 0);
    };
    const best = orgs.reduce((a, b) => (score(b) > score(a) ? b : a));
    this.orgUuid = best.uuid || best.id;
    this.store.set('orgUuid', this.orgUuid);
    return this.orgUuid;
  }

  async fetchUsage(get, orgId) {
    const res = await get(`/api/organizations/${orgId}/usage`);
    this.lastDiagnostic.usage = { status: res.status, sample: res.sample };
    if (res.status === 401 || res.status === 403) {
      throw new AuthError(`Session rejected by /usage (HTTP ${res.status}).`);
    }
    if (res.status !== 200) throw new Error(`Usage API returned HTTP ${res.status}.`);
    return res.body;
  }

  /** One full poll. Throws AuthError when the session is genuinely rejected. */
  async poll() {
    if (Date.now() >= this.fallbackUntil) {
      try {
        const get = (p) => this.directGet(p);
        // A bot-check page is HTML, not JSON: treat as "direct path blocked".
        const guard = async (p) => {
          const res = await get(p);
          if (!res.json && res.status !== 401) throw Object.assign(new Error('direct blocked'), { res });
          return res;
        };
        const org = await this.resolveOrg(guard);
        const raw = await this.fetchUsage(guard, org);
        this.lastDiagnostic.mode = 'direct';
        // Direct works: the hidden page is dead weight, free it.
        if (this.window && !this.window.isDestroyed() && !this.window.isVisible()) this.destroy();
        return { raw, orgUuid: org };
      } catch (err) {
        // 401 = really signed out. 403 may be the bot filter, so verify below.
        if (err instanceof AuthError && /HTTP 401/.test(err.message)) throw err;
        this.lastDiagnostic.directError = err.message;
        this.fallbackUntil = Date.now() + FALLBACK_MS;
      }
    }
    const win = await this.ensureWindow();
    if (!win.webContents.getURL().startsWith(ORIGIN)) {
      await win.loadURL(ORIGIN + '/');
    }
    const get = this.windowGet(win);
    const org = await this.resolveOrg(get);
    this.lastDiagnostic.mode = 'window';
    return { raw: await this.fetchUsage(get, org), orgUuid: org };
  }

  /**
   * Show a real window for sign-in, and verify it end to end before closing —
   * accepting a cached org as proof would let us close the only window the user
   * can fix anything in, without ever testing the new session.
   */
  async signIn(onDone) {
    this.orgUuid = null;
    this.store.set('orgUuid', null);
    if (this.window && !this.window.isDestroyed()) this.window.destroy();

    const win = this.createWindow({ show: true });
    this.window = win;
    await win.loadURL(ORIGIN + '/login');
    win.focus();

    const deadline = Date.now() + 5 * 60 * 1000;
    const check = async () => {
      if (win.isDestroyed()) return onDone(false);
      if (Date.now() > deadline) {
        win.destroy();
        return onDone(false);
      }
      try {
        const get = this.windowGet(win);
        const org = await this.resolveOrg(get, { force: true });
        await this.fetchUsage(get, org);
        this.fallbackUntil = 0;
        // Cookies live in the persist:claude partition; the window can go.
        win.destroy();
        this.window = null;
        return onDone(true);
      } catch (err) {
        setTimeout(check, 2000);
      }
    };
    setTimeout(check, 2500);
  }

  destroy() {
    if (this.window && !this.window.isDestroyed()) this.window.destroy();
    this.window = null;
  }
}

module.exports = { Fetcher, AuthError, ORIGIN, PARTITION };
