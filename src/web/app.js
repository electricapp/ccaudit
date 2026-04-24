let IDX = [],
  // Daily rollup shipped inside index.json. Derived from the same cache
  // preaggs the CLI usage report reads, so the heatmap and `ccaudit daily`
  // agree to the cent. Shape: {p:[proj_names], m:[model_names], d:[day_strs],
  // rows:[[day_idx, proj_idx, model_idx, in, out, cr, cw, cost], ...]}.
  DAILY = null,
  SIX = null,
  view = 'projects',
  prevView = null,
  cp = null,
  cs = null,
  q = '',
  sq = '',
  msgs = null,
  matchIdx = -1,
  matchEls = [],
  sel = -1;
// Mutated by adding/removing keys, never reassigned — const is the
// honest declaration even though the contents change.
const cache = {};
// Chunked render state: renderToken bumps on every rMessages() so stale
// idle callbacks from a prior render bail out on the token check.
let renderToken = 0,
  pendingGroups = null,
  pendingCtx = null;
let showKinds = new Set(['User', 'Assistant', 'ToolUse', 'ToolResult', 'Thinking', 'System']);
let detail = 'high',
  compact = false,
  dateFrom = '',
  dateTo = '',
  histMode = 'day',
  histLog = true,
  pieMode = 'model';
function setPieMode(m) {
  pieMode = m;
  render();
}
// Multi-select model filter: empty Set = "all models" (no filtering).
// Toggling via the dropdown adds/removes; clicking a model name elsewhere
// (row, pie slice) drills in to just that one. Reset clears the Set.
const modelFilters = new Set();
// Per-view sort state: every list view (projects, sessions, search,
// dashboard sub-tables) keeps its own (col, desc) so navigating away
// and back doesn't lose the user's chosen ordering. Default for time
// columns is descending ("most recent first"), descending for cost,
// ascending for identity (alphabetic).
// Defaults are kept frozen and reapplied (deep-copied) by resetAll.
const SORT_DEFAULTS = Object.freeze({
  projects: { col: 'date', desc: true },
  sessions: { col: 'date', desc: true },
  search: { col: 'date', desc: true },
  dashByModel: { col: 'cost', desc: true },
  dashBySession: { col: 'cost', desc: true },
  dashByProject: { col: 'cost', desc: true },
  // Session message viewer: default date-ascending (chronological,
  // matching the source JSONL). Click a header to re-sort by who/tokens.
  messages: { col: 'date', desc: false },
});
const DASH_LIMIT_DEFAULTS = Object.freeze({
  dashBySession: 12,
  dashByProject: 12,
  dashByModel: Infinity,
});
const sortState = freshSortState();
const dashLimit = freshDashLimit();
function freshSortState() {
  const o = {};
  for (const k of Object.keys(SORT_DEFAULTS)) o[k] = { ...SORT_DEFAULTS[k] };
  return o;
}
function freshDashLimit() {
  return { ...DASH_LIMIT_DEFAULTS };
}
// Dashboard scope: which slice of data the aggregations reflect. Set
// when the user enters the dashboard via 'd':
//   from landing → {kind:'all'}
//   from a project's session list → {kind:'project',pi}
//   from a session's messages view → {kind:'session',pi,si}
// Reset (`r`) always restores {kind:'all'} so the universal dashboard
// is one keystroke away.
let dashScope = { kind: 'all' };
// Tracks the view the previous render() painted so filter-only
// re-renders (same view) can preserve scroll instead of snapping to top.
let lastRenderedView = null;
function setHistMode(m) {
  histMode = m;
  render();
}
function toggleHistLog() {
  histLog = !histLog;
  render();
}
const $ = (s) => document.querySelector(s);
const M = $('#main'),
  S = $('#search'),
  B = $('#back'),
  DF = $('#dfrom'),
  DT = $('#dto'),
  BT = $('#btt'),
  MF = $('#mfilt'),
  CP = $('#crumb-p'),
  CS = $('#crumb-s');

// Load index + search index in parallel, then resolve any pre-loaded
// URL (e.g. user pasted a deep link) into navigation state, then render.
// Absolute paths on every fetch — the SPA router puts us at arbitrary
// URLs (e.g. /p/1/s/2) and relative paths would resolve against the
// nested path instead of the site root, yielding "not found" on deep
// links.
Promise.all([
  fetch('/index.json').then((r) => r.json()),
  fetch('/search.json').then((r) => r.json()),
])
  .then(([idx, six]) => {
    IDX = idx.projects;
    DAILY = idx.daily;
    SIX = six;
    buildRouteIndex();
    populateModelFilter();
    return applyPath();
  })
  .then(() => render())
  .catch((e) => {
    M.innerHTML = errorState('failed to load index', String(e));
  });

// ── URL routing ──
//
// URLs identify projects by slug and sessions by UUID, so a pasted link
// reads like the thing it points at (e.g. `/p/phonon-server/s/d50e7c15…`)
// rather than opaque numeric indices that can shuffle between builds.
// Routes:
//   /                                          → projects landing
//   /p/{project-slug}                          → project's session list
//   /p/{project-slug}/s/{session-uuid}         → session messages viewer
//   /dash                                      → universal dashboard
//   /dash/p/{project-slug}                     → project-scoped dashboard
//   /dash/p/{project-slug}/s/{session-uuid}    → session-scoped dashboard
//   /search?q=…                                → search results

// slugify / fd / fc / ft / esc / hl / dn / dayStr / durMs / fdur /
// tokenParts / costParts / tokTip / costTip all live in util.js and
// are globals by the time this file runs (util.js is concatenated
// ahead of app.js in the generated <script> block).
// Lookup tables from URL tokens to numeric IDX indices. Built once
// after the JSON loads; rebuilt only if we ever hot-reload the index.
let projectSlugByPi = [];
let piBySlug = new Map();
let sessionUuidByKey = new Map(); // `${pi}:${si}` → uuid
let siByProjectAndUuid = new Map(); // `${pi}:${uuid}` → si
function buildRouteIndex() {
  projectSlugByPi = new Array(IDX.length);
  piBySlug = new Map();
  sessionUuidByKey = new Map();
  siByProjectAndUuid = new Map();
  const slugCount = new Map();
  for (let pi = 0; pi < IDX.length; pi++) {
    let s = slugify(IDX[pi].name) || 'p' + pi;
    // Disambiguate collisions so every slug round-trips uniquely.
    const n = slugCount.get(s) || 0;
    if (n > 0) s = s + '-' + (n + 1);
    slugCount.set(s, n + 1);
    projectSlugByPi[pi] = s;
    piBySlug.set(s, pi);
    for (let si = 0; si < IDX[pi].sessions.length; si++) {
      const id = IDX[pi].sessions[si].id;
      sessionUuidByKey.set(pi + ':' + si, id);
      siByProjectAndUuid.set(pi + ':' + id, si);
    }
  }
}
function projectSlug(pi) {
  return projectSlugByPi[pi] || String(pi);
}
function sessionUuid(pi, si) {
  return sessionUuidByKey.get(pi + ':' + si) || String(si);
}
function resolveProject(slug) {
  return piBySlug.has(slug) ? piBySlug.get(slug) : -1;
}
function resolveSession(pi, uuid) {
  const key = pi + ':' + uuid;
  return siByProjectAndUuid.has(key) ? siByProjectAndUuid.get(key) : -1;
}

function buildPath() {
  if (view === 'sessions' && cp != null) return '/p/' + projectSlug(cp);
  if (view === 'messages' && cp != null && cs != null)
    return '/p/' + projectSlug(cp) + '/s/' + sessionUuid(cp, cs);
  if (view === 'dash') {
    if (dashScope.kind === 'session')
      return (
        '/dash/p/' + projectSlug(dashScope.pi) + '/s/' + sessionUuid(dashScope.pi, dashScope.si)
      );
    if (dashScope.kind === 'project') return '/dash/p/' + projectSlug(dashScope.pi);
    return '/dash';
  }
  if (view === 'search' && sq) return '/search?q=' + encodeURIComponent(sq);
  return '/';
}
function syncUrl() {
  const path = buildPath();
  const cur = location.pathname + location.search;
  if (cur !== path) history.pushState({}, '', path);
}
// Restore navigation state from the current URL. Async because the
// messages view needs the session JSON fetched before it can render.
// Slugs/UUIDs are matched greedily ([^/]+) so any URL-safe token works.
async function applyPath() {
  const path = location.pathname;
  let m;
  if ((m = path.match(/^\/dash\/p\/([^/]+)\/s\/([^/]+)\/?$/))) {
    const pi = resolveProject(m[1]);
    const si = pi >= 0 ? resolveSession(pi, m[2]) : -1;
    if (pi >= 0 && si >= 0) {
      view = 'dash';
      dashScope = { kind: 'session', pi, si };
      cp = pi;
      cs = si;
      return;
    }
  } else if ((m = path.match(/^\/dash\/p\/([^/]+)\/?$/))) {
    const pi = resolveProject(m[1]);
    if (pi >= 0) {
      view = 'dash';
      dashScope = { kind: 'project', pi };
      cp = pi;
      return;
    }
  } else if (path.match(/^\/dash\/?$/)) {
    view = 'dash';
    dashScope = { kind: 'all' };
    return;
  } else if ((m = path.match(/^\/p\/([^/]+)\/s\/([^/]+)\/?$/))) {
    const pi = resolveProject(m[1]);
    const si = pi >= 0 ? resolveSession(pi, m[2]) : -1;
    if (pi >= 0 && si >= 0) {
      const s = IDX[pi].sessions[si];
      cp = pi;
      cs = si;
      view = 'messages';
      if (!cache[s.file]) {
        try {
          const r = await fetch('/s/' + s.file);
          cache[s.file] = await r.json();
        } catch (_e) {
          view = 'projects';
          return;
        }
      }
      msgs = cache[s.file];
      return;
    }
  } else if ((m = path.match(/^\/p\/([^/]+)\/?$/))) {
    const pi = resolveProject(m[1]);
    if (pi >= 0) {
      cp = pi;
      view = 'sessions';
      return;
    }
  } else if (path.match(/^\/search\/?$/)) {
    const params = new URLSearchParams(location.search);
    sq = params.get('q') || '';
    q = sq;
    if (S) S.value = q;
    view = 'search';
    return;
  }
  // Unrecognized → fall through to landing.
  view = 'projects';
}
window.addEventListener('popstate', () => {
  applyPath().then(() => render());
});

// ── Unified table renderer ──
//
// One column schema for every list/aggregate view. Each row passes a
// uniform record shape (date, identity, sessions, messages, tokens,
// cost, duration + breakdown fields for tooltips). Headers click to
// sort. Header label embeds the count or sum of that column across
// visible rows so the totals don't need a separate stat bar.
const UCOLS = [
  { key: 'date', label: 'Date', cls: 'col-date', align: 'l', def: 'desc' },
  { key: 'identity', label: '%LBL%', cls: 'col-ident', align: 'l', def: 'asc' },
  { key: 'sessions', label: 'Sessions', cls: 'col-sess', align: 'r', def: 'desc', sum: 'count' },
  { key: 'messages', label: 'Messages', cls: 'col-msgs', align: 'r', def: 'desc', sum: 'count' },
  { key: 'tokens', label: 'Tokens', cls: 'col-tok', align: 'r', def: 'desc', sum: 'count' },
  { key: 'cost', label: 'Cost', cls: 'col-cost', align: 'r', def: 'desc', sum: 'cost' },
  { key: 'duration', label: 'Duration', cls: 'col-dur', align: 'r', def: 'desc', sum: 'dur' },
];
// tokenParts / costParts live in util.js.
// Segmented button row used by the pie (model/tool) and histogram
// (day/project/hour) toggles. Each entry is [value, label]; the
// button matching `current` gets the `on` modifier. `handler` is the
// bare name of a globally-exposed setter (e.g. 'setPieMode').
function buildModeToggle(modes, current, handler) {
  return modes
    .map(
      ([v, lbl]) =>
        '<button class="pbtn' +
        (current === v ? ' on' : '') +
        '" onclick="' +
        handler +
        "('" +
        v +
        '\')">' +
        lbl +
        '</button>'
    )
    .join('');
}
// tokTip / costTip live in util.js.

