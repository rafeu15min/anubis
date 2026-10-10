// O favorito (bookmarklet) da Crunchyroll: roda no crunchyroll.com, com a
// sessão de quem está logado, e baixa o histórico de exibição (o mesmo que o
// site mostra). Este arquivo entra como texto (import ?raw) no link
// "javascript:" das Preferências: precisa ser uma função só, sem nada de fora.
async function crunchyrollHistory() {
  if (location.hostname !== 'www.crunchyroll.com') { alert('Abra o www.crunchyroll.com (logado) e clique no favorito lá.'); return; }
  try {
    const cfg = window.__APP_CONFIG__ || (window.etp || {}).config || {};
    const id = (cfg.cxApiParams || {}).accountAuthClientId;
    if (!id) throw new Error('não achei a configuração do site (a Crunchyroll mudou algo)');
    // o site manda o aparelho junto: device_id vem do cookie dele, device_type é "Navegador on Sistema"
    const device = (document.cookie.match(/(?:^|; )device_id=([^;]*)/) || [])[1] || crypto.randomUUID();
    const ua = navigator.userAgent;
    const kind = (/Firefox\//.test(ua) ? 'Firefox' : /Edg\//.test(ua) ? 'Edge' : 'Chrome') + ' on ' + (/Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows' : /Mac OS/.test(ua) ? 'Mac OS' : 'Linux');
    // token como o site pede (com o perfil, quando a conta tem vários)
    const token = async (profile) => {
      const tr = await fetch('/auth/v1/token', { method: 'POST', credentials: 'include',
        headers: { Authorization: 'Basic ' + btoa(id + ':'), 'Content-Type': 'application/x-www-form-urlencoded' },
        body: 'grant_type=etp_rt_cookie&device_id=' + encodeURIComponent(device) + '&device_type=' + encodeURIComponent(kind) + (profile ? '&profile_id=' + encodeURIComponent(profile) : '') });
      if (!tr.ok) {
        const why = await tr.text();
        throw new Error(/etp_rt/.test(why) ? 'este navegador não está logado na Crunchyroll (entre com sua conta no site e tente de novo)' : 'sessão recusada (HTTP ' + tr.status + '): ' + why.slice(0, 200));
      }
      return tr.json();
    };
    const get = (tok, url) => fetch(url, { headers: { Authorization: 'Bearer ' + tok.access_token } });
    let tok = await token();
    // o histórico, como o site: page_size=100 e depois o next_page que a Crunchyroll devolve
    let first = await get(tok, '/content/v2/' + tok.account_id + '/watch-history?page_size=100');
    if (!first.ok) {
      // conta com vários perfis: o histórico é do perfil
      const pr = await get(tok, '/accounts/v1/me/multiprofile');
      const profiles = pr.ok ? ((await pr.json()).profiles || []) : [];
      let pick = profiles.length === 1 ? profiles[0] : null;
      if (profiles.length > 1) {
        const names = profiles.map((p, i) => (i + 1) + ') ' + (p.profile_name || p.username || p.profile_id)).join('\n');
        pick = profiles[Number(prompt('De qual perfil é o histórico?\n' + names, '1')) - 1];
      }
      if (pick) {
        tok = await token(pick.profile_id);
        first = await get(tok, '/content/v2/' + tok.account_id + '/watch-history?page_size=100');
      }
      if (!first.ok) throw new Error('histórico: HTTP ' + first.status + ' ' + (await first.text()).slice(0, 200));
    }
    const items = {};
    let total = 0;
    let r = first;
    for (let n = 0; n < 500; n++) {
      if (!r.ok) throw new Error('histórico (página ' + (n + 1) + '): HTTP ' + r.status + ' ' + (await r.text()).slice(0, 200));
      const body = await r.json();
      for (const h of body.data || []) {
        const p = h.panel || {};
        const ep = p.episode_metadata || {};
        const movie = p.type === 'movie';
        const key = movie ? p.id : (ep.series_id || p.id);
        const title = movie ? p.title : (ep.series_title || p.title);
        if (!key || !title) continue;
        const it = items[key] || (items[key] = { title, movie, episodes: 0, last: '' });
        it.episodes++; total++;
        if ((h.date_played || '') > it.last) it.last = h.date_played || '';
      }
      const next = (body.meta || {}).next_page;
      if (!next || !(body.data || []).length) break;
      r = await get(tok, next);
    }
    const out = { source: 'crunchyroll', exported: new Date().toISOString(), items: Object.values(items) };
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([JSON.stringify(out)], { type: 'application/json' }));
    a.download = 'crunchyroll-historico.json';
    document.body.appendChild(a); a.click(); a.remove();
    alert(out.items.length + ' títulos (' + total + ' episódios) salvos em crunchyroll-historico.json. Envie esse arquivo nas Preferências do Anubis.');
  } catch (e) { alert('Não deu: ' + e.message); }
}
