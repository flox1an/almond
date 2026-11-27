import { ExtensionSigner, NostrConnectSigner } from 'applesauce-signers';
import { RelayPool } from 'applesauce-relay';

NostrConnectSigner.pool = new RelayPool();
const $ = (id) => document.getElementById(id);
let signer, revision, collaboratorRevision, cursor = null, reportCursor = null, selectedReport = null;
const notice = (text) => { $('notice').textContent = text; };
const errorMessage = (e) => e instanceof Error ? e.message : String(e);
async function api(path, method = 'GET', body) {
  if (!signer) throw new Error('Connect a signer first.');
  const url = new URL(path, location.origin).href;
  const payload = body === undefined ? undefined : JSON.stringify(body);
  const tags = [['u', url], ['method', method]];
  if (payload !== undefined) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(payload));
    tags.push(['payload', Array.from(new Uint8Array(digest), (v) => v.toString(16).padStart(2, '0')).join('')]);
  }
  if (method !== 'GET') {
    const response = await fetch('/admin/api/challenge', { cache: 'no-store' });
    if (!response.ok) throw new Error('Could not obtain an authorization challenge.');
    tags.push(['challenge', (await response.json()).challenge]);
  }
  const event = await signer.signEvent({ kind: 27235, created_at: Math.floor(Date.now() / 1000), tags, content: '' });
  const headers = { Authorization: `Nostr ${btoa(JSON.stringify(event))}` };
  if (payload !== undefined) headers['Content-Type'] = 'application/json';
  const response = await fetch(url, { method, headers, body: payload, cache: 'no-store' });
  if (!response.ok) throw new Error(`${response.status} ${await response.text() || response.statusText}`);
  if (response.status === 204) return null;
  const type = response.headers.get('content-type') || '';
  return type.includes('json') ? response.json() : response.blob();
}
function show(id, value) {
  const target = $(id); target.replaceChildren();
  const pre = document.createElement('pre'); pre.textContent = JSON.stringify(value, null, 2); target.append(pre);
}
function button(text, action) {
  const b = document.createElement('button'); b.type = 'button'; b.className = 'secondary'; b.textContent = text;
  b.addEventListener('click', () => run(action)); return b;
}
async function run(fn) { try { notice(''); await fn(); } catch (e) { notice(errorMessage(e)); } }
function fields(id, values) {
  const target = $(id); target.replaceChildren(); const list = document.createElement('dl');
  for (const [name, value] of Object.entries(values)) {
    const term = document.createElement('dt'); term.textContent = name.replaceAll('_', ' ');
    const detail = document.createElement('dd'); detail.textContent = value == null ? 'Unavailable' : typeof value === 'object' ? JSON.stringify(value) : String(value); list.append(term, detail);
  }
  target.append(list);
}
function settings(data) {
  revision = data.revision; const target = $('settings-data'); target.replaceChildren(); const select = $('setting-name'); select.replaceChildren();
  for (const [name, field] of Object.entries(data.settings)) {
    const item = document.createElement('details'); item.className = 'item'; const title = document.createElement('summary'); title.textContent = `${name}${field.restart_required ? ' — restart required' : ''}`; item.append(title);
    const text = document.createElement('pre'); text.textContent = `Running (${field.running_source}): ${JSON.stringify(field.running)}\nSaved (${field.saved_source}): ${JSON.stringify(field.saved)}\nManaged: ${field.managed == null ? 'not set' : JSON.stringify(field.managed)}\n${field.editable && data.writable ? 'Editable' : `Read-only: ${field.reason || 'managed configuration unavailable'}`}`; item.append(text); target.append(item);
    if (data.writable && (field.editable || field.managed != null)) { const option = document.createElement('option'); option.value = name; option.textContent = name; option.dataset.editable = String(field.editable); select.append(option); }
  }
  $('setting-form').hidden = !select.options.length; updateSettingControl();
}
function updateSettingControl() {
  const locked = $('setting-name').selectedOptions[0]?.dataset.editable === 'false'; $('setting-reset').checked = locked; $('setting-reset').disabled = locked; $('setting-value').disabled = locked || $('setting-reset').checked;
}
$('setting-name').addEventListener('change', updateSettingControl);
$('setting-reset').addEventListener('change', () => { $('setting-value').disabled = $('setting-reset').checked; });
function withdrawal(value) {
  const target = $('withdrawal-data'); target.replaceChildren(); const { token, ...status } = value; const pre = document.createElement('pre'); pre.textContent = JSON.stringify(status, null, 2); target.append(pre);
  if (token) { const details = document.createElement('details'); const summary = document.createElement('summary'); summary.textContent = 'Reveal bearer ecash token'; const secret = document.createElement('pre'); secret.textContent = token; details.append(summary, secret); target.append(details); }
}
async function connect(candidate) {
  signer = candidate;
  try {
    const pubkey = await signer.getPublicKey();
    const me = await api('/admin/api/me');
    $('identity').textContent = `Connected: ${me.pubkey || pubkey}`;
    $('connect').classList.add('connected'); $('disconnect').hidden = false; $('workspace').hidden = false;
    $('bunker').value = ''; await navigate();
  } catch (e) { signer = undefined; throw e; }
}
$('extension').addEventListener('click', () => run(() => connect(new ExtensionSigner())));
$('bunker-form').addEventListener('submit', (e) => { e.preventDefault(); run(async () => {
  const remote = await NostrConnectSigner.fromBunkerURI($('bunker').value, { permissions: NostrConnectSigner.buildSigningPermissions([27235]), onAuth: async (url) => { notice(`Signer authorization required: ${url}`); } });
  await connect(remote);
}); });
$('disconnect').addEventListener('click', () => { if (signer?.close) signer.close(); signer = undefined; $('identity').textContent = 'Not connected'; $('workspace').hidden = true; $('disconnect').hidden = true; $('connect').classList.remove('connected'); $('withdrawal-data').replaceChildren(); notice('Disconnected.'); });
const sections = ['overview', 'blobs', 'reports', 'access', 'settings', 'wallet', 'activity'];
async function navigate() {
  if (!signer) return;
  const current = sections.includes(location.hash.slice(1)) ? location.hash.slice(1) : 'overview';
  sections.forEach((s) => { $(s).hidden = s !== current; document.querySelector(`nav a[href="#${s}"]`).toggleAttribute('aria-current', s === current); });
  const link = document.querySelector(`nav a[href="#${current}"]`); link.setAttribute('aria-current', 'page');
  await load(current);
}
async function load(section) {
  if (section === 'overview') {
    $('overview-data').textContent = 'Loading overview…'; const data = await api('/admin/api/status');
    let pending = 'Unavailable'; try { const config = await api('/admin/api/settings'); pending = Object.values(config.settings).filter(field => field.restart_required).length; } catch { /* Settings availability is independent of server status. */ }
    fields('overview-data', { 'Stored blobs': data.blob_count, 'Blob limit': data.max_total_files === 0 ? 'Unlimited' : data.max_total_files, 'Stored bytes': data.total_bytes, 'Storage limit (bytes)': data.max_total_size === 0 ? 'Unlimited' : data.max_total_size, 'Outstanding report targets': data.outstanding_reports, 'S3 backend': data.s3 ?? 'Disabled', 'Settings awaiting restart': pending });
    const links = document.createElement('p'); for (const [label, href] of [['Review reports','#reports'], ['Browse blobs','#blobs'], ['Review settings','#settings']]) { const link = document.createElement('a'); link.href = href; link.textContent = label; links.append(link, document.createTextNode('  ')); } $('overview-data').append(links);
  }
  if (section === 'blobs') await blobs();
  if (section === 'reports') await reports();
  if (section === 'activity') await activity();
  if (section === 'access') {
    const data = await api('/admin/api/collaborators'); collaboratorRevision = data.revision; show('access-data', data);
    $('collaborator-keys').value = (data.collaborators || []).join('\n'); $('collaborators').hidden = !data.writable; show('blocks-data', await api('/admin/api/blocks'));
  }
  if (section === 'settings') { $('settings-data').textContent = 'Loading settings…'; settings(await api('/admin/api/settings')); }
  if (section === 'wallet') {
    try { show('wallet-data', await api('/admin/wallet')); $('withdraw-form').hidden = false; $('lookup-form').hidden = false; }
    catch (e) { $('wallet-data').textContent = `Wallet unavailable: ${errorMessage(e)}`; $('withdraw-form').hidden = true; $('lookup-form').hidden = true; }
  }
}
let activityCursor = null;
async function activity() {
  const target = $('activity-data'); target.textContent = 'Loading activity…';
  const params = new URLSearchParams(); if (activityCursor) params.set('cursor', activityCursor);
  const data = await api(`/admin/api/activity${params.size ? `?${params}` : ''}`); target.replaceChildren();
  if (!data.items?.length) target.textContent = 'No administrative activity found.';
  for (const item of data.items || []) {
    const row = document.createElement('div'); row.className = 'item';
    const heading = document.createElement('h2'); heading.textContent = `${item.action} — ${item.outcome?.status || 'uncertain'}`; row.append(heading);
    const detail = document.createElement('pre'); detail.textContent = JSON.stringify(item, null, 2); row.append(detail);
    if (item.outcome?.status === 'uncertain') {
      const form = document.createElement('form'); const label = document.createElement('label'); label.textContent = 'Observations after inspecting the actual state';
      const input = document.createElement('textarea'); input.required = true; input.maxLength = 2048; input.rows = 3; label.append(input);
      const submit = document.createElement('button'); submit.type = 'submit'; submit.textContent = 'Record reconciliation'; form.append(label, submit);
      form.addEventListener('submit', event => { event.preventDefault(); run(async () => {
        const reason = input.value.trim(); if (!reason) throw new Error('Describe what you observed before reconciling.');
        if (!confirm(`Record your observations for action ${item.id}? This does not assert that the action succeeded or failed.`)) return;
        submit.disabled = true;
        try { await api(`/admin/api/activity/${encodeURIComponent(item.id)}/reconcile`, 'POST', { reason }); await activity(); notice('Reconciliation recorded.'); }
        catch (e) { throw new Error(`Could not confirm reconciliation. Refresh activity before retrying. ${errorMessage(e)}`); }
        finally { submit.disabled = false; }
      }); }); row.append(form);
    }
    target.append(row);
  }
  const pages = document.createElement('div'); pages.className = 'row';
  if (activityCursor) pages.append(button('First page', async () => { activityCursor = null; await activity(); }));
  if (data.next_cursor) pages.append(button('Next page', async () => { activityCursor = data.next_cursor; await activity(); }));
  target.append(pages);
}
async function reports() {
  const params = new URLSearchParams(); if (reportCursor) params.set('cursor', reportCursor);
  const target = $('report-data'); target.textContent = 'Loading reports…';
  const data = await api(`/admin/api/reports${params.size ? `?${params}` : ''}`); target.replaceChildren();
  if (!data.items?.length) target.textContent = 'No reports found.';
  for (const group of data.items || []) {
    const row = document.createElement('div'); row.className = 'item'; const title = document.createElement('h2'); title.textContent = group.hash; row.append(title);
    for (const report of group.reports || []) { const detail = document.createElement('details'); const summary = document.createElement('summary'); const reviews = (report.reviews || []).filter(review => review.hash === group.hash); const latest = reviews.reduce((a, b) => !a || b.time > a.time ? b : a, null); const state = !latest ? 'open' : latest.action === 'dismiss' ? 'dismissed' : 'action taken'; summary.textContent = `Report ${report.event?.id || 'unknown'} — ${state}`; const content = document.createElement('pre'); content.textContent = JSON.stringify(report, null, 2); detail.append(summary, content); row.append(detail); }
    const ids = (group.reports || []).map(report => report.event?.id).filter(Boolean);
    row.append(button('Review this blob', () => { selectedReport = ids; $('hashes').value = group.hash; location.hash = 'blobs'; })); target.append(row);
  }
  const pages = $('report-pages'); pages.replaceChildren();
  if (reportCursor) pages.append(button('First page', async () => { reportCursor = null; await reports(); }));
  if (data.next_cursor) pages.append(button('Next page', async () => { reportCursor = data.next_cursor; await reports(); }));
}
async function blobs() {
  const params = new URLSearchParams(); for (const [id, key] of [['blob-query','search'], ['blob-type','mime_type'], ['blob-backend','backend'], ['blob-min-size','min_size'], ['blob-max-size','max_size'], ['blob-origin','origin'], ['blob-expiration','expiration'], ['blob-moderation','moderation'], ['blob-after','created_after'], ['blob-before','created_before']]) { const value = $(id).value.trim(); if (value) params.set(key, value); } if (cursor) params.set('cursor', cursor);
  const target = $('blob-data'); target.textContent = 'Loading blobs…'; const data = await api(`/admin/api/blobs${params.size ? `?${params}` : ''}`); target.replaceChildren();
  if (!data.items?.length) target.textContent = 'No blobs found.';
  for (const item of data.items || []) {
    const row = document.createElement('div'); row.className = 'item'; const hash = document.createElement('code'); hash.textContent = item.hash; row.append(hash);
    const details = document.createElement('pre'); details.textContent = JSON.stringify(item.metadata, null, 2); row.append(details);
    row.append(button('View details', async () => { $('blob-detail').textContent = 'Loading blob details…'; show('blob-detail', await api(`/admin/api/blobs/${encodeURIComponent(item.hash)}`)); $('blob-detail').scrollIntoView({block:'nearest'}); }));
    row.append(button('Use in action', () => { selectedReport = null; $('hashes').value = item.hash; $('hashes').focus(); }));
    row.append(button('Download privileged preview', async () => { const blob = await api(`/admin/api/blobs/${encodeURIComponent(item.hash)}/preview`); const a = document.createElement('a'); a.href = URL.createObjectURL(blob); a.download = item.hash; a.click(); setTimeout(() => URL.revokeObjectURL(a.href), 1000); }));
    target.append(row);
  }
  const pages = $('blob-pages'); pages.replaceChildren(); if (cursor) pages.append(button('First page', async () => { cursor = null; await blobs(); })); if (data.next_cursor) pages.append(button('Next page', async () => { cursor = data.next_cursor; await blobs(); }));
}
$('blob-search').addEventListener('submit', (e) => { e.preventDefault(); cursor = null; run(blobs); });
document.querySelectorAll('[data-refresh]').forEach((b) => b.addEventListener('click', () => run(() => load(b.dataset.refresh))));
$('hashes').addEventListener('input', () => { selectedReport = null; });
$('actions').addEventListener('submit', (e) => { e.preventDefault(); run(async () => {
  const hashes = $('hashes').value.split(/\s+/).filter(Boolean); const action = $('action').value; const reason = $('reason').value;
  if (!hashes.length || hashes.length > 100) throw new Error('Select between 1 and 100 hashes.');
  if (!confirm(`${action.toUpperCase()} ${hashes.length} hash(es)?\n${hashes.join('\n')}\n\nModeration does not automatically refund paid retention.`)) return;
  try { const result = await api('/admin/api/actions', 'POST', { action, hashes, reason, report_ids: selectedReport || [] }); show('action-result', result); selectedReport = null; notice(result.results?.some(item => !item.ok || item.status === 'uncertain') ? 'Some actions failed or have uncertain outcomes. Review each result and activity before retrying.' : 'Action outcomes are shown below.'); }
  catch (e) { throw new Error(`Action outcome may be uncertain. Check activity before retrying. ${errorMessage(e)}`); }
}); });
$('collaborators').addEventListener('submit', (e) => { e.preventDefault(); run(async () => {
  const collaborators = $('collaborator-keys').value.split(/\s+/).filter(Boolean); if (!confirm('Replace the collaborator list? Root administrators are unchanged.')) return;
  show('access-data', await api('/admin/api/collaborators', 'PATCH', { base_revision: collaboratorRevision, collaborators })); await load('access'); notice('Collaborators saved.');
}); });
$('setting-form').addEventListener('submit', (e) => { e.preventDefault(); run(async () => {
  const name = $('setting-name').value.trim(); const reset = $('setting-reset').checked;
  const value = $('setting-value').value; const body = { base_revision: revision, set: reset ? {} : { [name]: value }, reset: reset ? [name] : [] };
  try { await api('/admin/api/settings', 'PATCH', body); await load('settings'); notice('Settings saved. Restart is required for server settings.'); }
  catch (e) { if (errorMessage(e).startsWith('412 ')) { await load('settings'); throw new Error('Settings changed since you loaded them. Review current values and try again.'); } throw e; }
}); });
$('withdraw-form').addEventListener('submit', (e) => { e.preventDefault(); run(async () => {
  const amount = Number($('amount').value), request_id = $('request-id').value.trim();
  if (!Number.isSafeInteger(amount) || amount < 1 || !request_id) throw new Error('Enter a valid amount and request ID.');
  if (!confirm(`Withdraw ${amount} with request ID ${request_id}? Keep the resulting bearer ecash private.`)) return;
  $('lookup-id').value = request_id;
  try { withdrawal(await api('/admin/withdraw', 'POST', { amount, request_id })); }
  catch (e) { throw new Error(`Withdrawal outcome may be uncertain. Look up request ${request_id}; retry only with the same ID and amount. ${errorMessage(e)}`); }
}); });
$('lookup-form').addEventListener('submit', (e) => { e.preventDefault(); run(async () => withdrawal(await api(`/admin/withdraw/${encodeURIComponent($('lookup-id').value.trim())}`))); });
$('reclaim').addEventListener('click', () => run(async () => { const id = $('lookup-id').value.trim(); if (!id) throw new Error('Enter a withdrawal request ID.'); if (confirm(`Reclaim withdrawal ${id}?`)) show('withdrawal-data', await api(`/admin/withdraw/${encodeURIComponent(id)}/reclaim`, 'POST')); }));