// Sort `rows` in place per `view`'s sort state. Identity sorts as a
// case-insensitive string; everything else as numeric.
function sortRows(rows, view) {
  const ss = sortState[view];
  if (!ss) return rows;
  const k = ss.col,
    d = ss.desc ? -1 : 1;
  rows.sort((a, b) => {
    let av = a[k],
      bv = b[k];
    if (k === 'identity' || k === 'date') {
      av = String(av || '').toLowerCase();
      bv = String(bv || '').toLowerCase();
      if (av < bv) return -1 * d;
      if (av > bv) return 1 * d;
      return 0;
    }
    return ((+av || 0) - (+bv || 0)) * d;
  });
  return rows;
}
// Render the unified table for a given view.
//   view       — sortState key (e.g. 'projects', 'dashByModel')
//   identLbl   — human-readable identity-column label (e.g. 'Projects', 'Models')
//   rows       — array of row records (see UCOLS keys)
//   limit      — optional cap; rows beyond emit a "more" expander row
function buildTable(view, identLbl, rows, limit) {
  sortRows(rows, view);
  const ss = sortState[view];
  const total = rows.length;
  const cap = limit && limit < total ? limit : total;
  const visible = rows.slice(0, cap);
  // Detect "degenerate" columns — every row passed null for this field.
  // Treated as label-only: no count in header, no sortable affordance,
  // dim em-dash cells. Lets a view (e.g. dashboard's by-session table)
  // declare a column meaningless without changing the column set.
  const dead = new Set();
  for (const c of UCOLS) {
    if (c.key === 'date' || c.key === 'identity') continue;
    if (visible.every((r) => r[c.key] == null)) dead.add(c.key);
  }
  // Header — plain labels (no embedded sums; we'll surface totals via
  // a different mechanism later). Active sort column gets the .active
  // class so the user can see which column is sorting; click-to-reverse
  // is implicit (no arrow chrome needed).
  let h = '<table class="utable"><thead><tr>';
  for (const c of UCOLS) {
    let label = c.label;
    if (label === '%LBL%') label = identLbl;
    const sortable = !dead.has(c.key);
    const active = sortable && ss && ss.col === c.key ? ' active' : '';
    const cls = c.cls + (c.align === 'r' ? ' r' : '') + (sortable ? ' sortable' : '') + active;
    const attrs = sortable ? ' data-sort="' + c.key + '" data-view="' + view + '"' : '';
    h += '<th class="' + cls + '"' + attrs + '>' + esc(label) + '</th>';
  }
  h += '</tr></thead><tbody>';
  for (const r of visible) {
    const attrs =
      (r.click ? ' onclick="' + r.click + '"' : '') +
      (r.dataModel ? ' data-model="' + esc(r.dataModel) + '"' : '');
    h += '<tr class="clickable"' + attrs + '>';
    for (const c of UCOLS) {
      const align = c.align === 'r' ? ' r' : '';
      const dimCls =
        c.key === 'date' || c.key === 'sessions' || c.key === 'messages' || c.key === 'duration'
          ? ' dim'
          : '';
      if (dead.has(c.key)) {
        h += '<td class="' + c.cls + align + ' dim">—</td>';
        continue;
      }
      if (c.key === 'date') {
        h += '<td class="' + c.cls + align + dimCls + '">' + (r.date ? fd(r.date) : '') + '</td>';
        continue;
      }
      if (c.key === 'identity') {
        h += '<td class="' + c.cls + '">' + (r.identityHtml || esc(r.identity || '')) + '</td>';
        continue;
      }
      if (c.key === 'sessions') {
        h += '<td class="' + c.cls + align + dimCls + '">' + ft(r.sessions || 0) + '</td>';
        continue;
      }
      if (c.key === 'messages') {
        h += '<td class="' + c.cls + align + dimCls + '">' + ft(r.messages || 0) + '</td>';
        continue;
      }
      if (c.key === 'tokens') {
        const tip = r.breakdown ? ' data-tip="' + esc(tokTip(r.breakdown)) + '"' : '';
        const cls = c.cls + align + dimCls + (tip ? ' tipcell' : '');
        h += '<td class="' + cls + '"' + tip + '>' + ft(r.tokens || 0) + '</td>';
        continue;
      }
      if (c.key === 'cost') {
        const tip =
          ' data-tip="' +
          esc(
            costTip(r.cost || 0, r.sessions || 0, r.messages || 0, r.tokens || 0, r.costBreakdown)
          ) +
          '"';
        h +=
          '<td class="' +
          c.cls +
          align +
          ' tipcell"' +
          tip +
          '><span class="cost-val">' +
          fc(r.cost || 0) +
          '</span></td>';
        continue;
      }
      if (c.key === 'duration') {
        h += '<td class="' + c.cls + align + dimCls + '">' + fdur(r.duration || 0) + '</td>';
        continue;
      }
    }
    h += '</tr>';
  }
  // Expander: a "▼ N more" chip extends the window by 12, "▾ All"
  // jumps straight to fully expanded. "△ Collapse" appears once the
  // cap has been bumped above the default, even if every row already
  // fits — so the user has a round-trip back to the compact state.
  const expanded =
    limit && DASH_LIMIT_DEFAULTS[view] != null && dashLimit[view] > DASH_LIMIT_DEFAULTS[view];
  if (limit && (total > cap || expanded)) {
    h += '<tr class="more-row"><td colspan="' + UCOLS.length + '">';
    if (total > cap) {
      const hidden = total - cap;
      h += '<span class="more-chip" data-expand="' + view + '">▼ ' + ft(hidden) + ' more</span>';
      h += '<span class="more-chip more-all" data-expand-all="' + view + '">▾ All</span>';
    }
    if (expanded) {
      h += '<span class="more-chip" data-collapse="' + view + '">△ Collapse</span>';
    }
    h += '</td></tr>';
  }
  h += '</tbody></table>';
  return h;
}
function populateModelFilter() {
  const seen = new Set();
  IDX.forEach((p) =>
    p.sessions.forEach((s) => {
      if (s.model) seen.add(s.model);
    })
  );
  const opts = [{ v: '', l: 'all models' }].concat([...seen].sort().map((m) => ({ v: m, l: m })));
  renderModelDrop(MF, opts);
}
// Multi-select dropdown for the model filter. "All models" highlights
// when the Set is empty; individual rows highlight + show a checkmark
// (CSS) when present in the Set.
function renderModelDrop(root, opts) {
  let h =
    '<button type="button" class="drop-btn">' +
    esc(modelLabel()) +
    '<span class="drop-arr">▾</span></button>';
  h += '<div class="drop-menu hidden">';
  for (const o of opts) {
    const isAll = o.v === '';
    const on = isAll ? modelFilters.size === 0 : modelFilters.has(o.v);
    h +=
      '<div class="drop-item' +
      (on ? ' on' : '') +
      '" data-v="' +
      esc(o.v) +
      '">' +
      esc(o.l) +
      '</div>';
  }
  h += '</div>';
  root.innerHTML = h;
}
function modelLabel() {
  if (modelFilters.size === 0) return 'all models';
  if (modelFilters.size === 1) return [...modelFilters][0];
  return modelFilters.size + ' models';
}
// Dropdown click: toggle (or, on the "all models" row, clear). Keep the
// menu open so users can pick several without reopening it each time.
// Read back the dropdown's current option list from the DOM. Used
// by the mutation helpers below to rebuild the dropdown without
// reassembling the full option list from IDX each time.
function modelDropOpts() {
  if (!MF) return [];
  return [...MF.querySelectorAll('.drop-item')].map((i) => ({
    v: i.dataset.v,
    l: i.textContent,
  }));
}
function toggleModelFilter(m) {
  if (!m) {
    modelFilters.clear();
  } else if (modelFilters.has(m)) {
    modelFilters.delete(m);
  } else {
    modelFilters.add(m);
  }
  if (MF) {
    renderModelDrop(MF, modelDropOpts());
    const menu = MF.querySelector('.drop-menu');
    if (menu) menu.classList.remove('hidden');
  }
  render();
}
// Drill-in click (row or pie slice): replace the filter with just this
// model. Closes the dropdown menu since this is a navigational action.
function setModelFilterOnly(m) {
  modelFilters.clear();
  if (m) modelFilters.add(m);
  if (MF) renderModelDrop(MF, modelDropOpts());
  render();
}
function setSingleDay(day) {
  dateFrom = day;
  dateTo = day;
  DF.value = day;
  DT.value = day;
  applyDateBounds();
  syncPresets();
  render();
}
// Clear every active filter: date range, model, search, kind toggles,
// detail level, compact. Snaps the user back to the unfiltered view.
function resetAll() {
  dateFrom = '';
  dateTo = '';
  DF.value = '';
  DT.value = '';
  applyDateBounds();
  syncPresets();
  modelFilters.clear();
  if (MF) {
    const opts = modelDropOpts();
    if (opts.length) renderModelDrop(MF, opts);
  }
  q = '';
  sq = '';
  S.value = '';
  showKinds = new Set(['User', 'Assistant', 'ToolUse', 'ToolResult', 'Thinking', 'System']);
  detail = 'high';
  compact = false;
  // Reset always returns the dashboard to its universal scope, even
  // if invoked from another view — so 'r' is the unconditional escape
  // hatch back to "everything" no matter where you are.
  dashScope = { kind: 'all' };
  // Reset table sort selections and dashboard expand caps back to the
  // factory defaults so 'r' is a true "back to baseline" key.
  Object.assign(sortState, freshSortState());
  Object.assign(dashLimit, freshDashLimit());
  render();
}
function inModel(s) {
  return !modelFilters.size || modelFilters.has(s.model);
}

// Build flat session map once IDX is loaded (maps flat index → {pi,si,session,project})
let flatMap = [];
function buildFlatMap() {
  flatMap = [];
  for (let pi = 0; pi < IDX.length; pi++)
    for (let si = 0; si < IDX[pi].sessions.length; si++)
      flatMap.push({ pi, si, s: IDX[pi].sessions[si], p: IDX[pi] });
}

S.addEventListener('input', (e) => {
  q = e.target.value.toLowerCase();
  // Global search at 2+ chars when in projects/sessions/search view
  if (q.length >= 2 && view !== 'messages') {
    const res = doSearch(q);
    if (res.length || view === 'search') {
      sq = q;
      view = 'search';
    }
  } else if (view === 'search' && q.length < 2) {
    view = prevView || 'projects';
    sq = '';
  }
  render();
});

DF.addEventListener('change', (e) => {
  dateFrom = e.target.value;
  // Invariant: from ≤ to. If user picks a from later than the current
  // to, snap to = from so the range stays valid.
  if (dateFrom && dateTo && dateFrom > dateTo) {
    dateTo = dateFrom;
    DT.value = dateTo;
  }
  applyDateBounds();
  syncPresets();
  render();
});
DT.addEventListener('change', (e) => {
  dateTo = e.target.value;
  if (dateFrom && dateTo && dateTo < dateFrom) {
    dateFrom = dateTo;
    DF.value = dateFrom;
  }
  applyDateBounds();
  syncPresets();
  render();
});
M.addEventListener('scroll', () => {
  BT.classList.toggle('on', M.scrollTop > 400);
});

// Keep the native pickers in sync with the invariant so users can't
// even select an out-of-range date.
function applyDateBounds() {
  DF.max = dateTo || '';
  DT.min = dateFrom || '';
}

// Preset ranges: `days == null` clears the filter (shows everything).
// Highlights the matching preset pill; manual edits clear the highlight.
function setDateRange(days) {
  if (days == null) {
    dateFrom = '';
    dateTo = '';
  } else {
    const now = new Date();
    dateTo = now.toISOString().slice(0, 10);
    dateFrom = new Date(now.getTime() - days * 86400000).toISOString().slice(0, 10);
  }
  DF.value = dateFrom;
  DT.value = dateTo;
  applyDateBounds();
  syncPresets(days);
  render();
}
function syncPresets(activeDays) {
  document.querySelectorAll('.pbtn').forEach((b) => {
    const d = b.dataset.days;
    const on =
      (activeDays === null && d === '0') || (activeDays != null && String(activeDays) === d);
    b.classList.toggle('on', !!on);
  });
}

