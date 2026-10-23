// Kuery landing page: the live SQL editor in the hero, the sources board,
// the pinned workflow, the keymap and platform switches, and the entrances.
(() => {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
  const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

  // Example queries shown in the hero editor. Results are synthetic sample data.
  const QUERIES = [
    {
      engine: 'Polars SQL · read only', where: 'orders.csv + regional_targets.json',
      status: '18 result rows · 36 ms · full file inputs', tables: ['orders', 'regional_targets'],
      sql: "SELECT o.month, o.region, COUNT(*) AS orders,\n  ROUND(SUM(o.revenue), 2) AS revenue, t.monthly_target\nFROM orders o\nJOIN regional_targets t ON o.region = t.region\nWHERE o.status = 'completed'\nGROUP BY o.month, o.region, t.monthly_target\nORDER BY o.month DESC, revenue DESC;",
      cols: '1fr 1.4fr 0.8fr 1fr 1.2fr',
      heads: [['month', ''], ['region', ''], ['orders', 'n'], ['revenue', 'n'], ['monthly_target', 'n']],
      rows: [
        ['2026-06', 'Latin America', '34', '13778.16', '15000'],
        ['2026-06', 'Europe', '35', '12041.9', '13500'],
        ['2026-06', 'North America', '34', '10517', '12000'],
        ['2026-05', 'Latin America', '35', '13872', '15000'],
        ['2026-05', 'Europe', '34', '12360.5', '13500'],
        ['2026-05', 'North America', '34', '9469', '12000'],
        ['2026-04', 'Latin America', '34', '12720.08', '15000'],
      ],
    },
    {
      engine: 'PostgreSQL 16.15 · Read', where: 'local-dev',
      status: '5 rows · 60.6 ms · first rows in 17 ms', tables: ['orders', 'customers'],
      sql: "SELECT c.country, COUNT(*) AS refunds,\n  SUM(o.total) AS refunded\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded'\nGROUP BY c.country\nORDER BY refunded DESC;",
      cols: '1.6fr 1fr 1.2fr',
      heads: [['country', ''], ['refunds', 'n'], ['refunded', 'n']],
      rows: [
        ['Spain', '412', '38120.55'],
        ['United States', '388', '36904.10'],
        ['Mexico', '251', '21877.40'],
        ['Germany', '197', '18342.95'],
        ['Brazil', '164', '14410.00'],
      ],
    },
    {
      engine: 'Polars SQL · read only', where: 'events.parquet',
      status: '4 result rows · 22 ms · full file inputs', tables: ['events'],
      sql: "SELECT device, COUNT(*) AS sessions,\n  ROUND(AVG(duration_s), 1) AS avg_s\nFROM events\nWHERE day >= '2026-10-01'\nGROUP BY device\nORDER BY sessions DESC;",
      cols: '1.6fr 1fr 1fr',
      heads: [['device', ''], ['sessions', 'n'], ['avg_s', 'n']],
      rows: [
        ['desktop', '48213', '312.4'],
        ['mobile', '39870', '141.9'],
        ['tablet', '6412', '228.7'],
        ['tv', '318', '905.2'],
      ],
    },
  ];

  const KEYWORDS = new Set(['SELECT', 'FROM', 'JOIN', 'ON', 'WHERE', 'GROUP', 'BY', 'ORDER', 'AS', 'DESC', 'ASC', 'AND', 'COUNT', 'SUM', 'ROUND', 'AVG']);

  function tokenize(sql, tables) {
    const out = [];
    for (const [s] of sql.matchAll(/'[^']*'|\d+(?:\.\d+)?|[A-Za-z_]\w*|\s+|./g)) {
      let k = 'id';
      if (s[0] === "'") k = 'st';
      else if (/^\d/.test(s)) k = 'nu';
      else if (KEYWORDS.has(s)) k = 'kw';
      else if (tables.includes(s)) k = 'tb';
      out.push({ s, k });
    }
    return out;
  }

  // --- hero editor
  const frag = $('.frag');
  if (frag) {
    const tabs = $$('.f-tab', frag);
    const code = $('[data-code]', frag);
    const gutter = $('[data-gutter]', frag);
    const head = $('[data-head]', frag);
    const out = $('[data-out]', frag);
    const status = $('[data-status]', frag);
    const engine = $('[data-engine]', frag);
    const cancel = $('[data-cancel]', frag);
    const toks = QUERIES.map((q) => tokenize(q.sql, q.tables));
    let src = 0, typed = 0, typing = 0, running = 0;

    const paintCode = () => {
      let left = typed, html = '';
      for (const t of toks[src]) {
        if (left <= 0) break;
        const s = t.s.length <= left ? t.s : t.s.slice(0, left);
        left -= s.length;
        html += `<span class="${t.k}">${esc(s)}</span>`;
      }
      code.innerHTML = html + '<span class="caret"></span>';
    };

    const setPhase = (phase) => {
      const q = QUERIES[src];
      cancel.disabled = phase !== 'running';
      if (phase === 'typing') {
        status.textContent = 'No results yet';
        out.innerHTML = '<div class="f-wait">Typing the statement. Press Run to execute it now.</div>';
      } else if (phase === 'running') {
        status.textContent = 'Running…';
        out.innerHTML = `<div class="f-wait"><span class="spin"></span>Running on ${esc(q.where)}…</div>`;
      } else {
        status.textContent = q.status;
        out.innerHTML = '<div class="f-body">' + q.rows.map((r, i) =>
          `<div class="f-row" style="grid-template-columns: ${q.cols}; animation-delay: ${i * 55}ms">` +
          r.map((v, j) => `<samp class="${q.heads[j][1]}">${esc(v)}</samp>`).join('') + '</div>').join('') + '</div>';
      }
    };

    const run = () => {
      clearInterval(typing); clearTimeout(running);
      typed = QUERIES[src].sql.length; paintCode();
      setPhase('running');
      running = setTimeout(() => setPhase('done'), reduced ? 0 : 700);
    };

    const select = (i) => {
      clearInterval(typing); clearTimeout(running);
      src = i;
      const q = QUERIES[i];
      tabs.forEach((t, j) => { t.classList.toggle('on', j === i); t.setAttribute('aria-selected', String(j === i)); });
      engine.textContent = q.engine;
      gutter.innerHTML = q.sql.split('\n').map((_, n) => `<span>${n + 1}</span>`).join('');
      head.style.gridTemplateColumns = q.cols;
      head.innerHTML = q.heads.map(([v, c]) => `<span class="${c}">${esc(v)}</span>`).join('');
      if (reduced) { run(); return; }
      typed = 0; paintCode(); setPhase('typing');
      typing = setInterval(() => {
        typed = Math.min(q.sql.length, typed + 3);
        paintCode();
        if (typed >= q.sql.length) run();
      }, 26);
    };

    tabs.forEach((t, i) => t.addEventListener('click', () => select(i)));
    $('[data-run]', frag).addEventListener('click', run);
    cancel.addEventListener('click', () => { clearTimeout(running); setPhase('typing'); });
    $('[data-format]', frag).addEventListener('click', () => { clearInterval(typing); typed = QUERIES[src].sql.length; paintCode(); });
    select(0);
  }

  // --- sources board: one row open at a time
  const rows = $$('.brow');
  rows.forEach((row) => row.addEventListener('click', () => {
    rows.forEach((r) => { r.classList.toggle('open', r === row); r.setAttribute('aria-expanded', String(r === row)); });
  }));

  // --- workflow: the statement nearest the middle of the screen picks the pane
  const steps = $$('[data-step]');
  const panes = $$('[data-pane]');
  if (steps.length) {
    let current = 0, ticking = false;
    const update = () => {
      ticking = false;
      const mid = innerHeight / 2;
      let best = 0, bestD = Infinity;
      steps.forEach((el, i) => {
        const r = el.getBoundingClientRect();
        const d = Math.abs(r.top + r.height / 2 - mid);
        if (d < bestD) { bestD = d; best = i; }
      });
      if (best === current) return;
      current = best;
      steps.forEach((el, i) => el.classList.toggle('on', i === best));
      panes.forEach((el, i) => el.classList.toggle('on', i === best));
    };
    addEventListener('scroll', () => { if (!ticking) { ticking = true; requestAnimationFrame(update); } }, { passive: true });
    update();
  }

  // --- keymap: macOS or Windows and Linux
  const keyButtons = $$('[data-keys]');
  keyButtons.forEach((b) => b.addEventListener('click', () => {
    const mac = b.dataset.keys === 'mac';
    keyButtons.forEach((x) => { x.classList.toggle('on', x === b); x.setAttribute('aria-pressed', String(x === b)); });
    $$('kbd[data-mac]').forEach((k) => { k.textContent = mac ? k.dataset.mac : k.dataset.pc; });
  }));

  // --- downloads: the visitor's platform first
  const platButtons = $$('[data-plat]');
  const panels = $$('[data-panel]');
  const showPlat = (i) => {
    platButtons.forEach((b, j) => { b.classList.toggle('on', j === i); b.setAttribute('aria-pressed', String(j === i)); });
    panels.forEach((p, j) => { p.hidden = j !== i; });
  };
  platButtons.forEach((b, i) => b.addEventListener('click', () => showPlat(i)));

  const ua = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || navigator.userAgent;
  let os = 'macOS', plat = 0;
  if (/win/i.test(ua)) { os = 'Windows'; plat = 1; }
  else if (/linux|x11/i.test(ua) && !/android/i.test(navigator.userAgent)) { os = 'Linux'; plat = 2; }
  $$('[data-os-name]').forEach((el) => { el.textContent = os; });
  if (plat !== 0) {
    showPlat(plat);
    $$('kbd[data-pc]').forEach((k) => { k.textContent = k.dataset.pc; });
    keyButtons.forEach((x) => { const on = x.dataset.keys === 'pc'; x.classList.toggle('on', on); x.setAttribute('aria-pressed', String(on)); });
  }

  const copy = $('[data-copy]');
  if (copy) {
    copy.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText($('[data-src-cmds]').textContent);
        copy.textContent = 'Copied to clipboard';
        setTimeout(() => { copy.textContent = 'Copy commands'; }, 2200);
      } catch {
        copy.textContent = 'Select the commands above to copy them';
      }
    });
  }

  // --- entrances: each element rises once as it comes into view
  if (!reduced && 'IntersectionObserver' in window) {
    document.documentElement.classList.add('rv-on');
    const io = new IntersectionObserver((entries) => {
      for (const en of entries) {
        if (en.isIntersecting) { en.target.classList.add('in'); io.unobserve(en.target); }
      }
    }, { threshold: 0.12 });
    $$('.rv').forEach((el) => io.observe(el));
  }
})();
