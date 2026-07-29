// Pure helpers — no DOM access, no closures over module state.
//
// Loaded as a separate `<script>` before app.js in the generated
// index.html so the functions are available as globals by the time
// app.js runs. Also loaded standalone by `test.html` so these
// functions can be exercised without any framework or build step.
//
// Anything in this file MUST be:
//   - Deterministic (no Date.now()/Math.random() dependence unless passed in)
//   - Dependency-free (no references to IDX, view, dateFrom, cache, etc.)
//   - Side-effect-free
// If a function grows a runtime-state dependency, move it out of this file.

// Slugify a project name into a URL-safe token. Lowercase + swap any
// non-alphanum run for '-' + collapse dashes + trim. Stable enough to
// use as the canonical URL identifier.
function slugify(s) {
  return String(s || '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '');
}

// Insert underscore thousands separator — matches the TUI/CLI formatter.
function ft(n) {
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, '_');
}

// Split a string into searchable terms. MUST match `tokenize_into` in
// src/web.rs — divergence shows up as a query that silently matches
// nothing. Cases pinned in test.js. Kept: >=3 chars containing a letter;
// >=2 if any char is non-ASCII; all-digit runs of 2-6 chars.
function tokenize(text) {
  const out = [];
  for (const run of String(text || '')
    .toLowerCase()
    .split(/[^\p{L}\p{N}_]+/u)) {
    if (!run) continue;
    const chars = [...run];
    const n = chars.length;
    const hasLetter = /\p{L}/u.test(run);
    const nonAscii = chars.some((c) => c.codePointAt(0) > 127);
    const keep = hasLetter ? n >= 3 || (nonAscii && n >= 2) : n >= 2 && n <= 6 && /^\d+$/.test(run);
    if (keep) out.push(run);
  }
  return out;
}

// Index of the first element >= x in a sorted array.
function lowerBound(arr, x) {
  let lo = 0,
    hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid] < x) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