document.addEventListener('keydown', (e) => {
  if (e.key === '/' && document.activeElement !== S) {
    e.preventDefault();
    S.focus();
    return;
  }
  if (e.key === 'Escape') {
    if (document.activeElement === S) S.blur();
    goBack();
    e.preventDefault();
    return;
  }
  // n/N for match navigation in message view
  if (view === 'messages' && sq && document.activeElement !== S) {
    if (e.key === 'n') {
      jumpMatch(1);
      e.preventDefault();
      return;
    }
    if (e.key === 'N') {
      jumpMatch(-1);
      e.preventDefault();
      return;
    }
  }
  if (document.activeElement === S) return;
  if (e.key === 'q') {
    goBack();
    e.preventDefault();
    return;
  }
  // Arrow navigation for list views
  const listView = view === 'projects' || view === 'sessions' || view === 'search';
  if (listView) {
    if (e.key === 'ArrowDown' || e.key === 'j') {
      sel++;
      updateSel();
      e.preventDefault();
      return;
    }
    if (e.key === 'ArrowUp' || e.key === 'k') {
      sel--;
      updateSel();
      e.preventDefault();
      return;
    }
    if (e.key === 'ArrowRight' || e.key === 'Enter') {
      activateSel();
      e.preventDefault();
      return;
    }
  }
  // left arrow = back
  if (e.key === 'ArrowLeft') {
    goBack();
    e.preventDefault();
    return;
  }
  // d toggles the dashboard. Entry view determines scope:
  //   landing → universal | sessions → that project | messages → that session
  // Pressing d again returns to the view we came from (so the toggle is
  // truly round-trip), preserving cp/cs.
  if (e.key === 'd') {
    if (view === 'dash') {
      if (dashScope.kind === 'session') view = 'messages';
      else if (dashScope.kind === 'project') view = 'sessions';
      else {
        view = 'projects';
        q = '';
        S.value = '';
      }
    } else {
      if (view === 'sessions') dashScope = { kind: 'project', pi: cp };
      else if (view === 'messages') dashScope = { kind: 'session', pi: cp, si: cs };
      else dashScope = { kind: 'all' };
      view = 'dash';
      sel = -1;
    }
    render();
    e.preventDefault();
    return;
  }
  // Dashboard keys: h/p/y cycle histogram mode, l toggles log/linear,
  // m/t switch the pie between by-model and by-tool.
  if (view === 'dash') {
    if (e.key === 'h') {
      setHistMode('hour');
      e.preventDefault();
      return;
    }
    if (e.key === 'p') {
      setHistMode('project');
      e.preventDefault();
      return;
    }
    if (e.key === 'y') {
      setHistMode('day');
      e.preventDefault();
      return;
    }
    if (e.key === 'l') {
      toggleHistLog();
      e.preventDefault();
      return;
    }
    if (e.key === 'm') {
      setPieMode('model');
      e.preventDefault();
      return;
    }
    if (e.key === 't') {
      setPieMode('tool');
      e.preventDefault();
      return;
    }
  }
  // r resets all filters (from any view).
  if (e.key === 'r') {
    resetAll();
    e.preventDefault();
    return;
  }
});

// After the list-view refactor to `<table class="utable">`, data rows
// are `<tr class="clickable">` (the `.more-row` expander is
// deliberately excluded). The j/k/arrow navigation + enter-to-open
// lookups all route through this.
function getRows() {
  return M.querySelectorAll('.utable tr.clickable');
}

function updateSel() {
  const rows = getRows();
  if (!rows.length) {
    sel = -1;
    return;
  }
  if (sel < 0) sel = 0;
  if (sel >= rows.length) sel = rows.length - 1;
  rows.forEach((r, i) => {
    r.classList.toggle('sel', i === sel);
  });
  rows[sel].scrollIntoView({ block: 'nearest' });
}

function activateSel() {
  const rows = getRows();
  if (sel >= 0 && sel < rows.length) rows[sel].click();
}

function goBack() {
  sel = -1;
  if (view === 'messages') {
    // Honor the entry point. prevView === 'dash' means the user came
    // from the dashboard's top-sessions click; go back there.
    if (prevView === 'dash') {
      view = 'dash';
      cs = null;
      msgs = null;
    } else if (sq) {
      view = 'search';
      q = sq;
      S.value = sq;
    } else {
      view = 'sessions';
      cs = null;
      msgs = null;
    }
  } else if (view === 'dash') {
    view = 'projects';
    q = '';
    S.value = '';
  } else if (view === 'search') {
    view = 'projects';
    q = '';
    sq = '';
    S.value = '';
  } else if (view === 'sessions') {
    view = 'projects';
    cp = null;
    q = '';
    S.value = '';
  }
  render();
}

// --- Utilities ---
// ft / fc / fd / esc / fdur / durMs / hl / dn / dayStr all live in util.js.
// Structured empty / error / loading markup. The three variants share
// the same shape (icon + title + optional body) so whatever the reason
// the main area has no rows, the user lands on a consistent layout.
function emptyState(title, body) {
  return (
    '<div class="empty" role="status"><div class="empty-icon">∅</div>' +
    '<div class="empty-title">' +
    esc(title) +
    '</div>' +
    (body ? '<div class="empty-body">' + esc(body) + '</div>' : '') +
    '</div>'
  );
}
function errorState(title, body) {
  return (
    '<div class="empty error" role="alert"><div class="empty-icon">!</div>' +
    '<div class="empty-title">' +
    esc(title) +
    '</div>' +
    (body ? '<div class="empty-body">' + esc(body) + '</div>' : '') +
    '</div>'
  );
}
function loadingState(label) {
  return (
    '<div class="loading" role="status" aria-live="polite">' + esc(label || 'loading…') + '</div>'
  );
}
// mq + inDate close over module state so they stay here.
function mq(t) {
  return !q || t.toLowerCase().includes(q);
}
function inDate(iso) {
  if (!iso || iso.length < 10) return true;
  const d = iso.slice(0, 10);
  if (dateFrom && d < dateFrom) return false;
  if (dateTo && d > dateTo) return false;
  return true;
}

// Fenced-code syntax highlight. Splits on ```lang ... ``` blocks, tokenizes
// inside each, escapes outside. Intentionally tiny — covers js/ts/py/rs/go/sh
// keywords + strings + numbers + line comments. Unknown langs still get the
// code-block styling but no keyword coloring.
const KW = {
  js: 'async await break case catch class const continue default delete do else export extends finally for from function if import in instanceof let new null of return super switch this throw try typeof undefined var void while yield true false',
  ts: 'async await break case catch class const continue default delete do else enum export extends finally for from function if implements import in instanceof interface let new null of return super switch this throw try type typeof undefined var void while yield true false',
  py: 'and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield',
  python:
    'and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield',
  rs: 'as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while',
  rust: 'as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while',
  go: 'break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var true false nil',
  sh: 'if then else elif fi case esac for while do done in function return exit',
  bash: 'if then else elif fi case esac for while do done in function return exit',
};
function tok(code, lang) {
  const kws = new Set((KW[lang] || '').split(/\s+/).filter(Boolean));
  const cmt = lang === 'py' || lang === 'python' || lang === 'sh' || lang === 'bash' ? '#' : '//';
  const n = code.length;
  let out = '',
    i = 0;
  while (i < n) {
    const c = code[i];
    if (code.substr(i, cmt.length) === cmt) {
      let e = code.indexOf('\n', i);
      if (e < 0) e = n;
      out += '<span class="cm">' + esc(code.slice(i, e)) + '</span>';
      i = e;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      let j = i + 1;
      while (j < n && code[j] !== c) {
        if (code[j] === '\\' && j + 1 < n) j++;
        j++;
      }
      out += '<span class="st">' + esc(code.slice(i, Math.min(j + 1, n))) + '</span>';
      i = Math.min(j + 1, n);
      continue;
    }
    if (c >= '0' && c <= '9') {
      let j = i;
      while (j < n && /[\d._xXa-fA-F]/.test(code[j])) j++;
      out += '<span class="nm">' + esc(code.slice(i, j)) + '</span>';
      i = j;
      continue;
    }
    if (/[a-zA-Z_$]/.test(c)) {
      let j = i;
      while (j < n && /[a-zA-Z0-9_$]/.test(code[j])) j++;
      const w = code.slice(i, j);
      out += kws.has(w) ? '<span class="kw">' + esc(w) + '</span>' : esc(w);
      i = j;
      continue;
    }
    out += esc(c);
    i++;
  }
  return out;
}
// Hoisted regex — otherwise we'd recompile it on every message render.
const RE_FENCE = /```([a-zA-Z0-9]*)\n?([\s\S]*?)```/g;
function hlCode(text) {
  if (!text) return '';
  RE_FENCE.lastIndex = 0;
  let out = '',
    last = 0,
    m;
  while ((m = RE_FENCE.exec(text)) !== null) {
    out += esc(text.slice(last, m.index));
    out += '<pre class="code"><code>' + tok(m[2], m[1].toLowerCase()) + '</code></pre>';
    last = m.index + m[0].length;
  }
  out += esc(text.slice(last));
  return out;
}

// --- Search engine ---
function tokenize(text) {
  const r = [];
  const m = text.toLowerCase().match(/[a-z][a-z0-9_]{2,}/g);
  return m || r;
}

function doSearch(query) {
  if (!SIX || !query) return [];
  if (!flatMap.length) buildFlatMap();
  const terms = tokenize(query);
  if (!terms.length) return [];
  let hits = null;
  for (let ti = 0; ti < terms.length; ti++) {
    const term = terms[ti];
    const isLast = ti === terms.length - 1;
    let posting = SIX.w[term];
    // Prefix match for last (partial) term
    if (!posting && isLast) {
      const merged = new Set();
      for (const w in SIX.w) {
        if (w.startsWith(term)) for (const id of SIX.w[w]) merged.add(id);
      }
      if (merged.size) posting = [...merged];
    }
    if (!posting || !posting.length) return [];
    if (!hits) {
      hits = new Set(posting);
    } else {
      // Intersect in place — iterate the smaller set, delete what's
      // not in the larger. Avoids the old spread-filter-spread dance.
      const other = new Set(posting);
      const [small, big] = hits.size <= other.size ? [hits, other] : [other, hits];
      const next = new Set();
      for (const x of small) if (big.has(x)) next.add(x);
      hits = next;
    }
    if (!hits.size) return [];
  }
  // Map to results, sorted by recency
  const res = [];
  for (const idx of hits) {
    if (idx < flatMap.length) res.push(flatMap[idx]);
  }
  res.sort((a, b) => (b.s.started_at || '').localeCompare(a.s.started_at || ''));
  return res;
}

// Is any non-default state active? Controls the reset button's
// "actionable" look (yellow vs dim). Includes filters, but also sort
// selections, dashboard expansion, and scope — since `r` resets all of
// them, the button's color should reflect whether clicking will DO
// something.
function anyFilterActive() {
  const KINDS = ['User', 'Assistant', 'ToolUse', 'ToolResult', 'Thinking', 'System'];
  const allKinds = showKinds.size === KINDS.length && KINDS.every((k) => showKinds.has(k));
  // Sort drift: any view's (col, desc) differs from its default.
  const sortDrift = Object.keys(SORT_DEFAULTS).some((k) => {
    const cur = sortState[k],
      def = SORT_DEFAULTS[k];
    return cur.col !== def.col || cur.desc !== def.desc;
  });
  // Dashboard expansion above default cap.
  const expandDrift = Object.keys(DASH_LIMIT_DEFAULTS).some(
    (k) => dashLimit[k] !== DASH_LIMIT_DEFAULTS[k]
  );
  return !!(
    dateFrom ||
    dateTo ||
    modelFilters.size ||
    q ||
    sq ||
    !allKinds ||
    detail !== 'high' ||
    compact ||
    dashScope.kind !== 'all' ||
    sortDrift ||
    expandDrift
  );
}
function syncResetState() {
  document.querySelectorAll('.pbtn.reset').forEach((b) => {
    b.classList.toggle('dim', !anyFilterActive());
  });
}

