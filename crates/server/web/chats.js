(async function () {
  const $ = id => document.getElementById(id);
  const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const ICON = { channel: '📢', chat: '👥', supergroup: '👥', user: '👤' };
  let contacts = [], tab = 'all', search = '', flash = null;

  try { contacts = (await (await fetch('/api/contacts')).json()).filter(c => !c.is_bot); } catch (_) { $('count').textContent = 'cannot load contacts'; }

  const TABS = [
    ['all', 'all', () => true], ['channels', 'channels', c => c.kind === 'channel'], ['groups', 'groups', c => c.kind === 'chat' || c.kind === 'supergroup'],
    ['people', 'people', c => c.kind === 'user'], ['news', '📰 news sources', c => c.is_news_source], ['off', '🚫 not mirrored', c => !c.mirror],
  ];
  const cats = [...new Set(contacts.map(c => c.category).filter(Boolean))].sort();
  cats.forEach(k => TABS.push(['cat:' + k, k, c => c.category === k]));
  const pred = () => (TABS.find(t => t[0] === tab) || TABS[0])[2];

  function renderTabs() {
    $('tabs').innerHTML = TABS.map(([id, label, f]) =>
      `<button type="button" data-tab="${esc(id)}" class="${id === tab ? 'on' : ''}">${esc(label)} <b>${contacts.filter(f).length}</b></button>`).join('');
  }

  function render() {
    const s = search.trim().toLowerCase(), f = pred();
    const rows = contacts.filter(f).filter(c => !s || c.name.toLowerCase().includes(s) || (c.username || '').toLowerCase().includes(s));
    $('count').textContent = flash || `${rows.length} of ${contacts.length} shown`;
    $('list').innerHTML = rows.slice(0, 500).map(c => {
      const id = Number(c.peer_id);
      const news = c.kind !== 'user'
        ? `<label class="chk"><input type="checkbox" data-id="${id}" data-field="news_source" ${c.is_news_source ? 'checked' : ''}> 📰 news</label>`
        : '<span class="chk off">—</span>';
      return `<div class="row${c.is_archived ? ' arch' : ''}">
        <span class="ava"><img src="/avatars/${id}" loading="lazy" alt="" data-kind="${esc(c.kind)}"></span>
        <span class="name">${esc(c.name)}${c.username ? ` <small>@${esc(c.username)}</small>` : ''}</span>
        <span class="cat">${esc(c.category || '')}</span><span class="msgs">${Number(c.messages)} msgs</span>
        ${news}<label class="chk"><input type="checkbox" data-id="${id}" data-field="mirror" ${c.mirror ? 'checked' : ''}> 🪞 mirror</label></div>`;
    }).join('');
  }

  // Broken/missing avatar -> emoji by kind (error events do not bubble, so capture on the container).
  $('list').addEventListener('error', e => {
    const img = e.target;
    if (img.tagName !== 'IMG') return;
    img.replaceWith(Object.assign(document.createElement('span'), { textContent: ICON[img.dataset.kind] || '👤' }));
  }, true);

  $('tabs').addEventListener('click', e => {
    const b = e.target.closest('button[data-tab]');
    if (b) { tab = b.dataset.tab; renderTabs(); render(); }
  });
  $('q').addEventListener('input', e => { search = e.target.value; render(); });

  $('list').addEventListener('change', async e => {
    const field = e.target.dataset.field;
    if (!field) return;
    const c = contacts.find(x => x.peer_id === Number(e.target.dataset.id));
    if (!c) return;
    const key = field === 'news_source' ? 'is_news_source' : 'mirror', val = e.target.checked;
    c[key] = val;
    let ok = false;
    try {
      ok = (await fetch('/api/contacts/' + c.peer_id, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Requested-With': 'tgh' }, body: JSON.stringify({ [field]: val }) })).ok;
    } catch (_) {}
    if (!ok) { c[key] = !val; flash = 'save failed'; setTimeout(() => { flash = null; render(); }, 3000); }
    renderTabs(); render();
  });

  renderTabs(); render();
})();