// POSIX single-quote a shell word — project paths contain spaces.
function shq(p) {
  return "'" + String(p == null ? '' : p).replace(/'/g, "'\\''") + "'";
}

// Shell command that resumes a session. `claude -r` resolves against the
// project derived from the shell's cwd, so another project's session
// needs a `cd` first; and a subagent transcript isn't resumable itself —
// `resume_id` carries its parent's id.
function resumeCmd(s) {
  const id = (s && (s.resume_id || s.id)) || '';
  const base = 'claude -r ' + id;
  const cwd = s && s.cwd;
  return cwd ? 'cd ' + shq(cwd) + ' && ' + base : base;
}

// Currency formatter — mirrors Rust `write_cost` (report/fmt.rs)
// exactly so CLI and web agree visibly: "<$0.01" only for tiny
// *positive* values, "$0.00" for zero, "-$X.YY" for negatives.
// Cents-first (round once) to dodge fp formatting drift; underscore
// grouping on the dollar portion.
function fc(n) {
  if (n > 0 && n < 0.01) return '<$0.01';
  const cents = Math.round(n * 100);
  const sign = cents < 0 ? '-$' : '$';
  const abs = Math.abs(cents);
  const dol = String(Math.floor(abs / 100)).replace(/\B(?=(\d{3})+(?!\d))/g, '_');
  return sign + dol + '.' + String(abs % 100).padStart(2, '0');
}

// "Mon D HH:MM" date with the day right-padded by &nbsp; so single-
// and double-digit days line up in a monospace column.
function fd(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  const mo = d.toLocaleString('en', { month: 'short' });
  const day = d.getDate();
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const dayPad = day < 10 ? '&nbsp;' + day : String(day);
  return mo + '&nbsp;' + dayPad + '&nbsp;' + hh + ':' + mm;
}

// Minimal HTML escape. Covers element-content contexts (innerHTML) AND
// double-/single-quoted attribute contexts: callers interpolate into
// attributes like data-model="' + esc(...) + '" and data-tip="' + esc(...),
// so a quote in a model/tool/project name must not break out. Escaping
// quotes is harmless in element-content contexts too.
function esc(s) {
  if (!s) return '';
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

// Human duration from ms: "42s", "5m 12s", "2h 10m", "3d 4h".
function fdur(ms) {
  if (!isFinite(ms) || ms <= 0) return '';
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + 's';
  const m = Math.floor(s / 60);
  if (m < 60) return m + 'm ' + (s % 60) + 's';
  const h = Math.floor(m / 60);
  if (h < 24) return h + 'h ' + (m % 60) + 'm';
  const d = Math.floor(h / 24);
  return d + 'd ' + (h % 24) + 'h';
}

function durMs(s) {
  if (!s || !s.started_at || !s.ended_at) return 0;
  const a = Date.parse(s.started_at),
    b = Date.parse(s.ended_at);
  return isFinite(a) && isFinite(b) && b > a ? b - a : 0;
}

// Highlight the (lowercased) needle `q` inside `t`, wrapping each hit
// in <span class="hl">. Called everywhere we fuzzy-match user input.
function hl(t, q) {
  if (!q) return esc(t);
  const lo = t.toLowerCase();
  let r = '',
    last = 0;
  while (true) {
    const i = lo.indexOf(q, last);
    if (i < 0) {
      r += esc(t.slice(last));
      break;
    }
    r += esc(t.slice(last, i)) + '<span class="hl">' + esc(t.slice(i, i + q.length)) + '</span>';
    last = i + q.length;
  }
  return r;
}

// Session display name — summary preferred; first user message as
// fallback; session id (a UUID) if neither was captured.
function dn(s) {
  return s.summary || s.first_user_msg || s.id;
}

// YYYY-MM-DD prefix of an ISO string, or null if too short to be valid.
function dayStr(iso) {
  return iso && iso.length >= 10 ? iso.slice(0, 10) : null;
}

// Fresh heatmap bucket. Populated in two passes: session/msg counts are
// added in the session loop (keyed on `started_at`), then tokens + cost
// are added from DAILY rows (keyed per-message UTC day).
function emptyDay() {
  return {
    tokens: 0,
    input: 0,
    output: 0,
    cache_read: 0,
    cache_create: 0,
    cost: 0,
    msgs: 0,
    sessions: 0,
  };
}

// Lift an IndexSession's per-type token counts into the canonical
// shape used by every list-view row (breakdown tooltip + row.tokens sum).
function tokenParts(s) {
  const input = s.total_input_tokens || 0;
  const output = s.total_output_tokens || 0;
  const cache_read = s.total_cache_read || 0;
  const cache_create = s.total_cache_create || 0;
  return {
    input,
    output,
    cache_read,
    cache_create,
    sum: input + output + cache_read + cache_create,
  };
}

function costParts(s) {
  return {
    input: s.cost_input || 0,
    output: s.cost_output || 0,
    cache_read: s.cost_cache_read || 0,
    cache_create: s.cost_cache_create || 0,
  };
}

// Tooltip strings. Pipe-separated so the mouseover handler can split
// them into the existing multi-line tooltip component.
function tokTip(b) {
  if (!b) return '';
  return (
    'Tokens|input: ' +
    ft(b.input || 0) +
    '|output: ' +
    ft(b.output || 0) +
    '|cache-read: ' +
    ft(b.cache_read || 0) +
    '|cache-write: ' +
    ft(b.cache_create || 0)
  );
}

// Cost cell tooltip. When a per-token-type breakdown is available
// (rows that carry `costBreakdown`), show the real dollar split.
// Otherwise fall back to per-session / per-message / per-1M-tok
// averages so aggregate rows still get useful context.
function costTip(cost, sessions, msgs, tokens, cb) {
  const lines = ['Cost'];
  if (cb) {
    lines.push('input: ' + fc(cb.input || 0));
    lines.push('output: ' + fc(cb.output || 0));
    lines.push('cache-read: ' + fc(cb.cache_read || 0));
    lines.push('cache-write: ' + fc(cb.cache_create || 0));
  } else {
    if (sessions > 0) lines.push('per session: ' + fc(cost / sessions));
    if (msgs > 0) lines.push('per message: ' + fc(cost / msgs));
    if (tokens > 0) lines.push('per 1M tok: ' + fc((cost * 1e6) / tokens));
  }
  return lines.join('|');
}