// Update the project/session breadcrumb in the header to reflect the
// current navigation. Both segments are click-targets when populated:
//   project → jumps to that project's session list
//   session → jumps to that session's message viewer
// "—" + .dim renders as a non-interactive placeholder when not relevant.
function updateCrumbs() {
  let pName = null,
    sName = null;
  // Inherit names from view OR from dashboard scope, since the
  // dashboard can be project- or session-scoped while view==='dash'.
  if (view === 'sessions' && IDX[cp]) {
    pName = IDX[cp].name;
  } else if (view === 'messages' && IDX[cp] && IDX[cp].sessions[cs]) {
    pName = IDX[cp].name;
    // Show the session UUID rather than the truncated first-message
    // text — it's stable, copy-pasteable, and what `claude -r` takes.
    sName = IDX[cp].sessions[cs].id;
  } else if (view === 'dash') {
    if (dashScope.kind === 'project' && IDX[dashScope.pi]) {
      pName = IDX[dashScope.pi].name;
    } else if (
      dashScope.kind === 'session' &&
      IDX[dashScope.pi] &&
      IDX[dashScope.pi].sessions[dashScope.si]
    ) {
      pName = IDX[dashScope.pi].name;
      sName = IDX[dashScope.pi].sessions[dashScope.si].id;
    }
  }
  if (CP) {
    CP.textContent = pName || '—';
    CP.classList.toggle('dim', !pName);
  }
  if (CS) {
    CS.textContent = sName || '—';
    CS.classList.toggle('dim', !sName);
  }
  // Browser tab / shared-link title. Reflects the current view and
  // scope so a pasted URL carries meaningful context when it lands.
  let t = 'ccaudit';
  if (view === 'dash') {
    if (pName && sName) t = 'dashboard · ' + pName + ' · ' + sName + ' — ccaudit';
    else if (pName) t = 'dashboard · ' + pName + ' — ccaudit';
    else t = 'dashboard — ccaudit';
  } else if (view === 'sessions' && pName) {
    t = pName + ' — ccaudit';
  } else if (view === 'messages' && pName && sName) {
    t = sName + ' · ' + pName + ' — ccaudit';
  } else if (view === 'search' && sq) {
    t = 'search: ' + sq + ' — ccaudit';
  }
  if (document.title !== t) document.title = t;
}
// Click handlers for the breadcrumb anchors. Wired by inline onclick
// in index.html; no-ops when the segment is not currently populated.
function crumbClickP() {
  // Resolve the target project from either current view or dash scope.
  let pi = null;
  if (view === 'sessions' || view === 'messages') pi = cp;
  else if (view === 'dash' && (dashScope.kind === 'project' || dashScope.kind === 'session'))
    pi = dashScope.pi;
  if (pi == null || !IDX[pi]) return;
  cp = pi;
  cs = null;
  view = 'sessions';
  q = '';
  S.value = '';
  sel = -1;
  render();
}
function crumbClickS() {
  let pi = null,
    si = null;
  if (view === 'messages') {
    pi = cp;
    si = cs;
  } else if (view === 'dash' && dashScope.kind === 'session') {
    pi = dashScope.pi;
    si = dashScope.si;
  }
  if (pi == null || si == null || !IDX[pi] || !IDX[pi].sessions[si]) return;
  oSR(pi, si);
}

// --- Render dispatcher ---
function render() {
  // Bump renderToken at the dispatcher so in-flight chunks from a
  // prior messages render bail out, regardless of which view we're
  // switching to. rMessages reads the current token (no second bump).
  renderToken++;
  // Preserve scroll on intra-view re-renders (sort click, filter
  // change, "more" expansion). Only reset to 0 when the user actually
  // navigated to a different view. Capture before any view-fn writes
  // innerHTML; restore after.
  const sameView = view === lastRenderedView;
  const savedScroll = sameView ? M.scrollTop : 0;
  B.className = view === 'projects' || view === 'dash' ? 'hidden' : '';
  if (view === 'projects') rProjects();
  else if (view === 'sessions') rSessions();
  else if (view === 'search') rSearch();
  else if (view === 'dash') rDash();
  else rMessages();
  // Restore scroll on intra-view re-renders. View functions used to
  // unconditionally `M.scrollTop=0` — they no longer do; this is the
  // single source of truth.
  M.scrollTop = savedScroll;
  lastRenderedView = view;
  updateCrumbs();
  syncResetState();
  syncUrl();
}

// --- Projects view ---
// Recompute a project's totals over just the sessions that pass the
// current date filter. Returns null when no sessions qualify — caller
// hides the project in that case.
function projectStats(p) {
  const filtered = dateFrom || dateTo || modelFilters.size;
  const ss = filtered ? p.sessions.filter((s) => inDate(s.started_at) && inModel(s)) : p.sessions;
  if (!ss.length) return null;
  let msg_count = 0,
    cost = 0,
    ti = 0,
    to = 0,
    tcr = 0,
    tcc = 0,
    ci = 0,
    co = 0,
    ccr = 0,
    ccc = 0,
    last = '',
    dur = 0;
  for (const s of ss) {
    msg_count += s.msg_count;
    cost += s.cost;
    ti += s.total_input_tokens;
    to += s.total_output_tokens;
    tcr += s.total_cache_read;
    tcc += s.total_cache_create;
    ci += s.cost_input || 0;
    co += s.cost_output || 0;
    ccr += s.cost_cache_read || 0;
    ccc += s.cost_cache_create || 0;
    if (s.started_at && s.started_at > last) last = s.started_at;
    dur += durMs(s);
  }
  return {
    sessions: ss.length,
    msg_count,
    cost,
    total_input: ti,
    total_output: to,
    total_cache_read: tcr,
    total_cache_create: tcc,
    cost_input: ci,
    cost_output: co,
    cost_cache_read: ccr,
    cost_cache_create: ccc,
    last_active: last || null,
    duration_ms: dur,
  };
}

function rProjects() {
  const rows = [];
  for (const p of IDX) {
    if (!mq(p.name)) continue;
    const st = projectStats(p);
    if (!st) continue;
    const i = IDX.indexOf(p);
    rows.push({
      date: st.last_active,
      identity: p.name,
      identityHtml: hl(p.name, q),
      sessions: st.sessions,
      messages: st.msg_count,
      tokens: st.total_input + st.total_output + st.total_cache_read + st.total_cache_create,
      cost: st.cost,
      duration: st.duration_ms,
      breakdown: {
        input: st.total_input,
        output: st.total_output,
        cache_read: st.total_cache_read,
        cache_create: st.total_cache_create,
      },
      costBreakdown: {
        input: st.cost_input,
        output: st.cost_output,
        cache_read: st.cost_cache_read,
        cache_create: st.cost_cache_create,
      },
      click: 'oP(' + i + ')',
    });
  }
  if (!rows.length) {
    M.innerHTML = emptyState(
      'no projects',
      dateFrom || dateTo
        ? 'Nothing in the selected date range. Press r to reset filters.'
        : 'No Claude Code sessions found in ~/.claude/projects yet.'
    );
    return;
  }
  M.innerHTML = buildTable('projects', 'Projects', rows);
}

function oP(i) {
  cp = i;
  view = 'sessions';
  q = '';
  S.value = '';
  sel = -1;
  render();
}

// --- Sessions view ---
function rSessions() {
  const p = IDX[cp];
  const f = p.sessions.filter((s) => mq(dn(s)) && inDate(s.started_at) && inModel(s));
  if (!f.length) {
    M.innerHTML = emptyState(
      'no sessions',
      dateFrom || dateTo || modelFilters.size
        ? 'Nothing matches the current filters. Press r to reset.'
        : 'This project has no sessions yet.'
    );
    return;
  }
  const rows = f.map((s) => {
    const i = p.sessions.indexOf(s);
    const nm = dn(s);
    const d = nm.length > 90 ? nm.slice(0, 87) + '...' : nm;
    const tp = tokenParts(s);
    return {
      date: s.started_at,
      identity: nm,
      identityHtml: hl(d, q),
      sessions: null,
      messages: s.msg_count,
      tokens: tp.sum,
      cost: s.cost || 0,
      duration: durMs(s),
      breakdown: tp,
      costBreakdown: costParts(s),
      click: 'oS(' + i + ')',
    };
  });
  M.innerHTML = buildTable('sessions', 'Sessions', rows);
}

// --- Search results view ---
function rSearch() {
  const all = doSearch(sq);
  const res = all.filter((r) => inDate(r.s.started_at) && inModel(r.s));
  if (!res.length) {
    M.innerHTML = emptyState(
      'no matches',
      'Nothing matches "' + sq + '"' + (dateFrom || dateTo ? ' in the current date range.' : '.')
    );
    return;
  }
  const rows = res.map((r) => {
    const nm = dn(r.s);
    const d = nm.length > 80 ? nm.slice(0, 77) + '...' : nm;
    const tp = tokenParts(r.s);
    return {
      date: r.s.started_at,
      identity: r.p.name + ' / ' + nm,
      identityHtml: '<span class="search-proj">' + esc(r.p.name) + '</span> ' + esc(d),
      sessions: null,
      messages: r.s.msg_count,
      tokens: tp.sum,
      cost: r.s.cost || 0,
      duration: durMs(r.s),
      breakdown: tp,
      costBreakdown: costParts(r.s),
      click: 'oSR(' + r.pi + ',' + r.si + ')',
    };
  });
  M.innerHTML = buildTable('search', 'Results', rows);
}

// Open session from search results (preserve search query)
async function oSR(pi, si) {
  // Remember the caller (dash or search) so `goBack` returns there.
  prevView = view === 'dash' ? 'dash' : 'search';
  cp = pi;
  cs = si;
  view = 'messages';
  // sq stays set — used for highlighting in message view
  const s = IDX[cp].sessions[cs];
  if (cache[s.file]) {
    msgs = cache[s.file];
    render();
    return;
  }
  M.innerHTML = loadingState();
  B.className = '';
  try {
    const r = await fetch('/s/' + s.file);
    msgs = await r.json();
    cache[s.file] = msgs;
    render();
  } catch (e) {
    M.innerHTML = errorState('failed to load session', e.message);
  }
}

async function oS(i) {
  cs = i;
  prevView = 'sessions';
  view = 'messages';
  sq = '';
  q = '';
  S.value = '';
  const s = IDX[cp].sessions[i];
  if (cache[s.file]) {
    msgs = cache[s.file];
    render();
    return;
  }
  M.innerHTML = loadingState();
  B.className = '';
  try {
    const r = await fetch('/s/' + s.file);
    msgs = await r.json();
    cache[s.file] = msgs;
    render();
  } catch (e) {
    M.innerHTML = errorState('failed to load session', e.message);
  }
}

function toggleKind(k) {
  if (showKinds.has(k)) showKinds.delete(k);
  else showKinds.add(k);
  // Compact regroups by kind and search (q) filters — both need a
  // full re-render. Otherwise flip CSS class + button state; no DOM
  // rebuild = instant.
  if (compact || q) {
    render();
    return;
  }
  M.classList.toggle('hide-k-' + k, !showKinds.has(k));
  document.querySelectorAll('.fbtn[data-kind="' + k + '"]').forEach((b) => {
    b.classList.toggle('on', showKinds.has(k));
  });
  syncResetState();
}
function setDetail(v) {
  detail = v;
  render();
}
function toggleCompact() {
  compact = !compact;
  render();
}

