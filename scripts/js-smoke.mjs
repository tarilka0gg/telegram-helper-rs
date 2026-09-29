// Runs the dashboard scripts against a tiny fake DOM with hostile API data and checks that nothing
// unescaped reaches innerHTML. Usage: node scripts/js-smoke.mjs
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';

const WEB = new URL('../crates/server/web/', import.meta.url);
const load = f => readFileSync(new URL(f, WEB), 'utf8');

class El {
  constructor(id) { this.id = id; this.innerHTML = ''; this.textContent = ''; this.value = ''; this.dataset = {}; this.className = ''; this.listeners = {}; this.kids = {}; }
  get classList() { return { toggle: () => {}, add: () => {}, remove: () => {} }; }
  addEventListener(t, f) { (this.listeners[t] ||= []).push(f); }
  querySelector(sel) { return (this.kids[sel] ||= new El(this.id + ' ' + sel)); }
  fire(t, ev) { for (const f of this.listeners[t] || []) f(ev); }
}
function makeDom() {
  const els = {};
  const document = {
    getElementById: id => (els[id] ||= new El(id)),
    addEventListener: (t, f) => (document._l ||= {})[t] = f,
    createElement: () => new El('new'),
  };
  return { els, document };
}
const flush = () => new Promise(r => setTimeout(r, 5));
const XSS = `<img src=x onerror=alert(1)>"'><script>alert(2)</script>&`;
// Only the attacker's own markup counts as a leak (our templates legitimately contain <img> for avatars).
const RAW = /<img src=x|<script|<iframe|<svg|onerror=alert\(1\)>/i;
function assertNoRaw(els, where) {
  const walk = e => { assert.ok(!RAW.test(e.innerHTML), `${where}: unescaped HTML in #${e.id}: ${e.innerHTML.slice(0, 200)}`); Object.values(e.kids).forEach(walk); };
  Object.values(els).forEach(walk);
}
const json = body => ({ ok: true, json: async () => body });

// ---------------- chats.js ----------------
{
  const { els, document } = makeDom();
  const contacts = [
    { peer_id: 1, name: XSS, username: XSS, kind: 'channel', category: XSS, is_news_source: true, mirror: true, is_archived: false, is_bot: false, messages: 3 },
    { peer_id: 2, name: 'Оля', username: null, kind: 'user', category: 'friends', is_news_source: false, mirror: true, is_archived: false, is_bot: false, messages: 1 },
    { peer_id: 3, name: 'Bot', username: 'b', kind: 'user', category: null, is_news_source: false, mirror: true, is_archived: false, is_bot: true, messages: 0 },
    { peer_id: 4, name: 'Група', username: null, kind: 'supergroup', category: 'groups', is_news_source: false, mirror: false, is_archived: false, is_bot: false, messages: 9 },
  ];
  let fail = false; const posts = [];
  const fetch = async (url, opt) => {
    if (opt?.method === 'POST') { posts.push({ url, opt }); return { ok: !fail }; }
    return json(contacts);
  };
  new Function('document', 'fetch', 'setTimeout', load('chats.js'))(document, fetch, (f) => f());
  await flush();
  assertNoRaw(els, 'chats initial');
  assert.match(els.count.textContent, /3 of 3/, 'bots must be hidden');
  assert.ok(els.list.innerHTML.includes('&lt;img'), 'hostile name must be escaped, not dropped');
  assert.ok(!els.list.innerHTML.includes('Bot</span>'), 'bot row hidden');
  assert.ok(/supergroup|groups/.test(els.tabs.innerHTML) && els.tabs.innerHTML.includes('groups'), 'category tabs present');
  assert.ok(els.tabs.innerHTML.includes('&lt;img'), 'category tab label escaped');

  // toggle mirror on chat 2 -> POST with the CSRF header and correct body
  els.list.fire('change', { target: { dataset: { field: 'mirror', id: '2' }, checked: false } });
  await flush();
  assert.equal(posts.length, 1);
  assert.equal(posts[0].url, '/api/contacts/2');
  assert.equal(posts[0].opt.headers['X-Requested-With'], 'tgh');
  assert.deepEqual(JSON.parse(posts[0].opt.body), { mirror: false });
  // failed save reverts and reports
  fail = true;
  els.list.fire('change', { target: { dataset: { field: 'news_source', id: '1' }, checked: false } });
  await flush();
  assert.ok(/save failed/.test(els.count.textContent) || /shown/.test(els.count.textContent));
  assertNoRaw(els, 'chats after toggles');

  // search + tab switching never throw
  els.q.fire('input', { target: { value: '<script>' } });
  els.tabs.fire('click', { target: { closest: () => ({ dataset: { tab: 'news' } }) } });
  els.tabs.fire('click', { target: { closest: () => null } });
  assertNoRaw(els, 'chats after search');
  console.log('chats.js ok');
}

// ---------------- chats.js with a dead API ----------------
{
  const { els, document } = makeDom();
  new Function('document', 'fetch', 'setTimeout', load('chats.js'))(document, async () => { throw new Error('down'); }, (f) => f());
  await flush();
  assert.match(els.count.textContent, /cannot load/);
  console.log('chats.js offline ok');
}

// ---------------- app.js ----------------
{
  const { els, document } = makeDom();
  const routes = {
    '/api/overview': { messages_total: 5, userbot_connected: true },
    '/api/messages/daily': [{ day: XSS, incoming: 3, outgoing: 1 }],
    '/api/messages/hourly': [{ hour: 3, count: 2 }],
    '/api/messages/top_chats': [{ peer_id: 1, name: XSS, count: 4 }],
    '/api/llm/daily': [{ day: '2026-01-01', calls: 1, prompt_tokens: 10, completion_tokens: 5, errors: 0, avg_latency_ms: 12.5 }],
    '/api/llm/by_purpose': [{ purpose: XSS, calls: 1, tokens: 15, avg_latency_ms: 12.5 }],
    '/api/autoreply/recent': [{ created_at: '2026-01-01 10:00:00', peer_name: XSS, incoming_text: XSS, reply_text: XSS }],
    '/api/commitments': [{ id: 1, peer_name: XSS, direction: 'mine', text: XSS, deadline_at: null, status: 'open' }],
    '/api/events': [{ ts: '2026-01-01 10:00:00', kind: XSS, peer_id: null, detail: XSS }],
  };
  const fetch = async url => json(routes[url.split('?')[0]] ?? {});
  els.days = new El('days'); els.days.value = '30';
  new Function('document', 'fetch', 'setInterval', 'setTimeout', load('app.js'))(document, fetch, () => 0, (f) => f());
  document._l.DOMContentLoaded();
  await flush();
  assertNoRaw(els, 'app.js hostile data');
  assert.equal(els['stat-messages'].textContent, '5');
  console.log('app.js ok');

  // every endpoint failing must not throw or leave "undefined"/"NaN" in the KPIs
  const dom2 = makeDom();
  dom2.els.days = new El('days'); dom2.els.days.value = '7';
  new Function('document', 'fetch', 'setInterval', 'setTimeout', load('app.js'))(dom2.document, async () => ({ ok: false, json: async () => { throw new Error('x'); } }), () => 0, (f) => f());
  dom2.document._l.DOMContentLoaded();
  await flush();
  for (const id of ['stat-messages', 'stat-contacts', 'stat-tokens24']) assert.ok(!/undefined|NaN/.test(dom2.els[id].textContent), `${id}: ${dom2.els[id].textContent}`);
  console.log('app.js all-endpoints-down ok');
}
console.log('ALL JS SMOKE TESTS PASSED');
