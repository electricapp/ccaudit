// Tiny test harness — 20 lines of describe/it/eq, no framework needed.
// Collects results into the #out element on the host page and
// summarises pass/fail at the bottom.
(function () {
  const out = document.getElementById('out');
  const summary = document.getElementById('summary');
  let pass = 0,
    fail = 0;
  function suite(name, fn) {
    const h = document.createElement('div');
    h.className = 'suite';
    h.textContent = name;
    out.appendChild(h);
    fn();
  }
  function it(name, fn) {
    const line = document.createElement('div');
    try {
      fn();
      line.textContent = '  ok · ' + name;
      line.className = 'ok';
      pass++;
    } catch (e) {
      line.textContent = '  FAIL · ' + name + '  — ' + e.message;
      line.className = 'fail';
      fail++;
    }
    out.appendChild(line);
  }
  function eq(actual, expected, label) {
    const a = JSON.stringify(actual),
      e = JSON.stringify(expected);
    if (a !== e) {
      throw new Error((label ? label + ': ' : '') + 'expected ' + e + ', got ' + a);
    }
  }

  // ── slugify ──
  suite('slugify', () => {
    it('empty input → empty string', () => eq(slugify(''), ''));
    it('null / undefined → empty string', () => {
      eq(slugify(null), '');
      eq(slugify(undefined), '');
    });
    it('lowercases + swaps slashes for dashes', () =>
      eq(slugify('phonon/subscription/server'), 'phonon-subscription-server'));
    it('collapses runs of non-alphanum', () => eq(slugify('foo   /  bar'), 'foo-bar'));
    it('trims leading and trailing dashes', () =>
      eq(slugify('-home-user-code-ccaudit'), 'home-user-code-ccaudit'));
    it('keeps alphanumerics, drops punctuation', () =>
      eq(slugify('my project: v2.1!'), 'my-project-v2-1'));
    it('stable: slugifying the slug is idempotent', () => {
      const s = slugify('phonon/crates/power/monitor');
      eq(slugify(s), s);
    });
  });

  // ── ft (thousands separator) ──
  suite('ft — thousand separators', () => {
    it('small numbers are unchanged', () => eq(ft(0), '0'));
    it('3-digit numbers are unchanged', () => eq(ft(123), '123'));
    it('4-digit inserts one separator', () => eq(ft(1234), '1_234'));
    it('7-digit inserts two separators', () => eq(ft(1234567), '1_234_567'));
    it('10-digit (billions) inserts three separators', () => eq(ft(12884508231), '12_884_508_231'));
  });

  // ── fc (currency) ──
  suite('fc — cost formatter', () => {
    it('tiny costs collapse to <$0.01', () => eq(fc(0), '<$0.01'));
    it('just under threshold still collapses', () => eq(fc(0.009), '<$0.01'));
    it('cents render with two digits', () => eq(fc(0.05), '$0.05'));
    it('round dollars keep the .00', () => eq(fc(42), '$42.00'));
    it('thousands get the underscore separator', () => eq(fc(1234.5), '$1_234.50'));
    it('millions get two separators', () => eq(fc(1234567.89), '$1_234_567.89'));
  });

  // ── fdur (duration) ──
  suite('fdur — human duration', () => {
    it('zero or negative → empty', () => {
      eq(fdur(0), '');
      eq(fdur(-1), '');
    });
    it('non-finite → empty', () => eq(fdur(NaN), ''));
    it('seconds only', () => eq(fdur(42000), '42s'));
    it('minutes + seconds', () => eq(fdur(5 * 60000 + 12000), '5m 12s'));
    it('hours + minutes', () => eq(fdur(2 * 3600000 + 10 * 60000), '2h 10m'));
    it('days + hours', () => eq(fdur(3 * 86400000 + 4 * 3600000), '3d 4h'));
  });

  // ── esc (HTML escape) ──
  suite('esc — html escape', () => {
    it('empty → empty', () => eq(esc(''), ''));
    it('plain text passes through', () => eq(esc('hello world'), 'hello world'));
    it('escapes <, >, &', () => eq(esc('<script>&"'), '&lt;script&gt;&amp;"'));
    it("quotes are NOT escaped (attribute escape is the caller's job)", () =>
      eq(esc('"foo"'), '"foo"'));
  });

  // ── durMs ──
  suite('durMs — session duration', () => {
    it('missing session → 0', () => eq(durMs(null), 0));
    it('missing start → 0', () => eq(durMs({ ended_at: '2026-01-01T00:00:00Z' }), 0));
    it('missing end → 0', () => eq(durMs({ started_at: '2026-01-01T00:00:00Z' }), 0));
    it('end before start → 0', () =>
      eq(durMs({ started_at: '2026-01-01T01:00:00Z', ended_at: '2026-01-01T00:00:00Z' }), 0));
    it('one-hour session', () =>
      eq(durMs({ started_at: '2026-01-01T00:00:00Z', ended_at: '2026-01-01T01:00:00Z' }), 3600000));
  });

  // ── dn (display name priority) ──
  suite('dn — display name priority', () => {
    it('prefers summary', () => eq(dn({ summary: 'S', first_user_msg: 'F', id: 'I' }), 'S'));
    it('falls back to first_user_msg', () => eq(dn({ first_user_msg: 'F', id: 'I' }), 'F'));
    it('falls back to id', () => eq(dn({ id: 'I' }), 'I'));
    it('empty summary treated as missing', () =>
      eq(dn({ summary: '', first_user_msg: 'F', id: 'I' }), 'F'));
  });

  // ── dayStr ──
  suite('dayStr — YYYY-MM-DD slice', () => {
    it('null → null', () => eq(dayStr(null), null));
    it('empty → null', () => eq(dayStr(''), null));
    it('too short → null', () => eq(dayStr('2026'), null));
    it('slices full ISO', () => eq(dayStr('2026-04-21T14:30:00Z'), '2026-04-21'));
    it('slices date-only', () => eq(dayStr('2026-04-21'), '2026-04-21'));
  });

  // ── tokenParts ──
  suite('tokenParts', () => {
    it('missing fields default to 0', () => {
      eq(tokenParts({}), { input: 0, output: 0, cache_read: 0, cache_create: 0, sum: 0 });
    });
    it('sums all four token types', () => {
      const s = {
        total_input_tokens: 100,
        total_output_tokens: 200,
        total_cache_read: 300,
        total_cache_create: 400,
      };
      eq(tokenParts(s), {
        input: 100,
        output: 200,
        cache_read: 300,
        cache_create: 400,
        sum: 1000,
      });
    });
  });

  // ── costParts ──
  suite('costParts', () => {
    it('missing fields default to 0', () => {
      eq(costParts({}), { input: 0, output: 0, cache_read: 0, cache_create: 0 });
    });
    it('reads all four cost types', () => {
      const s = {
        cost_input: 1,
        cost_output: 2,
        cost_cache_read: 3,
        cost_cache_create: 4,
      };
      eq(costParts(s), { input: 1, output: 2, cache_read: 3, cache_create: 4 });
    });
  });

  // ── tokTip / costTip format ──
  suite('tokTip', () => {
    it('empty breakdown → empty string', () => eq(tokTip(null), ''));
    it('formats breakdown with pipe separators', () => {
      const b = { input: 100, output: 200, cache_read: 1000, cache_create: 10000 };
      eq(tokTip(b), 'Tokens|input: 100|output: 200|cache-read: 1_000|cache-write: 10_000');
    });
  });

  suite('costTip', () => {
    it('per-type breakdown when costBreakdown supplied', () => {
      const cb = { input: 1, output: 2, cache_read: 3, cache_create: 4 };
      eq(
        costTip(0, 0, 0, 0, cb),
        'Cost|input: $1.00|output: $2.00|cache-read: $3.00|cache-write: $4.00'
      );
    });
    it('falls back to averages without breakdown', () => {
      eq(
        costTip(10, 5, 100, 1_000_000),
        'Cost|per session: $2.00|per message: $0.10|per 1M tok: $10.00'
      );
    });
    it('skips zero-denominator lines', () => {
      eq(costTip(10, 0, 0, 0), 'Cost');
    });
  });

  // ── hl (search highlight) ──
  suite('hl — search highlight', () => {
    it('no query returns escaped text', () => eq(hl('<b>', ''), '&lt;b&gt;'));
    it('wraps first hit in span', () =>
      eq(hl('hello world', 'world'), 'hello <span class="hl">world</span>'));
    it('wraps multiple hits', () =>
      eq(hl('foo foo', 'foo'), '<span class="hl">foo</span> <span class="hl">foo</span>'));
    it('escapes around hits', () =>
      eq(hl('<x>foo</x>', 'foo'), '&lt;x&gt;<span class="hl">foo</span>&lt;/x&gt;'));
  });

  // ── Render summary ──
  const total = pass + fail;
  summary.textContent = `${pass}/${total} passed` + (fail ? ` · ${fail} FAILED` : '');
  summary.className = fail ? 'fail' : 'pass';
})();