// --- Messages view ---
function rMessages() {
  if (!msgs) {
    M.innerHTML = loadingState();
    return;
  }
  const s = IDX[cp].sessions[cs];
  const hq = sq || q; // highlight query: from global search or local search
  // Kind filtering is applied via CSS (see toggleKind); render all
  // kinds regardless. Only the search query (q) narrows the DOM.
  const filtered = msgs.filter((m) => !q || mq(m.content));
  // Apply the current kind hide classes to main so CSS filters kick in.
  ['User', 'Assistant', 'ToolUse', 'ToolResult', 'Thinking', 'System'].forEach((k) => {
    M.classList.toggle('hide-k-' + k, !showKinds.has(k));
  });

  // Compact: merge adjacent same-kind messages into one group.
  const groups = [];
  if (compact) {
    for (const m of filtered) {
      const last = groups[groups.length - 1];
      if (last && last[0].kind === m.kind && (!m.tool_name || last[0].tool_name === m.tool_name))
        last.push(m);
      else groups.push([m]);
    }
  } else {
    for (const m of filtered) groups.push([m]);
  }
  // Apply the session message sort. Default is date-asc (chronological);
  // clicking the sticky header re-orders by kind or token count.
  const mss = sortState.messages;
  if (mss.col !== 'date' || mss.desc) {
    const grpTok = (g) =>
      g.reduce((s, m) => s + (m.tokens ? m.tokens.input + m.tokens.output : 0), 0);
    const grpTs = (g) => (g[0].timestamp ? Date.parse(g[0].timestamp) : 0);
    const dir = mss.desc ? -1 : 1;
    if (mss.col === 'date') {
      groups.sort((a, b) => (grpTs(a) - grpTs(b)) * dir);
    } else if (mss.col === 'who') {
      groups.sort((a, b) => a[0].kind.localeCompare(b[0].kind) * dir);
    } else if (mss.col === 'tokens') {
      groups.sort((a, b) => (grpTok(a) - grpTok(b)) * dir);
    }
  }

  // Stat strip: per-session model/msgs/turns/tokens/cost are all visible
  // on the dashboard now, so we don't repeat them here. Only the search-
  // match counter (when a query is active) gets surfaced.
  let h = '<div class="st">';
  if (hq) {
    const mc = msgs.filter((m) => m.content && m.content.toLowerCase().includes(hq)).length;
    h +=
      '<span class="match-info">' +
      ft(mc) +
      ' matches &mdash; <kbd>n</kbd>/<kbd>N</kbd> to jump</span>';
  }
  h += '<div class="filters">';
  // Resume copies `claude -r <id>` to the clipboard. Lives just left
  // of the detail dropdown so the "act on this session" affordance is
  // grouped with the other session controls.
  h +=
    '<button class="pbtn resume" title="copy `claude -r ' +
    esc(s.id) +
    '`" onclick="navigator.clipboard.writeText(\'claude -r ' +
    esc(s.id) +
    "');this.textContent='copied!'\">resume</button>";
  // Custom dropdown (matches site theme, no OS popup).
  h += '<div class="drop" data-drop="detail" title="detail level">';
  h +=
    '<button type="button" class="drop-btn">' + detail + '<span class="drop-arr">▾</span></button>';
  h += '<div class="drop-menu hidden">';
  ['minimal', 'low', 'high', 'full'].forEach((d) => {
    h +=
      '<div class="drop-item' +
      (detail === d ? ' on' : '') +
      '" data-v="' +
      d +
      '">' +
      d +
      '</div>';
  });
  h += '</div></div>';
  h +=
    '<label class="fchk' +
    (compact ? ' on' : '') +
    '" title="merge adjacent same-kind"><input type="checkbox" ' +
    (compact ? 'checked' : '') +
    ' onchange="toggleCompact()">compact</label>';
  // Button order groups the primaries (you/ai) first, then the
  // interactive tooling (tool/think), and parks the two "noise" kinds
  // (result/sys) together on the right.
  ['User', 'Assistant', 'ToolUse', 'Thinking', 'ToolResult', 'System'].forEach((k) => {
    const on = showKinds.has(k) ? 'on' : '';
    const labels = {
      User: 'you',
      Assistant: 'ai',
      ToolUse: 'tool',
      ToolResult: 'result',
      Thinking: 'think',
      System: 'sys',
    };
    h +=
      '<button class="fbtn ' +
      on +
      '" data-kind="' +
      k +
      '" onclick="toggleKind(\'' +
      k +
      '\')">' +
      labels[k] +
      '</button>';
  });
  h += '</div></div>';

  // Detail controls truncation and folding:
  //   full    → no truncation, nothing folded
  //   high    → no truncation, fold tool/result/thinking (default)
  //   low     → truncate to 500, fold tool/result/thinking
  //   minimal → truncate to 120, fold everything except user/assistant
  const LIM = { full: Infinity, high: Infinity, low: 500, minimal: 120 }[detail];
  const FOLD_ALL = detail === 'minimal';
  const NO_FOLD = detail === 'full';

  // Sticky header row labeling the meta columns on the right. Each
  // label is clickable — clicking sorts the message list by that
  // column. Default is date-asc (chronological); click again reverses.
  const ms = sortState.messages;
  const active = (col) => (ms.col === col ? ' active' : '');
  h +=
    '<div class="msg-header">' +
    '<span class="msg-header-lbl">message</span>' +
    '<span class="msg-meta">' +
    '<span class="msg-tm msg-sort' +
    active('date') +
    '" data-msg-sort="date">date</span>' +
    '<span class="msg-sort' +
    active('who') +
    '" data-msg-sort="who">who</span>' +
    '<span class="msg-tk msg-sort' +
    active('tokens') +
    '" data-msg-sort="tokens">tokens</span>' +
    '</span></div>';
  h += '<div id="msg-list">';
  // Split render: sync prelude paints FIRST groups (extended to the
  // first match if any), remaining groups stream in via idle callbacks.
  // The dispatcher (render) already bumped renderToken, so stale idle
  // callbacks from a prior render bail on their token check.
  const myToken = renderToken;
  pendingGroups = null;
  pendingCtx = null;
  const ctx = { hq, LIM, FOLD_ALL, NO_FOLD, miRef: { n: 0 } };
  const FIRST = 30;
  let firstCount = Math.min(FIRST, groups.length);
  if (hq) {
    for (let i = firstCount; i < groups.length; i++) {
      const g = groups[i];
      const merged =
        g.length === 1 ? g[0].content : g.map((x) => x.content).join('\n\n── ── ──\n\n');
      if (merged && merged.toLowerCase().includes(hq)) {
        firstCount = i + 1;
        break;
      }
    }
  }
  let sync = '';
  for (let i = 0; i < firstCount; i++) sync += renderGroupHtml(groups[i], ctx);
  h += sync;
  h += '</div>';
  M.innerHTML = h;

  if (firstCount < groups.length) {
    pendingGroups = groups.slice(firstCount);
    pendingCtx = ctx;
  }

  // Collect match elements and scroll to first
  matchEls = M.querySelectorAll('.msg-match');
  matchIdx = -1;
  // Skip flush on the initial auto-jump: the sync prelude already
  // extended firstCount to include the first match, so the match is
  // guaranteed to be in matchEls. Flushing here would defeat chunking.
  if (hq && matchEls.length) {
    jumpMatch(1, true);
  } else {
    M.scrollTop = 0;
  }

  if (pendingGroups) scheduleChunks(myToken);
}

function renderGroupHtml(grp, ctx) {
  const { hq, LIM, FOLD_ALL, NO_FOLD, miRef } = ctx;
  const first = grp[0];
  const mergedContent =
    grp.length === 1 ? first.content : grp.map((x) => x.content).join('\n\n── ── ──\n\n');
  const hasMatch = hq && mergedContent && mergedContent.toLowerCase().includes(hq);
  // Truncate before highlighting so search mode bypasses truncation.
  let body = mergedContent || '';
  if (!hq && isFinite(LIM) && body.length > LIM) {
    body = body.slice(0, LIM) + '\n… (truncated, switch detail to see more)';
  }
  const cls = first.kind === 'User' ? 'msg msg-u' : 'msg';
  const matchCls = hasMatch ? ' msg-match' : '';
  const tc =
    {
      User: 'tg-u',
      Assistant: 'tg-a',
      ToolUse: 'tg-t',
      ToolResult: 'tg-r',
      Thinking: 'tg-k',
      System: 'tg-s',
    }[first.kind] || 'tg-s';
  const lb =
    first.kind === 'ToolUse'
      ? first.tool_name || 'tool'
      : { User: 'you', Assistant: 'ai', Thinking: 'think', System: 'sys', ToolResult: 'result' }[
          first.kind
        ] || first.kind;
  const dm = first.kind === 'ToolResult' || first.kind === 'System' ? ' dim' : '';
  const tkSum = grp.reduce((a, x) => a + (x.tokens ? x.tokens.input + x.tokens.output : 0), 0);
  const tk = tkSum ? '<span class="msg-tk">' + ft(tkSum) + '</span>' : '';
  const tm = first.timestamp ? '<span class="msg-tm">' + fd(first.timestamp) + '</span>' : '';
  const mergeTag = grp.length > 1 ? '<span class="msg-merge">×' + grp.length + '</span>' : '';
  // Cache the highlighted/fenced-code HTML per message when the
  // content isn't being narrowed by search. Re-renders from filter
  // changes (detail/compact) skip the expensive escape+regex work.
  let ct;
  if (hq) {
    ct = hl(body, hq);
  } else if (grp.length === 1 && body === mergedContent) {
    if (first._ct === undefined) first._ct = hlCode(body);
    ct = first._ct;
  } else {
    ct = hlCode(body);
  }
  let fold;
  if (NO_FOLD) fold = false;
  else if (FOLD_ALL) fold = first.kind !== 'User' && !hasMatch;
  else
    fold =
      (first.kind === 'ToolUse' || first.kind === 'ToolResult' || first.kind === 'Thinking') &&
      !hasMatch;
  let out;
  // Header strip: meta lives in a single fixed-column block on the
  // right (date | who-tag | tokens). Mirrors the column convention
  // used everywhere else in the app — date first, then identity, then
  // numeric. Keeps the body's left edge clean.
  const meta =
    '<span class="msg-meta">' +
    tm +
    '<span class="tg ' +
    tc +
    '">' +
    esc(lb) +
    '</span>' +
    tk +
    '</span>';
  if (fold) {
    const preview = esc((body || '').split('\n')[0].slice(0, 120));
    out =
      '<details class="' +
      cls +
      matchCls +
      '" data-msgkind="' +
      first.kind +
      '"><summary class="msg-h">' +
      mergeTag +
      '<span class="msg-preview' +
      dm +
      '">' +
      preview +
      '</span>' +
      meta +
      '</summary><div class="msg-b' +
      dm +
      '">' +
      ct +
      '</div></details>';
  } else {
    out =
      '<div class="' +
      cls +
      matchCls +
      '" data-msgkind="' +
      first.kind +
      '" ' +
      (hasMatch ? 'data-mi="' + miRef.n + '"' : '') +
      '><div class="msg-h">' +
      mergeTag +
      meta +
      '</div><div class="msg-b' +
      dm +
      '">' +
      ct +
      '</div></div>';
  }
  if (hasMatch) miRef.n++;
  return out;
}

function scheduleChunks(myToken) {
  const ric =
    window.requestIdleCallback || ((cb) => setTimeout(() => cb({ timeRemaining: () => 5 }), 0));
  const tick = (deadline) => {
    if (renderToken !== myToken || !pendingGroups) return;
    const hasIdle = deadline && typeof deadline.timeRemaining === 'function';
    let html = '',
      rendered = 0;
    while (pendingGroups.length && rendered < 50 && (!hasIdle || deadline.timeRemaining() > 2)) {
      html += renderGroupHtml(pendingGroups.shift(), pendingCtx);
      rendered++;
    }
    if (html) {
      const list = document.getElementById('msg-list');
      if (list) list.insertAdjacentHTML('beforeend', html);
    }
    if (pendingGroups.length) ric(tick);
    else {
      pendingGroups = null;
      pendingCtx = null;
      matchEls = M.querySelectorAll('.msg-match');
    }
  };
  ric(tick);
}

function flushAllPending() {
  if (!pendingGroups || !pendingGroups.length) return;
  let html = '';
  for (const g of pendingGroups) html += renderGroupHtml(g, pendingCtx);
  pendingGroups = null;
  pendingCtx = null;
  const list = document.getElementById('msg-list');
  if (list) list.insertAdjacentHTML('beforeend', html);
  matchEls = M.querySelectorAll('.msg-match');
}

function jumpMatch(dir, noFlush) {
  // On a user-driven jump (n/N keypress), matches in later chunks
  // aren't in the DOM yet — flush so navigation never silently skips
  // a match. The initial auto-jump from rMessages passes noFlush since
  // the sync prelude already included the first match.
  if (!noFlush && pendingGroups && pendingGroups.length) flushAllPending();
  const n = matchEls.length;
  if (!n) return;
  // Remove highlight from current (may be stale after re-render).
  if (matchIdx >= 0 && matchIdx < n) matchEls[matchIdx].classList.remove('msg-focus');
  // Modular arithmetic keeps index valid even when dir has large
  // magnitude or the DOM shrank since last call.
  matchIdx = (((matchIdx + dir) % n) + n) % n;
  const el = matchEls[matchIdx];
  if (!el) return;
  el.classList.add('msg-focus');
  el.scrollIntoView({ block: 'center', behavior: 'smooth' });
  if (el.tagName === 'DETAILS') el.open = true;
}

// --- Dashboard aggregation (memoized) ---
//
// Single walk over every session in scope, producing the bundle of
// derived structures the dashboard consumes (totals, by-model,
// by-project, by-day, by-hour, top-N sessions). Cached because none of
// the dashboard's interactive affordances (sort, expand, navigation
// back) actually change the underlying numbers — only filter changes do.
let dashAggCache = null,
  dashAggKey = '';
