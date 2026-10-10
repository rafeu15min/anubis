// Dados e ações de Vídeos (filmes, séries, animes). A tela (views/Videos.vue
// e components/video/*) só desenha o que está em `V`; tudo o que fala com o
// servidor (src/api.rs) está aqui.
import { reactive, watch } from 'vue';
import { api, jf, storedJson, storeJson, size, sleep } from './util.js';
import { sections } from './sections.js';

export const keyOf = (it) => `${it.kind}:${it.id}`;

export const V = reactive({
  items: {},        // chave → título (da vitrine, da busca ou dos pedidos)
  status: {},       // chave → estado no servidor (processado por `apply`)
  // pedidos acompanhados: ficam no navegador para sobreviver a um recarregar
  tracked: storedJson('anubis.pedidos', {}),
  showcase: {},     // categoria → { ready: [chave], rows: [{ title, keys }], errors, error, checking, eta }
  results: null,    // busca: { loading } | { movies, series, errors } | { error }
  liked: {},        // chave → true
  busy: {},         // chave → pedindo / apagando
  watching: {},     // chave → mensagem do "assistir" em andamento
  dub: {},          // chave → { label, title }: "procurar dublagem" clicado
  open: null,       // chave do cartão aberto
  series: null,     // da série aberta: { k, tvdb, seasons, error, epAvail, note }
  pick: null,       // janela "nenhuma versão passa no perfil"
});

// navegação dentro do app (App.vue liga ao roteador)
let go = (path) => location.assign(path);
export function setNavigate(fn) { go = fn; }

export const itemOf = (k) => V.items[k] || V.tracked[k];
function put(it) {
  const k = keyOf(it);
  V.items[k] = V.items[k] ? Object.assign(V.items[k], it) : it;
  return k;
}

// ---- pedidos acompanhados
watch(() => Object.keys(V.tracked).sort().join(','), () => watchLive());
watch(() => V.tracked, (t) => storeJson('anubis.pedidos', t), { deep: true });
function untrack(k) { delete V.tracked[k]; }
// remove da lista o que já está disponível há um tempo
function prune() {
  for (const [k, t] of Object.entries(V.tracked)) if (!t.openWhenReady && t.done && Date.now() - t.done > 864e5) delete V.tracked[k];
}

// ---- estado de um título
// a busca de versões é lenta (vai aos indexadores): uma por item, refeita a cada 15 min
const versionsSeen = {};
async function versions(it) {
  const k = keyOf(it), c = versionsSeen[k];
  if (c && (c.pending || Date.now() - c.at < 15 * 60e3)) return c.v;
  versionsSeen[k] = { pending: true, v: c && c.v };
  try { versionsSeen[k] = { at: Date.now(), v: await api(`/status/${it.kind}/${it.id}/versions`) }; } catch { versionsSeen[k] = { at: Date.now(), v: null }; }
  return versionsSeen[k].v;
}

// aplica um estado (da consulta ou da conexão ao vivo)
export async function apply(k, st, openable = true) {
  const it = itemOf(k), t = V.tracked[k];
  if (!it) return;
  // o servidor não tem mais o pedido (apagado aqui, em outro aparelho ou no Radarr/Sonarr)
  if (t && st.state === 'not_requested' && !st.mux) { untrack(k); V.status[k] = st; return; }
  // pedido avulso ainda juntando: não conta como pronto (a dublagem
  // automática é de algo que já está na biblioteca)
  if (st.mux && !st.mux.auto && st.mux.step !== 'done') st.state = 'searching';
  if (st.state === 'searching' && !st.mux) {
    st.versions = await versions(it);
    // há versões com fontes, mas nenhuma dublada: oferece o legendado
    const v = st.versions;
    if (v && v.approved === 0 && v.dubbed === 0 && v.found - v.no_seeders > 0) st.offerSubbed = true;
  }
  V.status[k] = st;
  if (t && st.state === 'available' && !t.done) t.done = Date.now();
  // pedido feito nesta página e pronto: abre o Jellyfin (uma vez)
  if (openable && t && t.openWhenReady && st.state === 'available' && st.jellyfin_url) {
    t.openWhenReady = false;
    location.href = jf(st.jellyfin_url);
  }
}

