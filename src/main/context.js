'use strict';

/**
 * Live context-window fill of the active Claude Code session.
 *
 * claude.ai / Claude Desktop chats expose no context size anywhere. Claude Code
 * does: every assistant turn in ~/.claude/projects/<proj>/<session>.jsonl has a
 * `usage` block, and input + cache_creation + cache_read = tokens in context.
 * We read the tail of the most recently written transcript only.
 */

const fs = require('fs');
const os = require('os');
const path = require('path');

const ACTIVE_FOR = 30 * 60;      // seconds since last write to count as live
const TAIL_BYTES = 512 * 1024;

function projectsDir() {
  const base = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude');
  return path.join(base, 'projects');
}

function newestTranscript(dir = projectsDir()) {
  let best = null;
  let projects;
  try { projects = fs.readdirSync(dir, { withFileTypes: true }); } catch { return null; }
  for (const p of projects) {
    if (!p.isDirectory()) continue;
    const pdir = path.join(dir, p.name);
    let files;
    try { files = fs.readdirSync(pdir); } catch { continue; }
    for (const f of files) {
      if (!f.endsWith('.jsonl')) continue;
      const file = path.join(pdir, f);
      try {
        const { mtimeMs } = fs.statSync(file);
        if (!best || mtimeMs > best.mtimeMs) best = { file, mtimeMs, project: p.name };
      } catch { /* raced with deletion */ }
    }
  }
  return best;
}

function readTail(file, bytes = TAIL_BYTES) {
  const fd = fs.openSync(file, 'r');
  try {
    const { size } = fs.fstatSync(fd);
    const start = Math.max(0, size - bytes);
    const buf = Buffer.alloc(size - start);
    fs.readSync(fd, buf, 0, buf.length, start);
    return buf.toString('utf8');
  } finally {
    fs.closeSync(fd);
  }
}

/** Last assistant usage in a JSONL tail -> { tokens, model } or null. */
function lastUsage(text) {
  const lines = text.split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i];
    if (!line.includes('"usage"')) continue;
    let row;
    try { row = JSON.parse(line); } catch { continue; } // first line may be cut
    if (row.isSidechain) continue;                       // subagent, not main ctx
    const msg = row.message;
    const u = msg && msg.usage;
    if (!u) continue;
    const tokens = (u.input_tokens || 0) + (u.cache_creation_input_tokens || 0)
      + (u.cache_read_input_tokens || 0);
    if (!tokens) continue;
    return { tokens, model: msg.model || '' };
  }
  return null;
}

/** limitSetting: 'auto' | number. Auto = 200k, or 1M once usage passes 200k. */
function contextLimit(tokens, model, limitSetting) {
  const n = Number(limitSetting);
  if (Number.isFinite(n) && n > 0) return n;
  if (/\[1m\]|-1m\b/i.test(model) || tokens > 200000) return 1000000;
  return 200000;
}

function readContext(limitSetting = 'auto', now = Date.now()) {
  const t = newestTranscript();
  if (!t) return { available: false, reason: 'No Claude Code transcripts found' };
  const idleSec = (now - t.mtimeMs) / 1000;
  let usage = null;
  try { usage = lastUsage(readTail(t.file)); } catch (e) { return { available: false, reason: e.message }; }
  if (!usage) return { available: false, reason: 'No usage in latest session yet' };
  const limit = contextLimit(usage.tokens, usage.model, limitSetting);
  return {
    available: true,
    active: idleSec <= ACTIVE_FOR,
    idleSec,
    tokens: usage.tokens,
    limit,
    pct: Math.min(100, (usage.tokens / limit) * 100),
    model: usage.model,
    project: t.project,
    file: t.file,
  };
}

module.exports = { readContext, lastUsage, contextLimit, newestTranscript, projectsDir };