function dashAggKeyOf() {
  return JSON.stringify({
    s: dashScope,
    df: dateFrom,
    dt: dateTo,
    m: [...modelFilters].sort(),
  });
}
function computeDashAgg() {
  const k = dashAggKeyOf();
  if (dashAggCache && dashAggKey === k) return dashAggCache;
  let ts = 0,
    tm = 0,
    tc = 0,
    ti = 0,
    to = 0,
    tcr = 0,
    tcc = 0;
  const byProject = [];
  const byModel = {};
  const byDay = {};
  let earliest = null,
    latest = null;
  const byProjectMap = {};
  const byHour = {};
  const byTool = {};
  const activeDays = new Set();
  let projCount = 0;
  let maxDayTokens = 0;
  const allSessions = [];
  for (let h = 0; h < 24; h++)
    byHour[h] = { input: 0, output: 0, cache_read: 0, cache_create: 0, _cost: 0, _count: 0 };
  IDX.forEach((p, pi) => {
    if ((dashScope.kind === 'project' || dashScope.kind === 'session') && pi !== dashScope.pi)
      return;
    let pc = 0,
      psess = 0,
      pmsgs = 0,
      ptok = 0,
      pin = 0,
      pout = 0,
      pcr = 0,
      pcc = 0,
      pci = 0,
      pco = 0,
      pccr = 0,
      pccc = 0,
      pLast = '',
      pdur = 0;
    p.sessions.forEach((s, si) => {
      if (dashScope.kind === 'session' && si !== dashScope.si) return;
      if (!inDate(s.started_at) || !inModel(s)) return;
      allSessions.push({ s, p: p.name, pi, si });
      const tin = s.total_input_tokens || 0;
      const tout = s.total_output_tokens || 0;
      const tcache_r = s.total_cache_read || 0;
      const tcache_w = s.total_cache_create || 0;
      const tcost = s.cost || 0;
      const ci = s.cost_input || 0;
      const co = s.cost_output || 0;
      const ccr = s.cost_cache_read || 0;
      const ccc = s.cost_cache_create || 0;
      const tokSum = tin + tout + tcache_r + tcache_w;
      const sdur = durMs(s);
      ts++;
      tm += s.msg_count;
      tc += tcost;
      ti += tin;
      to += tout;
      tcr += tcache_r;
      tcc += tcache_w;
      pc += tcost;
      psess++;
      pmsgs += s.msg_count;
      ptok += tokSum;
      pdur += sdur;
      pin += tin;
      pout += tout;
      pcr += tcache_r;
      pcc += tcache_w;
      pci += ci;
      pco += co;
      pccr += ccr;
      pccc += ccc;
      if (s.started_at && s.started_at > pLast) pLast = s.started_at;
      const mod = s.model || 'unknown';
      const mRec =
        byModel[mod] ||
        (byModel[mod] = {
          sessions: 0,
          msgs: 0,
          tokens: 0,
          cost: 0,
          lastActive: '',
          input: 0,
          output: 0,
          cache_read: 0,
          cache_create: 0,
          duration: 0,
          cost_input: 0,
          cost_output: 0,
          cost_cache_read: 0,
          cost_cache_create: 0,
        });
      mRec.sessions++;
      mRec.msgs += s.msg_count;
      mRec.tokens += tokSum;
      mRec.cost += tcost;
      mRec.input += tin;
      mRec.output += tout;
      mRec.cache_read += tcache_r;
      mRec.cache_create += tcache_w;
      mRec.duration += sdur;
      mRec.cost_input += ci;
      mRec.cost_output += co;
      mRec.cost_cache_read += ccr;
      mRec.cost_cache_create += ccc;
      if (s.started_at && s.started_at > mRec.lastActive) mRec.lastActive = s.started_at;
      // msgs + session counts per day are still keyed on `started_at`
      // (a session is "one session" regardless of how it's distributed
      // across midnights). Token + cost bucketing is handled below in
      // the DAILY.rows pass so cross-midnight tokens attribute to the
      // calendar day the message actually landed on.
      const startDay = dayStr(s.started_at);
      if (startDay) {
        const dRec = byDay[startDay] || (byDay[startDay] = emptyDay());
        dRec.msgs += s.msg_count;
        dRec.sessions++;
        activeDays.add(startDay);
      }
      const rows = s.hourly;
      if (rows && rows.length) {
        let sessTok = 0;
        for (const r of rows) sessTok += r[1] + r[2] + r[3] + r[4];
        const costPerTok = sessTok > 0 ? tcost / sessTok : 0;
        for (const r of rows) {
          const hour = new Date(r[0] * 1000).getHours();
          const b = byHour[hour];
          b.input += r[1];
          b.output += r[2];
          b.cache_read += r[3];
          b.cache_create += r[4];
          b._cost += (r[1] + r[2] + r[3] + r[4]) * costPerTok;
          b._count++;
        }
      }
      // Tool invocation counts — summed across the in-scope sessions
      // and rendered as the pie's "by tool" mode.
      if (s.tool_counts) {
        for (const [name, n] of Object.entries(s.tool_counts)) {
          byTool[name] = (byTool[name] || 0) + n;
        }
      }
    });
    if (psess > 0) {
      projCount++;
      byProject.push({
        name: p.name,
        pi,
        sessions: psess,
        msgs: pmsgs,
        tokens: ptok,
        cost: pc,
        lastActive: pLast,
        input: pin,
        output: pout,
        cache_read: pcr,
        cache_create: pcc,
        duration: pdur,
        cost_input: pci,
        cost_output: pco,
        cost_cache_read: pccr,
        cost_cache_create: pccc,
      });
      byProjectMap[p.name] = {
        input: pin,
        output: pout,
        cache_read: pcr,
        cache_create: pcc,
        cost: pc,
        msgs: pmsgs,
        sessions: psess,
        tokens: ptok,
      };
    }
  });
  // Token + cost per day come from DAILY — the same cache preaggs that
  // back `ccaudit daily`. Preaggs are keyed on (day, model, project)
  // with cross-session dedup applied at build time, so a session that
  // spans midnight attributes to both days correctly.
  //
  // Filters: project scope maps to a project-name allow-set; date and
  // model filters replay the dropdown state against each row's day
  // string / model name.
  //
  // Session scope has no per-session dimension in DAILY (preaggs fold
  // sessions together). For a scope that small the heatmap is a
  // decoration anyway, so fall back to the session's own `hourly`
  // rolled up per UTC day with cost distributed proportional to tokens.
  if (dashScope.kind === 'session') {
    const one = allSessions.length === 1 ? allSessions[0].s : null;
    if (one && one.hourly && one.hourly.length) {
      let sessTok = 0;
      for (const r of one.hourly) sessTok += r[1] + r[2] + r[3] + r[4];
      const costPerTok = sessTok > 0 ? (one.cost || 0) / sessTok : 0;
      for (const r of one.hourly) {
        const day = new Date(r[0] * 1000).toISOString().slice(0, 10);
        const dRec = byDay[day] || (byDay[day] = emptyDay());
        const rowTok = r[1] + r[2] + r[3] + r[4];
        dRec.input += r[1];
        dRec.output += r[2];
        dRec.cache_read += r[3];
        dRec.cache_create += r[4];
        dRec.tokens += rowTok;
        dRec.cost += rowTok * costPerTok;
        if (dRec.tokens > maxDayTokens) maxDayTokens = dRec.tokens;
        if (!earliest || day < earliest) earliest = day;
        if (!latest || day > latest) latest = day;
        activeDays.add(day);
      }
    }
  } else if (DAILY && DAILY.rows && DAILY.rows.length) {
    // Allowed project indices into DAILY.p — matches by name against the
    // same prettified slug used on both sides (see `prettify_project_name`
    // in source/claude_code.rs).
    const allowProj = new Set();
    IDX.forEach((p, pi) => {
      if (dashScope.kind === 'project' && pi !== dashScope.pi) return;
      const idx = DAILY.p.indexOf(p.name);
      if (idx >= 0) allowProj.add(idx);
    });
    const noModelFilter = modelFilters.size === 0;
    for (const row of DAILY.rows) {
      const [di, pi, mi, inp, out, cr, cw, cost] = row;
      if (!allowProj.has(pi)) continue;
      const day = DAILY.d[di];
      if (dateFrom && day < dateFrom) continue;
      if (dateTo && day > dateTo) continue;
      if (!noModelFilter) {
        const name = mi < 0 ? '' : DAILY.m[mi];
        if (!modelFilters.has(name)) continue;
      }
      const dRec = byDay[day] || (byDay[day] = emptyDay());
      dRec.input += inp;
      dRec.output += out;
      dRec.cache_read += cr;
      dRec.cache_create += cw;
      dRec.tokens += inp + out + cr + cw;
      dRec.cost += cost;
      if (dRec.tokens > maxDayTokens) maxDayTokens = dRec.tokens;
      if (!earliest || day < earliest) earliest = day;
      if (!latest || day > latest) latest = day;
      activeDays.add(day);
    }
  }
  const nDays = Math.max(1, activeDays.size);
  for (let h = 0; h < 24; h++) {
    const b = byHour[h];
    b.input /= nDays;
    b.output /= nDays;
    b.cache_read /= nDays;
    b.cache_create /= nDays;
    b._cost /= nDays;
    b._days = activeDays.size;
  }
  byProject.sort((a, b) => b.cost - a.cost);
  allSessions.sort((a, b) => b.s.cost - a.s.cost);
  dashAggCache = {
    ts,
    tm,
    tc,
    ti,
    to,
    tcr,
    tcc,
    byProject,
    byModel,
    byDay,
    byProjectMap,
    byHour,
    byTool,
    projCount,
    maxDayTokens,
    allSessions,
    earliest,
    latest,
  };
  dashAggKey = k;
  return dashAggCache;
}

// --- Dashboard view ---
function rDash() {
  // Aggregation is the expensive part of rDash — running it on every
  // render (sort click, expand-more click, navigation back to dash)
  // wastes work since none of those actions invalidate the underlying
  // numbers. Cache keyed on the inputs that actually shape the result.
  const agg = computeDashAgg();
  const {
    ts,
    tm,
    tc,
    ti,
    to,
    tcr,
    tcc,
    byProject,
    byModel,
    byDay,
    byProjectMap,
    byHour,
    byTool,
    projCount,
    maxDayTokens,
    allSessions,
  } = agg;

  let h = '<div class="dash">';

  // Overview cards
  h += '<h2>overview</h2><div class="cards">';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(ts) +
    '</div><div class="card-lbl">sessions</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(tm) +
    '</div><div class="card-lbl">messages</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(projCount) +
    '</div><div class="card-lbl">projects</div></div>';
  h +=
    '<div class="card"><div class="card-val green">' +
    fc(tc) +
    '</div><div class="card-lbl">estimated cost</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(ti) +
    '</div><div class="card-lbl">input tokens</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(to) +
    '</div><div class="card-lbl">output tokens</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(tcc) +
    '</div><div class="card-lbl">cache write</div></div>';
  h +=
    '<div class="card"><div class="card-val">' +
    ft(tcr) +
    '</div><div class="card-lbl">cache read</div></div>';
  h += '</div>';

  // Charts: big pie on the left spanning both rows; histogram on top
  // right; github-style heatmap bottom right. Full-width.
  h += '<h2>at a glance</h2>';
  h += '<div class="dash-charts">';
  // Pie chart panel: toggle buttons double as the section label; no
  // redundant "by model" / "by tool" heading.
  const pieData = pieMode === 'tool' ? byTool : byModel;
  h +=
    '<div class="chart-panel pie-panel"><div class="panel-head"><div class="pie-toggle">' +
    buildModeToggle(
      [
        ['model', 'model'],
        ['tool', 'tool'],
      ],
      pieMode,
      'setPieMode'
    ) +
    '</div></div>' +
    buildPie(pieData, pieMode) +
    '</div>';
  h +=
    '<div class="chart-panel hist-panel"><div class="panel-head"><h3>tokens by</h3><div class="hist-toggle">';
  h += buildModeToggle(
    [
      ['day', 'day'],
      ['project', 'project'],
      ['hour', 'hour'],
    ],
    histMode,
    'setHistMode'
  );
  h += '</div></div>';
  if (histMode === 'hour') {
    h += buildHist(byHour, 'hour', CATS_TOKENS);
  } else {
    h += buildHist(histMode === 'project' ? byProjectMap : byDay, histMode, CATS_TOKENS);
  }
  h += '</div>';
  h +=
    '<div class="chart-panel heat-panel"><h3>activity</h3>' +
    buildHeatmap(byDay, maxDayTokens) +
    '</div>';
  h += '</div>';

  // Breakdown tables — rendered through the unified buildTable so they
  // share columns/sort/tooltips with the landing/sessions views. Tables
  // that would degenerate to a single row under the current scope are
  // skipped (e.g. by-project under project scope, all three under
  // session scope) instead of taking up screen real estate.

  // By model. Always shown — even a single-session scope can split
  // into multiple models if the session used more than one.
  const modelRows = Object.entries(byModel).map(([m, d]) => ({
    date: d.lastActive,
    identity: m,
    sessions: dashScope.kind === 'session' ? null : d.sessions,
    messages: d.msgs,
    tokens: d.tokens,
    cost: d.cost,
    duration: d.duration,
    breakdown: {
      input: d.input,
      output: d.output,
      cache_read: d.cache_read,
      cache_create: d.cache_create,
    },
    costBreakdown: {
      input: d.cost_input,
      output: d.cost_output,
      cache_read: d.cost_cache_read,
      cache_create: d.cost_cache_create,
    },
    dataModel: m,
  }));
  h += '<h2>by model</h2>' + buildTable('dashByModel', 'Models', modelRows, dashLimit.dashByModel);

  // By session — only meaningful when more than one session can show
  // up. Hidden under session scope.
  if (dashScope.kind !== 'session') {
    const sessionRows = allSessions.map(({ s, p, pi, si }) => {
      const nm = dn(s);
      const tp = tokenParts(s);
      return {
        date: s.started_at,
        identity: p + ' / ' + nm,
        identityHtml:
          '<span class="dim">' +
          esc(p) +
          '</span> / ' +
          esc(nm.length > 60 ? nm.slice(0, 57) + '...' : nm),
        sessions: null,
        messages: s.msg_count,
        tokens: tp.sum,
        cost: s.cost || 0,
        duration: durMs(s),
        breakdown: tp,
        costBreakdown: costParts(s),
        click: 'oSR(' + pi + ',' + si + ')',
      };
    });
    h +=
      '<h2>by session</h2>' +
      buildTable('dashBySession', 'Sessions', sessionRows, dashLimit.dashBySession);
  }

  // By project — only meaningful at universal scope. Hidden under
  // project or session scope (both reduce to a single row).
  if (dashScope.kind === 'all') {
    const projectRows = byProject.map((p) => ({
      date: p.lastActive,
      identity: p.name,
      sessions: p.sessions,
      messages: p.msgs,
      tokens: p.tokens,
      cost: p.cost,
      duration: p.duration,
      breakdown: {
        input: p.input,
        output: p.output,
        cache_read: p.cache_read,
        cache_create: p.cache_create,
      },
      costBreakdown: {
        input: p.cost_input,
        output: p.cost_output,
        cache_read: p.cost_cache_read,
        cache_create: p.cost_cache_create,
      },
      click: 'oP(' + p.pi + ')',
    }));
    h +=
      '<h2>by project</h2>' +
      buildTable('dashByProject', 'Projects', projectRows, dashLimit.dashByProject);
  }

  h += '</div>';
  M.innerHTML = h;
  // Warm the session cache for the rows the user is most likely to
  // click next — the top-cost sessions AND the hovered-first few in
  // the by-project table. Fire-and-forget; failures don't matter.
  prefetchSessions(allSessions.slice(0, 8));
}