// consulta avulsa (resultado de busca, ou sem a conexão ao vivo)
export async function refresh(k, openable = true) {
  const it = itemOf(k);
  if (!it) return;
  try { await apply(k, await api(`/status/${it.kind}/${it.id}`), openable); } catch {}
}

// estado ao vivo: uma conexão por aba (SSE). O servidor manda o estado de
// cada item ao conectar e depois só o que muda; sem ela (ou enquanto ela
// reconecta), consulta a cada 3 s o que ainda não está pronto.
let stream = null, streamKeys = '', pollTimer = null;
function fallbackPolling(on) {
  if (on && !pollTimer) pollTimer = setInterval(() => { for (const [k, t] of Object.entries(V.tracked)) if (!t.done) refresh(k); }, 3000);
  if (!on && pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}
function watchLive() {
  const keys = Object.keys(V.tracked).filter((k) => k.startsWith('movie:') || k.startsWith('series:')).sort().join(',');
  if (stream && keys === streamKeys) return;
  streamKeys = keys;
  if (stream) { stream.close(); stream = null; }
  if (!keys) { fallbackPolling(false); return; }
  if (!window.EventSource) { fallbackPolling(true); return; }
  stream = new EventSource(`/events?items=${encodeURIComponent(keys)}`);
  stream.addEventListener('status', (e) => {
    const d = JSON.parse(e.data);
    if (V.tracked[d.key] && !d.status.error) apply(d.key, d.status);
  });
  stream.onopen = () => fallbackPolling(false);
  stream.onerror = () => fallbackPolling(true);
}

// pedidos e biblioteca vêm do servidor: a mesma lista em qualquer aparelho e caminho
async function loadRequests() {
  try {
    const d = await api('/requests');
    for (const it of [...d.movies, ...d.series]) {
      const k = keyOf(it);
      if (!V.tracked[k]) V.tracked[k] = { ...it, requested: true, at: 0 };
    }
  } catch {}
}

// ---- vitrine por categoria: prontos para assistir + Destaques, Lançamentos
// e Populares no Brasil (TMDB)
let prefetched = false, availTimer = null;
const loadingCat = {};
export async function showCategory(cat) {
  if (loadingCat[cat]) return loadingCat[cat];
  loadingCat[cat] = (async () => {
    try {
      const d = await api(`/discover/${cat}`);
      const ready = d.ready.map((it) => put({ ...it, available: true }));  // na biblioteca: a janela oferece apagar
      const rows = d.rows.map((row) => ({ title: row.title, keys: row.items.map(put) }));
      V.showcase[cat] = { ready, rows, errors: d.errors, error: null, checking: d.checking, eta: d.checking_eta, at: Date.now() };
      watchAvailability(d.checking);
      // as outras abas já ficam prontas no servidor (a 1ª vez de séries e
      // animes leva segundos), sem passar na frente da aba aberta
      if (!prefetched) { prefetched = true; for (const c of ['filmes', 'series', 'animes']) if (c !== cat) fetch(`/discover/${c}?bg=1`).catch(() => {}); }
    } catch (e) {
      if (!V.showcase[cat]) V.showcase[cat] = { ready: [], rows: [], errors: [], error: e.message };
      else V.showcase[cat].error = e.message;
    } finally { delete loadingCat[cat]; }
  })();
  return loadingCat[cat];
}
// enquanto há fila, a cada 30 s: selos e resumo, sem reordenar
function watchAvailability(pending) {
  clearTimeout(availTimer);
  if (pending) availTimer = setTimeout(pollAvailability, 30000);
}
async function pollAvailability() {
  if (document.hidden) { watchAvailability(1); return; }
  try {
    const d = await api('/availability');
    for (const [k, a] of Object.entries(d.states)) if (V.items[k] && V.items[k].avail) V.items[k].avail = a;
    const sc = V.showcase[sections.video.tab];
    if (sc) { sc.checking = d.pending; sc.eta = d.eta; }
    watchAvailability(d.pending);
  } catch { watchAvailability(1); }
}
// título ainda não conferido: passa na frente da fila
async function checkNow(k) {
  const [kind, id] = k.split(':');
  try {
    const d = await api(`/availability/${kind}/${id}`, { method: 'POST' });
    const it = V.items[k];
    if (it && d.avail) { it.avail = d.avail; it.uncertain = d.avail === 'unknown'; }
  } catch {}
}

// ---- busca
let qSeq = 0;
export async function search(term) {
  const seq = ++qSeq;
  V.results = { loading: true };
  try {
    const d = await api(`/search?q=${encodeURIComponent(term)}`);
    if (seq !== qSeq) return;
    V.results = { movies: d.movies.map(put), series: d.series.map(put), errors: d.errors };
    // os já pedidos mostram o estado atual
    for (const it of [...d.movies, ...d.series]) if (it.requested) refresh(keyOf(it), false);
  } catch (e) {
    if (seq === qSeq) V.results = { error: e.message };
  }
}
export function clearSearch() { qSeq++; V.results = null; }

// ---- ações
export async function request(k) {
  const it = itemOf(k);
  if (!it || V.busy[k]) return;
  V.busy[k] = true;
  try {
    const st = await api('/request', { method: 'POST', body: { kind: it.kind, id: it.id } });
    // pronto ao terminar: abre o Jellyfin (não numa série que já tinha episódios)
    V.tracked[k] = { ...it, requested: true, openWhenReady: !it.available, at: Date.now() };
    V.status[k] = st;
    if (it.kind === 'series' && V.open === k) setTimeout(() => { if (V.open === k) loadEpisodes(k); }, 10000);
  } catch (e) {
    alert(`Não deu para pedir: ${e.message}`);
  } finally { delete V.busy[k]; }
}

// ⬇ de um episódio: a busca do "assistir" escolhe e o Sonarr baixa
export async function requestEpisode(id) {
  const k = `episode:${id}`;
  V.busy[k] = true;
  try {
    V.status[k] = await api('/request', { method: 'POST', body: { kind: 'episode', id } });
    const open = V.open;
    setTimeout(() => { if (V.open === open) loadEpisodes(open); }, 10000);
  } catch (e) {
    alert(`Não deu para baixar: ${e.message}`);
  } finally { delete V.busy[k]; }
}

// "assistir agora": o Anubis escolhe a versão, começa a baixar e abre o player
export async function watchNow(k, guid, keep = false) {
  const [kind, idS] = k.split(':'), id = +idS;
  if (V.watching[k]) return;
  V.watching[k] = 'Procurando versão…';
  // a busca passa por todas as fontes; as protegidas pelo Cloudflare levam ~30 s
  const slow = [setTimeout(() => { V.watching[k] = 'Buscando nas fontes…'; }, 8000), setTimeout(() => { V.watching[k] = 'Ainda buscando (até 1–2 min)…'; }, 30000)];
  try {
    const d = await api('/watch', { method: 'POST', body: { kind, id, guid, keep } });
    V.pick = null;
    V.open = null;
    go('/' + d.player.replace(/^\//, ''));
  } catch (e) {
    const d = e.data;
    if (e.status === 409 && d && d.no_approved) {
      const some = d.candidates.length > 0;
      // recusadas só por não serem dubladas: é a hora do legendado
      const onlyDub = some && d.candidates.every((c) => c.reasons.length && c.reasons.every((x) => /Custom Formats/i.test(x)));
      // dubladas que existem mas não tinham ninguém compartilhando agora
      const dead = (d.dead || []).length
        ? `${d.dead.length === 1 ? 'A versão dublada encontrada não tem' : `As ${d.dead.length} versões dubladas encontradas não têm`} ninguém compartilhando agora (${d.dead.join('; ')}). `
        : '';
      V.pick = {
        k,
        candidates: d.candidates.map((c) => ({ ...c, sizeText: size(c.size) })),
        onlyDub,
        title: onlyDub ? 'Dublagem não encontrada' : some ? 'Nenhuma versão passa no seu perfil de qualidade' : 'Nenhuma versão disponível',
        msg: dead + (onlyDub
          ? `${dead ? '' : 'Nenhuma fonte tem a versão dublada. '}A primeira é a recomendada; a legenda em português aparece no player se vier no arquivo (as da Amazon costumam trazer).`
          : some
            ? 'Todas as versões encontradas foram recusadas. Você pode assistir uma delas mesmo assim — o perfil não muda.'
            : 'As fontes não têm nenhuma versão com gente compartilhando agora.'),
        // fontes fora do ar: a busca pode ter deixado versões de fora
        down: d.indexers_down || [],
        downStill: false,
        retesting: false,
        // episódio: o "onde assistir" é da série
        where: kind === 'episode' ? (V.series ? ['series', V.series.tvdb] : null) : [kind, id],
      };
    } else alert(`Não deu para assistir: ${e.message}`);
  } finally {
    slow.forEach(clearTimeout);
    delete V.watching[k];
  }
}

// testa de novo as fontes fora do ar; alguma voltou: busca de novo
export async function retestIndexers() {
  const p = V.pick;
  p.retesting = true;
  try {
    const d = await api('/indexers/test', { method: 'POST' });
    if (d.down.length < p.down.length) { V.pick = null; watchNow(p.k); return; }
    p.down = d.down;
    p.downStill = true;
  } catch {} finally { p.retesting = false; }
}

// "Dublagem não encontrada, instalar legendado?" — sim
export async function acceptSubbed(k) {
  const [kind, id] = k.split(':');
  V.busy[`subbed:${k}`] = true;
  try {
    await apply(k, await api(`/${kind}/${id}/subbed`, { method: 'POST' }), false);
  } catch (e) { alert(`Não deu: ${e.message}`); } finally { delete V.busy[`subbed:${k}`]; }
}

// 🎙 procurar a dublagem agora (filme, episódio ou a série toda)
export async function dubNow(k) {
  const [kind, id] = k.split(':');
  V.dub[k] = { label: 'procurando…', title: '' };
  try {
    const d = await api(`/autodub/${kind}/${id}`, { method: 'POST' });
    V.dub[k] = d.queued.length
      ? { label: 'procurando dublagem', title: 'Quando a dublagem entrar, o português passa a tocar primeiro' }
      : { label: 'nada sem português', title: 'Nada sem português aqui' };
  } catch (e) {
    V.dub[k] = { label: 'não deu', title: e.message };
  }
}

// apagar da biblioteca (filme, série ou episódio): mostra o que sai e pede
// confirmação, porque não tem volta. `true` se apagou
export async function uninstall(k, ask = 'Apagar da biblioteca?') {
  const [kind, id] = k.split(':');
  V.busy[k] = true;
  try {
    const p = await api(`/library/${kind}/${id}`);
    const files = p.files > 1 ? ` (${p.files} arquivos)` : '';
    const seeding = p.torrents.length ? `\n\nSai junto do qBittorrent: ${p.torrents.join(', ')}.` : '';
    const kept = p.kept.length ? `\n\nContinua no pacote que semeia (${p.kept.join(', ')}); esse espaço não volta.` : '';
    if (!confirm(`${ask}\n\n${p.title}${files}: libera ${size(p.frees)}.${seeding}${kept}\n\nSai do disco e do Jellyfin e não é baixado de novo. Não dá para desfazer.`)) return false;
    const d = await api(`/library/${kind}/${id}`, { method: 'DELETE' });
    if (d.errors.length) alert(`Apagado em parte: ${d.errors.join('; ')}`);
  } catch (e) {
    alert(`Não deu: ${e.message}`);
    return false;
  } finally { delete V.busy[k]; }
  if (kind === 'episode') { if (V.open) loadEpisodes(V.open); return true; }
  // filme ou série: some de "Prontos" e o cartão volta a "pedir"
  const it = V.items[k];
  if (it) { it.available = false; it.requested = false; }
  untrack(k);
  delete V.status[k];
  if (V.open === k) V.open = null;
  showCategory(sections.video.tab);
  return true;
}

// "gostei": liga e desliga; vira semente do "Para você" (peso de favorito)
export async function toggleLike(k) {
  const [kind, id] = k.split(':');
  const on = !V.liked[k];
  V.busy[`like:${k}`] = true;
  try {
    await api(`/liked/${kind}/${id}`, { method: on ? 'POST' : 'DELETE' });
    if (on) V.liked[k] = true; else delete V.liked[k];
  } catch (e) { alert(`Não deu: ${e.message}`); } finally { delete V.busy[`like:${k}`]; }
}

// "não quero ver": sai da vitrine (e o que se parece com ele aparece menos)
export async function hide(k) {
  const [kind, id] = k.split(':');
  V.busy[`hide:${k}`] = true;
  try {
    await api(`/hidden/${kind}/${id}`, { method: 'POST' });
    for (const sc of Object.values(V.showcase)) {
      sc.ready = sc.ready.filter((x) => x !== k);
      for (const row of sc.rows) row.keys = row.keys.filter((x) => x !== k);
    }
    delete V.liked[k];
    V.open = null;
    // já baixado: oferece apagar também (cancelar só esconde)
    if (V.items[k] && V.items[k].available) await uninstall(k, 'Também apagar da biblioteca?');
  } catch (e) { alert(`Não deu: ${e.message}`); } finally { delete V.busy[`hide:${k}`]; }
}

// ---- cartão aberto
export function openItem(k) {
  const it = itemOf(k);
  if (!it || V.open === k) return;
  V.open = k;
  V.series = null;
  if (it.requested || it.available || V.tracked[k]) refresh(k, false);
  if (it.avail === 'unknown') checkNow(k);
  if (it.kind === 'series') loadEpisodes(k);
}
export function closeItem() { V.open = null; V.series = null; }

// temporadas e episódios de uma série, dentro da própria janela dela
export async function loadEpisodes(k) {
  const it = itemOf(k);
  if (!it) return;
  const tvdb = it.id;
  if (!V.series || V.series.k !== k) V.series = { k, tvdb, seasons: null, error: null, epAvail: {}, note: '' };
  try {
    const d = await api(`/series/${tvdb}/episodes`);
    if (V.open !== k) return;
    V.series.seasons = d.seasons;
    V.series.error = null;
    // o que se pediu à mão já está na lista nova
    for (const x of d.seasons) for (const e of x.episodes) delete V.status[`episode:${e.id}`];
    episodeAvailability(k, tvdb);
    // baixando: a lista se atualiza até terminar
    if (d.seasons.some((x) => x.episodes.some((e) => e.downloading != null || e.st))) setTimeout(() => { if (V.open === k) loadEpisodes(k); }, 10000);
  } catch (e) {
    if (V.series && V.series.k === k) V.series.error = e.message;
  }
}
// o que as fontes têm de cada episódio que falta ("só legendado", "sem
// fonte"). O servidor avalia em segundo plano (202): pergunta de novo
// enquanto a janela está aberta
async function episodeAvailability(k, tvdb) {
  for (let i = 0; i < 60 && V.open === k; i++) {
    let r;
    try { r = await fetch(`/series/${tvdb}/availability`); } catch { return; }
    if (!V.series || V.series.k !== k) return;
    if (r.status === 202) { V.series.note = 'Conferindo as fontes de cada episódio…'; await sleep(4000); continue; }
    V.series.note = '';
    if (!r.ok) return;
    V.series.epAvail = (await r.json()).episodes || {};
    return;
  }
  if (V.series) V.series.note = '';
}

// ---- início (uma vez, na primeira vez que Vídeos aparece ou é pedido antes)
let started = false;
export function start() {
  if (started) return;
  started = true;
  prune();
  watchLive();
  loadRequests();
  api('/liked').then((l) => { for (const h of l) V.liked[`${h.kind}:${h.id}`] = true; }).catch(() => {});
}
export function prefetch() {
  start();
  if (!V.showcase[sections.video.tab]) showCategory(sections.video.tab);
}

// ---- da série aberta: o próximo a assistir, quantos estão no servidor,
// quantos faltam (já exibidos), quantos sem português
const today = () => new Date().toISOString().slice(0, 10);
export const aired = (e) => !e.air_date || e.air_date <= today();
export function seriesInfo() {
  const s = V.series;
  if (!s || !s.seasons) return null;
  const list = s.seasons.filter((x) => x.number > 0).flatMap((x) => x.episodes.map((e) => ({ ...e, season: x.number }))).filter(aired);
  const all = s.seasons.flatMap((x) => x.episodes);
  return {
    k: s.k,
    next: list.find((e) => !(e.watch && e.watch.played)) || list[0],
    have: all.filter((e) => e.has_file).length,
    missing: list.filter((e) => !e.has_file && e.downloading == null).length,
    noPt: all.filter((e) => e.has_file && e.portuguese === false).length,
  };
}
