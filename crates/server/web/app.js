document.addEventListener('DOMContentLoaded', () => {
  const $ = id => document.getElementById(id);
  const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const fmt = n => (Number(n) || 0).toLocaleString();
  const cut = (s, n = 80) => { s = String(s ?? ''); return s.length > n ? s.slice(0, n) + '…' : s; };
  const get = (url, fallback) => fetch(url).then(r => (r.ok ? r.json() : fallback)).catch(() => fallback);

  // series: [{label, values:[v0, v1, ...]}]; keys: names of stacked segments (class bar-in / bar-out)
  function bars(svg, rows, { xKey, keys, names }) {
    if (!rows.length) { svg.innerHTML = '<text x="300" y="100" text-anchor="middle" class="axis-label">no data</text>'; return; }
    const W = 600, H = 200, L = 34, B = 20, T = 8;
    const totals = rows.map(r => keys.reduce((s, k) => s + (+r[k] || 0), 0));
    const max = Math.max(1, ...totals);
    const step = (W - L) / rows.length, bw = Math.max(1, step * 0.75);
    const scale = v => (v / max) * (H - B - T);
    let out = `<text x="2" y="${T + 8}" class="axis-label">${fmt(max)}</text>`;
    rows.forEach((r, i) => {
      const x = L + i * step + (step - bw) / 2;
      let y = H - B;
      const tip = `${r[xKey]}: ` + keys.map((k, j) => `${names[j]} ${fmt(r[k])}`).join(', ');
      keys.forEach((k, j) => {
        const h = scale(+r[k] || 0);
        y -= h;
        out += `<rect x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${bw.toFixed(1)}" height="${h.toFixed(1)}" class="${j ? 'bar-out' : 'bar-in'}"><title>${esc(tip)}</title></rect>`;
      });
    });
    out += `<text x="${L}" y="${H - 4}" class="axis-label">${esc(rows[0][xKey])}</text>`;
    out += `<text x="${W}" y="${H - 4}" text-anchor="end" class="axis-label">${esc(rows[rows.length - 1][xKey])}</text>`;
    svg.innerHTML = out;
  }

  const fill = (id, rows, cells) => {
    $(id).querySelector('tbody').innerHTML = rows.length
      ? rows.map(r => '<tr>' + cells(r).map(c => `<td>${esc(c)}</td>`).join('') + '</tr>').join('')
      : '<tr><td colspan="9" class="muted">—</td></tr>';
  };

  async function load() {
    const d = +$('days').value || 30;
    const [ov, daily, hourly, top, llmD, llmP, auto, commits, events] = await Promise.all([
      get('/api/overview', {}), get(`/api/messages/daily?days=${d}`, []), get(`/api/messages/hourly?days=${d}`, []),
      get(`/api/messages/top_chats?days=${d}&limit=10`, []), get(`/api/llm/daily?days=${d}`, []),
      get(`/api/llm/by_purpose?days=${d}`, []), get('/api/autoreply/recent?limit=50', []),
      get('/api/commitments?status=open', []), get('/api/events?limit=100', []),
    ]);
    const kpi = { 'stat-messages': 'messages_total', 'stat-messages24': 'messages_24h', 'stat-contacts': 'contacts',
      'stat-commitments': 'open_commitments', 'stat-autoreplies24': 'autoreplies_24h', 'stat-llmcalls24': 'llm_calls_24h',
      'stat-tokens24': 'llm_tokens_24h', 'stat-errors24': 'llm_errors_24h' };
    for (const [id, k] of Object.entries(kpi)) $(id).textContent = fmt(ov[k]);
    const c = $('conn');
    c.textContent = ov.userbot_connected ? 'userbot online' : 'userbot offline';
    c.classList.toggle('ok', !!ov.userbot_connected);
    c.classList.toggle('bad', !ov.userbot_connected);

    bars($('chart-daily'), daily, { xKey: 'day', keys: ['incoming', 'outgoing'], names: ['incoming', 'outgoing'] });
    const byHour = Array.from({ length: 24 }, (_, h) => ({ hour: String(h).padStart(2, '0') + ':00', count: (hourly.find(x => x.hour === h) || {}).count || 0 }));
    bars($('chart-hourly'), byHour, { xKey: 'hour', keys: ['count'], names: ['messages'] });
    bars($('chart-llm'), llmD, { xKey: 'day', keys: ['prompt_tokens', 'completion_tokens'], names: ['prompt', 'completion'] });

    fill('tbl-top', top, r => [r.name || r.peer_id, fmt(r.count)]);
    fill('tbl-purpose', llmP, r => [r.purpose, fmt(r.calls), fmt(r.tokens), fmt(Math.round(r.avg_latency_ms)) + ' ms']);
    fill('tbl-autoreply', auto, r => [cut(r.created_at, 16), cut(r.peer_name, 30), cut(r.incoming_text), cut(r.reply_text)]);
    fill('tbl-commitments', commits, r => [r.peer_name, r.direction, cut(r.text), cut(r.deadline_at || '', 16)]);
    fill('tbl-events', events, r => [cut(r.ts, 16), r.kind, r.peer_id ?? '', cut(r.detail)]);
  }

  $('days').addEventListener('change', load);
  load();
  setInterval(load, 30000);
});