// Kick off background fetches for session JSON files so clicking the
// row later resolves from cache instantly. Uses an in-flight set to
// de-dupe requests across rDash re-renders.
const prefetchInFlight = new Set();
function prefetchSessions(rows) {
  for (const r of rows) {
    const s = IDX[r.pi] && IDX[r.pi].sessions[r.si];
    if (!s) continue;
    const file = s.file;
    if (cache[file] || prefetchInFlight.has(file)) continue;
    prefetchInFlight.add(file);
    fetch('/s/' + file)
      .then((res) => res.json())
      .then((m) => {
        cache[file] = m;
      })
      .catch(() => {})
      .finally(() => {
        prefetchInFlight.delete(file);
      });
  }
}

function buildHeatmap(byDay, maxTokens) {
  const today = new Date();
  const dayMs = 86400000;
  const end = new Date(today);
  end.setHours(0, 0, 0, 0);
  const startOff = end.getDay();
  const start = new Date(end.getTime() - (52 * 7 + startOff) * dayMs);

  // Fall back to scanning byDay if caller didn't pre-compute the max.
  if (maxTokens == null) {
    maxTokens = 0;
    for (const k in byDay) {
      if (byDay[k].tokens > maxTokens) maxTokens = byDay[k].tokens;
    }
  }

  let h = '<div class="heatmap-wrap"><div class="heatmap">';
  // Pre-build YYYY-MM-DD strings for the 53*7 cells in one UTC pass.
  // toISOString() is expensive; compute it once per cell up-front.
  const keys = new Array(53 * 7);
  const cur = new Date(start);
  for (let i = 0; i < keys.length; i++) {
    keys[i] = cur.toISOString().slice(0, 10);
    cur.setTime(cur.getTime() + dayMs);
  }
  let idx = 0;

  for (let w = 0; w < 53; w++) {
    h += '<div class="heatmap-col">';
    for (let d = 0; d < 7; d++) {
      const key = keys[idx++];
      const data = byDay[key];
      let lvl = '';
      if (data) {
        const ratio = maxTokens > 0 ? data.tokens / maxTokens : 0;
        if (ratio > 0.75) lvl = ' l4';
        else if (ratio > 0.4) lvl = ' l3';
        else if (ratio > 0.15) lvl = ' l2';
        else lvl = ' l1';
      }
      const tip = data
        ? key +
          '|' +
          ft(data.tokens) +
          ' tok|' +
          ft(data.msgs) +
          ' msgs|' +
          data.sessions +
          ' sessions|' +
          fc(data.cost)
        : key + '|no activity';
      h +=
        '<div class="heatmap-cell' +
        lvl +
        '" data-tip="' +
        esc(tip) +
        '" data-day="' +
        key +
        '"></div>';
    }
    h += '</div>';
  }
  h += '</div>';
  h += '<div class="heatmap-tip" id="htip"></div>';

  h += '<div class="heatmap-lbl"><span>less</span>';
  h += '<div class="heatmap-cell"></div>';
  h += '<div class="heatmap-cell l1"></div>';
  h += '<div class="heatmap-cell l2"></div>';
  h += '<div class="heatmap-cell l3"></div>';
  h += '<div class="heatmap-cell l4"></div>';
  h += '<span>more</span></div>';
  h += '</div>';
  return h;
}

// Chart palette. Reuses theme variables so dark/light themes carry over.
const CHART_COLORS = [
  'var(--accent)',
  'var(--cyan)',
  'var(--green)',
  'var(--yellow)',
  'var(--magenta)',
  'var(--red)',
  '#ffb89a',
  '#c8e0b0',
];

// Default sub-bar category set for day/project modes — the four token
// columns in their standard colors.
const CATS_TOKENS = [
  { key: 'input', color: 'var(--yellow)', label: 'in' },
  { key: 'output', color: 'var(--green)', label: 'out' },
  { key: 'cache_create', color: 'var(--cyan)', label: 'cache-w' },
  { key: 'cache_read', color: 'var(--accent)', label: 'cache-r' },
];

// (Hour aggregation is now folded into rDash's single-pass loop so
// dashboards don't walk IDX.sessions twice.)

// Pie/donut with mode-aware slice sizing.
//   mode='model' — data is {model: {cost, tokens, ...}}, slices sized by cost
//   mode='tool'  — data is {tool_name: count}, slices sized by count
// Legend column widths stay fixed (grid) so the pct/value align across
// rows regardless of label length.
function buildPie(data, mode) {
  const isTool = mode === 'tool';
  const entries = Object.entries(data)
    .map(([k, v]) => ({ k, value: isTool ? v : v.cost || 0, extra: v }))
    .filter((e) => e.value > 0)
    .sort((a, b) => b.value - a.value);
  const total = entries.reduce((a, e) => a + e.value, 0);
  if (!total) return emptyState('no data');
  // Fill the panel — the SVG scales to 100% width up to a cap, so it
  // lives at roughly the full panel width while the legend stays dense.
  const cx = 100,
    cy = 100,
    r = 90;
  // Subtle dark separators between slices and on the outer ring improve
  // legibility against adjacent same-hue colors. Stroke is shared (group
  // attribute) so the SVG stays compact.
  let svg =
    '<svg class="pie-svg" viewBox="0 0 200 200" preserveAspectRatio="xMidYMid meet"><g stroke="black" stroke-width="0.6" stroke-opacity="0.55" stroke-linejoin="round">';
  let cum = 0;
  const legend = [];
  entries.forEach((e, i) => {
    const frac = e.value / total;
    const color = CHART_COLORS[i % CHART_COLORS.length];
    if (frac >= 0.9995) {
      svg += '<circle cx="' + cx + '" cy="' + cy + '" r="' + r + '" fill="' + color + '"/>';
    } else if (frac > 0) {
      const a1 = cum * 2 * Math.PI;
      const a2 = (cum + frac) * 2 * Math.PI;
      const x1 = cx + r * Math.sin(a1),
        y1 = cy - r * Math.cos(a1);
      const x2 = cx + r * Math.sin(a2),
        y2 = cy - r * Math.cos(a2);
      const large = a2 - a1 > Math.PI ? 1 : 0;
      const tipValue = isTool ? ft(e.value) + ' calls' : fc(e.value);
      const tip = e.k + '|' + (frac * 100).toFixed(1) + '%|' + tipValue;
      // data-model only emitted in model mode so clicking a slice
      // filters by that model. Tool slices don't currently filter.
      const extraAttr = isTool ? '' : ' data-model="' + esc(e.k) + '"';
      svg +=
        '<path d="M ' +
        cx +
        ' ' +
        cy +
        ' L ' +
        x1.toFixed(2) +
        ' ' +
        y1.toFixed(2) +
        ' A ' +
        r +
        ' ' +
        r +
        ' 0 ' +
        large +
        ' 1 ' +
        x2.toFixed(2) +
        ' ' +
        y2.toFixed(2) +
        ' Z" fill="' +
        color +
        '" data-tip="' +
        esc(tip) +
        '"' +
        extraAttr +
        '/>';
    }
    cum += frac;
    legend.push({ k: e.k, frac, value: e.value, color });
  });
  // Outer ring: drawn last so the stroke isn't visually broken by slice
  // edges. Slightly heavier than the radial separators for emphasis.
  svg +=
    '<circle cx="' +
    cx +
    '" cy="' +
    cy +
    '" r="' +
    r +
    '" fill="none" stroke-width="0.8" stroke-opacity="0.7"/>';
  svg += '</g></svg>';
  let h = '<div class="pie-wrap">' + svg + '<div class="pie-legend">';
  legend.forEach(({ k, frac, value, color }) => {
    const pct = (frac * 100).toFixed(frac < 0.1 ? 1 : 0);
    // Four fixed columns (swatch | name | pct | value) so percent and
    // value line up regardless of label length. `value` is cost in
    // model mode, count of calls in tool mode.
    const valueStr = isTool ? ft(value) : fc(value);
    h +=
      '<div class="pie-item"><span class="pie-sw" style="background:' +
      color +
      '"></span><span class="pie-name">' +
      esc(k) +
      '</span><span class="pie-pct">' +
      pct +
      '%</span><span class="pie-cost">' +
      valueStr +
      '</span></div>';
  });
  h += '</div></div>';
  return h;
}

// Round a number up to a tight 2-significant-figure ceiling. Keep the
// result on an even step so the ½-midpoint label stays clean. e.g.
// 7M→7M, 7.25M→7.4M, 45K→46K. Avoids the fat top-of-chart gap that
// "nearest 1/2/5" rounding creates (7M would jump to 10M).
function niceCeil(n) {
  if (n <= 0) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(n)) - 1);
  return Math.ceil(n / (2 * p)) * 2 * p;
}
// Short y-axis label: 0 / 12K / 3.4M / 1.2B.
function fmtY(n) {
  if (n >= 1e9) return (n / 1e9).toFixed(1) + 'B';
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(0) + 'K';
  return String(Math.round(n));
}

// Grouped-bar histogram of token use per day on a LOG₁₀ y-axis.
// Each day renders 4 thin side-by-side bars (in/out/cache-w/cache-r)
// so every category keeps its own visible height regardless of how
// badly cache-r dominates. Log scale on y-axis; bars floor at 1K so
// idle days leave a visible gap.
//
// y-axis: one label per decade between 1K and niceMax (thinned to ~5).
// x-axis: rotated MM-DD every ~12th group (+ last).
function buildHist(data, mode, cats) {
  const entries = Object.entries(data);
  if (!entries.length) return emptyState('no data');
  // Day: chronological, cap last 120. Project: total-token desc, top
  // 30. Time: numeric hour 0..23 sort.
  if (mode === 'project') {
    entries.sort((a, b) => {
      const sum = (o) => cats.reduce((a, c) => a + (o[c.key] || 0), 0);
      return sum(b[1]) - sum(a[1]);
    });
  } else if (mode === 'hour') {
    // Rotate so the day starts at 06:00 (typical wake/work time).
    // Hours < 6 wrap to the end.
    const rot = (h) => (h - 6 + 24) % 24;
    entries.sort((a, b) => rot(Number(a[0])) - rot(Number(b[0])));
  } else {
    entries.sort((a, b) => (a[0] < b[0] ? -1 : 1));
  }
  const show =
    mode === 'project'
      ? entries.slice(0, 16)
      : mode === 'day' && entries.length > 120
        ? entries.slice(-120)
        : entries;
  const labelFor =
    mode === 'day'
      ? (k) => k.slice(5)
      : mode === 'hour'
        ? (k) => String(k).padStart(2, '0') + ':00'
        : (k) => (k.length > 18 ? k.slice(0, 17) + '…' : k);

  const PAD_L = 52,
    PAD_R = 10,
    PAD_T = 8,
    PAD_B = 52;
  const H_CHART = 220;
  // Fixed viewBox width + bars sized to fill it. Aspect ratio stays
  // constant so CSS width:100%+height:auto gives a chart that fills
  // both panel dimensions without distortion and without clipping.
  const nCats = cats.length || 1;
  const subGap = nCats > 1 ? 1 : 0;
  const TARGET_CHART_W = 880;
  const perGroup = TARGET_CHART_W / show.length;
  const dayGap = Math.max(2, perGroup * 0.2);
  const groupW = perGroup - dayGap;
  const subW = Math.max(1, (groupW - (nCats - 1) * subGap) / nCats);
  const chartW = TARGET_CHART_W;
  const W = PAD_L + chartW + PAD_R;
  const H = PAD_T + H_CHART + PAD_B;

  // Max over any category on any entry — the y-axis top.
  let maxSeg = 0;
  show.forEach((e) => {
    const v = e[1];
    cats.forEach((c) => {
      const h = v[c.key] || 0;
      if (h > maxSeg) maxSeg = h;
    });
  });
  const floor = 1000;
  const logFloor = Math.log10(floor);
  const logMax = Math.max(logFloor + 1, Math.ceil(Math.log10(Math.max(maxSeg, 10))));
  const logRange = logMax - logFloor;
  const niceMaxLog = Math.pow(10, logMax);
  const niceMaxLin = niceCeil(maxSeg || 1);
  const barPx = histLog
    ? (v) => (v < floor ? 0 : ((Math.log10(v) - logFloor) / logRange) * H_CHART)
    : (v) => (v <= 0 ? 0 : (v / niceMaxLin) * H_CHART);

  let svg = '<svg class="hist-svg" viewBox="0 0 ' + W + ' ' + H + '">';

  // Y-axis: log → one label per decade (thinned to ~5).
  //         linear → 0 / ½max / max.
  if (histLog) {
    const yStride = Math.max(1, Math.ceil(logRange / 5));
    for (let k = 0; k <= logRange; k += yStride) {
      const v = Math.pow(10, logFloor + k);
      const y = PAD_T + H_CHART - (k / logRange) * H_CHART;
      svg +=
        '<line x1="' +
        PAD_L +
        '" y1="' +
        y.toFixed(1) +
        '" x2="' +
        (W - PAD_R) +
        '" y2="' +
        y.toFixed(1) +
        '" stroke="var(--border)" stroke-width="0.5"/>';
      svg +=
        '<text x="' +
        (PAD_L - 4) +
        '" y="' +
        (y + 3).toFixed(1) +
        '" font-size="9" fill="var(--fg3)" text-anchor="end">' +
        fmtY(v) +
        '</text>';
    }
    const y = PAD_T;
    svg +=
      '<line x1="' +
      PAD_L +
      '" y1="' +
      y +
      '" x2="' +
      (W - PAD_R) +
      '" y2="' +
      y +
      '" stroke="var(--border)" stroke-width="0.5"/>';
    svg +=
      '<text x="' +
      (PAD_L - 4) +
      '" y="' +
      (y + 3) +
      '" font-size="9" fill="var(--fg3)" text-anchor="end">' +
      fmtY(niceMaxLog) +
      '</text>';
  } else {
    [0, niceMaxLin / 2, niceMaxLin].forEach((v) => {
      const y = PAD_T + H_CHART - (v / niceMaxLin) * H_CHART;
      svg +=
        '<line x1="' +
        PAD_L +
        '" y1="' +
        y.toFixed(1) +
        '" x2="' +
        (W - PAD_R) +
        '" y2="' +
        y.toFixed(1) +
        '" stroke="var(--border)" stroke-width="0.5"/>';
      svg +=
        '<text x="' +
        (PAD_L - 4) +
        '" y="' +
        (y + 3).toFixed(1) +
        '" font-size="9" fill="var(--fg3)" text-anchor="end">' +
        fmtY(v) +
        '</text>';
    });
  }

  // Hour mode: label every 3 hours (0/3/6/9/12/15/18/21) so rotated
  // "HH:00" text doesn't collide. Other modes: aim for ~12 labels.
  const xStride = mode === 'hour' ? 3 : Math.max(1, Math.ceil(show.length / 12));
  const lastIdx = show.length - 1;

  show.forEach((entry, i) => {
    const k = entry[0],
      v = entry[1];
    const gx = PAD_L + i * (groupW + dayGap);
    cats.forEach((cat, j) => {
      const h = v[cat.key] || 0;
      const bh = barPx(h);
      if (bh <= 0) return;
      const bx = gx + j * (subW + subGap);
      const by = PAD_T + H_CHART - bh;
      svg +=
        '<rect x="' +
        bx.toFixed(1) +
        '" y="' +
        by.toFixed(1) +
        '" width="' +
        subW +
        '" height="' +
        bh.toFixed(1) +
        '" fill="' +
        cat.color +
        '"/>';
    });

    // Label strategy: project always labels every bar; day + hour use stride.
    const doLabel = mode === 'project' || i % xStride === 0 || i === lastIdx;
    if (doLabel) {
      const lx = gx + groupW / 2;
      const ly = PAD_T + H_CHART + 8;
      svg +=
        '<text x="' +
        lx.toFixed(1) +
        '" y="' +
        ly +
        '" font-size="9" fill="var(--fg3)" text-anchor="end" transform="rotate(-55 ' +
        lx.toFixed(1) +
        ' ' +
        ly +
        ')">' +
        esc(labelFor(k)) +
        '</text>';
    }

    // Tooltip: one line per category, plus total + (cost if available).
    const hdr = mode === 'hour' ? String(k).padStart(2, '0') + ':00' : k;
    let total = 0;
    const catLines = cats.map((c) => {
      const h = v[c.key] || 0;
      total += h;
      return c.label + ': ' + ft(Math.round(h));
    });
    let tip = hdr + '|' + catLines.join('|') + '|total: ' + ft(Math.round(total));
    if (v.cost != null) tip += '|cost: ' + fc(v.cost);
    else if (v._cost != null) tip += '|cost: ' + fc(v._cost);
    // Only day bars are click-to-filter.
    const clickAttr = mode === 'day' ? ' data-day="' + k + '"' : '';
    svg +=
      '<rect class="hist-hit" x="' +
      gx.toFixed(1) +
      '" y="' +
      PAD_T +
      '" width="' +
      groupW +
      '" height="' +
      H_CHART +
      '" fill="transparent" data-tip="' +
      esc(tip) +
      '"' +
      clickAttr +
      '/>';
  });

  svg += '</svg>';
  // Legend generated from cats. Truncate long labels (model names).
  const legend = cats
    .map((c) => {
      const l = c.label.length > 22 ? c.label.slice(0, 21) + '…' : c.label;
      return (
        '<span><span class="pie-sw" style="background:' + c.color + '"></span>' + esc(l) + '</span>'
      );
    })
    .join('');
  const scalePart = histLog ? 'log₁₀ · ≥1K' : 'linear';
  const prefix = mode === 'hour' ? 'avg/day · ' : '';
  const toggle =
    '<button class="pbtn hist-log-toggle" onclick="toggleHistLog()" title="toggle log/linear scale">' +
    prefix +
    scalePart +
    '</button>';
  return (
    '<div class="hist-wrap">' +
    svg +
    '</div>' +
    '<div class="hist-legend">' +
    legend +
    '<span style="margin-left:auto">' +
    toggle +
    '</span>' +
    '</div>'
  );
}

// Instant, native-feel tooltip for any chart element carrying data-tip.
// Pipe-separated fields render one per line; first field is treated as
// the "header" (typically the date).
document.addEventListener('mouseover', (e) => {
  const el = e.target.closest && e.target.closest('[data-tip]');
  if (!el) return;
  const tip = $('#htip');
  if (!tip) return;
  const parts = el.getAttribute('data-tip').split('|');
  tip.innerHTML =
    '<div class="tip-date">' +
    parts[0] +
    '</div>' +
    parts
      .slice(1)
      .map((p) => '<div>' + p + '</div>')
      .join('');
  tip.style.display = 'block';
  const r = el.getBoundingClientRect();
  tip.style.left = r.left + 'px';
  tip.style.top = r.top - tip.offsetHeight - 6 + 'px';
});
document.addEventListener('mouseout', (e) => {
  const el = e.target.closest && e.target.closest('[data-tip]');
  if (el) {
    const tip = $('#htip');
    if (tip) tip.style.display = 'none';
  }
});

// Single delegated click handler:
//   1. Dropdown button → toggle its menu (close any others).
//   2. Dropdown item → select, update label, dispatch by data-drop id.
//   3. Elsewhere → close any open menus.
//   4. Heatmap day / pie slice → apply filter.
document.addEventListener('click', (e) => {
  if (!e.target.closest) return;

  // Session message viewer: a sticky header row above the messages has
  // three click-to-sort cells (date/who/tokens). Click toggles direction
  // on the same column; otherwise switches column to its default dir.
  const mh = e.target.closest('[data-msg-sort]');
  if (mh) {
    const col = mh.dataset.msgSort;
    const ss = sortState.messages;
    if (ss.col === col) ss.desc = !ss.desc;
    else {
      ss.col = col;
      ss.desc = col !== 'date';
    }
    render();
    e.stopPropagation();
    return;
  }

  // Sortable column header: toggle direction if same column, else select
  // it with the column's default direction. Stops here so the row's own
  // click handler doesn't fire.
  const th = e.target.closest('th.sortable');
  if (th) {
    const v = th.dataset.view,
      col = th.dataset.sort;
    const ss = sortState[v];
    if (ss) {
      if (ss.col === col) ss.desc = !ss.desc;
      else {
        ss.col = col;
        const def = (UCOLS.find((c) => c.key === col) || {}).def;
        ss.desc = def !== 'asc';
      }
      render();
    }
    e.stopPropagation();
    return;
  }

  // "More" expander chips at the bottom of paginated dashboard tables.
  // Two flavors: data-expand bumps the cap by 12, data-expand-all
  // jumps straight to Infinity (show everything).
  const moreAll = e.target.closest('[data-expand-all]');
  if (moreAll) {
    const v = moreAll.dataset.expandAll;
    if (v && dashLimit[v] != null) {
      dashLimit[v] = Infinity;
      render();
    }
    e.stopPropagation();
    return;
  }
  const collapseChip = e.target.closest('[data-collapse]');
  if (collapseChip) {
    const v = collapseChip.dataset.collapse;
    if (v && DASH_LIMIT_DEFAULTS[v] != null) {
      dashLimit[v] = DASH_LIMIT_DEFAULTS[v];
      render();
    }
    e.stopPropagation();
    return;
  }
  const moreOne = e.target.closest('[data-expand]');
  if (moreOne) {
    const v = moreOne.dataset.expand;
    if (v && dashLimit[v] != null) {
      dashLimit[v] += 12;
      render();
    }
    e.stopPropagation();
    return;
  }

  const btn = e.target.closest('.drop-btn');
  if (btn) {
    const menu = btn.nextElementSibling;
    document.querySelectorAll('.drop-menu').forEach((m) => {
      if (m !== menu) m.classList.add('hidden');
    });
    menu.classList.toggle('hidden');
    e.stopPropagation();
    return;
  }

  const item = e.target.closest('.drop-item');
  if (item) {
    const drop = item.closest('.drop');
    const v = item.dataset.v;
    const kind = drop.dataset.drop;
    if (kind === 'model') {
      // Multi-select: keep the menu open so several models can be picked
      // without re-opening between clicks.
      e.stopPropagation();
      toggleModelFilter(v);
    } else {
      item.parentNode.classList.add('hidden');
      if (kind === 'detail') {
        detail = v;
        render();
      }
    }
    return;
  }

  // Click outside any open menu — close them all.
  if (!e.target.closest('.drop')) {
    document.querySelectorAll('.drop-menu').forEach((m) => m.classList.add('hidden'));
  }

  const d = e.target.closest('[data-day]');
  if (d) {
    setSingleDay(d.getAttribute('data-day'));
    return;
  }
  const m = e.target.closest('[data-model]');
  if (m) {
    setModelFilterOnly(m.getAttribute('data-model'));
    return;
  }
});

render();
