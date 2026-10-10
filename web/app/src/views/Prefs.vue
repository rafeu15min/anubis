<script setup>
// Preferências: áudio, qualidade e legendas (Radarr/Sonarr/Bazarr, pela API
// /prefs), "gostei", ocultos e o histórico de outros serviços.
import { ref, reactive, computed, onMounted, onBeforeUnmount } from 'vue';
import { onBeforeRouteLeave } from 'vue-router';
import Queue from '../components/Queue.vue';
import { api, sleep } from '../lib/util.js';
import crSource from '../lib/crunchyroll.js?raw';

const RULES = {
  required: ['Obrigatório (dublado)', 'Só aceita versões em português do Brasil. O que não tiver versão dublada fica esperando.'],
  preferred: ['Preferido', 'Baixa no idioma original agora e troca sozinho pela versão dublada quando ela aparecer.'],
  any: ['Tanto faz', 'Português ou o idioma original, sem preferência entre os dois.'],
};
// nomes de qualidade do Radarr/Sonarr → como aparecem aqui
const QUAL = {
  'Remux-2160p': ['Remux 4K', 'cópia do disco sem compressão; arquivos enormes'], 'Bluray-2160p Remux': ['Remux 4K', 'cópia do disco sem compressão; arquivos enormes'],
  'Bluray-2160p': ['Bluray 4K', 'do disco, comprimido'], 'WEB 2160p': ['WEB 4K', 'do streaming (Netflix, Disney+…)'],
  'WEBDL-2160p': ['WEB-DL 4K', 'do streaming, sem reconversão'], 'WEBRip-2160p': ['WEBRip 4K', 'gravado do streaming'], 'HDTV-2160p': ['TV 4K', 'gravado da TV'],
  'Remux-1080p': ['Remux 1080p', 'sem compressão; muito grande'], 'Bluray-1080p Remux': ['Remux 1080p', 'sem compressão; muito grande'],
  'Bluray-1080p': ['Bluray 1080p', 'do disco, comprimido: a melhor imagem em 1080p'], 'WEB 1080p': ['WEB 1080p', 'do streaming'],
  'WEBDL-1080p': ['WEB-DL 1080p', 'do streaming, sem reconversão'], 'WEBRip-1080p': ['WEBRip 1080p', 'gravado do streaming'], 'HDTV-1080p': ['TV 1080p', 'gravado da TV'],
  'Bluray-720p': ['Bluray 720p', ''], 'WEB 720p': ['WEB 720p', 'do streaming'], 'WEBDL-720p': ['WEB-DL 720p', ''], 'WEBRip-720p': ['WEBRip 720p', ''], 'HDTV-720p': ['TV 720p', 'gravado da TV'],
  'Bluray-576p': ['Bluray 576p', 'definição padrão'], 'Bluray-480p': ['Bluray 480p', 'definição padrão'], 'WEB 480p': ['WEB 480p', 'definição padrão'],
  'DVD': ['DVD', 'definição padrão'], 'DVD-R': ['DVD-R', 'o disco DVD inteiro'], 'SDTV': ['TV SD', 'definição padrão, gravado da TV'],
  'DVDSCR': ['DVD de divulgação', 'screener, antes do lançamento'], 'REGIONAL': ['Regional', 'lançamento de outro país'],
  'CAM': ['Cinema (CAM)', 'filmado na sala de cinema'], 'TELESYNC': ['Cinema (Telesync)', 'filmado no cinema, áudio direto'],
  'TELECINE': ['Cinema (Telecine)', 'copiado da película'], 'WORKPRINT': ['Versão de trabalho', 'inacabada'],
  'BR-DISK': ['Disco Blu-ray inteiro', 'a pasta do disco; não toca no navegador'], 'Raw-HD': ['Raw-HD', 'captura de TV sem compressão'],
};
const qName = (n) => (QUAL[n] || [n])[0];
const qHint = (n) => (QUAL[n] || [, ''])[1];
// idiomas do Bazarr
const LANGS = { pb: 'Português (BR)', pt: 'Português (PT)', en: 'Inglês', es: 'Espanhol', fr: 'Francês', it: 'Italiano', de: 'Alemão', ja: 'Japonês', ko: 'Coreano', zh: 'Chinês', ru: 'Russo' };
const lName = (c) => LANGS[c] || c;

// ---- estado: `saved` é o que o servidor tem; `edit`, o que está na tela
const saved = ref(null);
const edit = ref(null);
const pick = (p) => ({
  movies_audio: p.movies_audio, series_audio: p.series_audio,
  movies_quality: p.movies_quality && { queue: [...p.movies_quality.queue], cutoff: p.movies_quality.cutoff, others: [...p.movies_quality.others] },
  series_quality: p.series_quality && { queue: [...p.series_quality.queue], cutoff: p.series_quality.cutoff, others: [...p.series_quality.others] },
  subs: p.subs && { languages: [...p.subs.languages], skip_if_audio: p.subs.skip_if_audio },
});
// o que conta para "mudou" (a lista de fora da fila é só consequência)
const essence = (e) => JSON.stringify([e.movies_audio, e.series_audio, e.movies_quality && [e.movies_quality.queue, e.movies_quality.cutoff], e.series_quality && [e.series_quality.queue, e.series_quality.cutoff], e.subs]);
const dirty = computed(() => !!saved.value && !!edit.value && essence(edit.value) !== essence(saved.value));
const msg = ref({ text: 'Carregando…', cls: '' });
const shown = computed(() => (msg.value.text != null ? msg.value : dirty.value ? { text: 'Alterações não salvas', cls: 'dirty' } : { text: 'Tudo salvo', cls: '' }));
const touched = () => { msg.value = { text: null, cls: '' }; };
const saving = ref(false);

const unload = (e) => { if (dirty.value) e.preventDefault(); };
onMounted(() => addEventListener('beforeunload', unload));
onBeforeUnmount(() => removeEventListener('beforeunload', unload));
onBeforeRouteLeave(() => !dirty.value || confirm('Sair sem salvar as alterações?'));

async function load() {
  try {
    const d = await api('/prefs');
    saved.value = pick(d); edit.value = pick(d);
    touched();
  } catch (e) { msg.value = { text: `Não carregou: ${e.message}`, cls: 'err' }; }
}
function undo() { edit.value = pick(saved.value); touched(); }
async function save() {
  const e = edit.value;
  const body = { movies_audio: e.movies_audio, series_audio: e.series_audio, subs: e.subs };
  for (const k of ['movies_quality', 'series_quality']) if (e[k]) body[k] = { queue: e[k].queue, cutoff: e[k].cutoff };
  saving.value = true;
  msg.value = { text: 'Salvando…', cls: '' };
  try {
    const d = await api('/prefs', { method: 'PUT', body });
    saved.value = pick(d); edit.value = pick(d);
    msg.value = { text: 'Salvo. Vale para as próximas buscas.', cls: 'ok' };
  } catch (err) { msg.value = { text: `Falhou: ${err.message}`, cls: 'err' }; } finally { saving.value = false; }
}
// tirou da fila de qualidade: vai para "fora da fila"; o alvo saiu: passa
// para a de baixo (ou a de cima, se era a última)
function qualityDrop(key, x, i) {
  const q = edit.value[key];
  q.others.unshift(x);
  if (q.cutoff === x) q.cutoff = q.queue[Math.min(i, q.queue.length - 1)];
}
function qualityAdd(key, x) { const q = edit.value[key]; q.others = q.others.filter((o) => o !== x); }
const subsOthers = computed(() => Object.keys(LANGS).filter((c) => !edit.value.subs.languages.includes(c)));

// ---- histórico de outros serviços
const hist = reactive({ netflix: '', crunchyroll: '', anilist: '', anilistUser: '' });
async function loadHistory() {
  try {
    const d = await api('/history');
    hist.netflix = d.netflix_titles ? `${d.netflix_titles} títulos importados (${d.netflix_at})` : 'nada importado ainda';
    hist.crunchyroll = d.crunchyroll_titles ? `${d.crunchyroll_titles} títulos importados (${d.crunchyroll_at})` : 'nada importado ainda';
    hist.anilistUser = d.anilist_user || '';
    hist.anilist = d.anilist_user ? `ligado${d.anilist_entries != null ? ` · ${d.anilist_entries} títulos usados` : ''}` : '';
    if (d.import.running) followImport();
  } catch {}
}
// a importação roda no servidor; a página só acompanha o andamento
const UNIT = { netflix: 'exibições', crunchyroll: 'episódios' };
let following = false, alive = true;
async function followImport() {
  if (following) return;
  following = true;
  let p;
  do {
    await sleep(1000);
    try { p = (await api('/history')).import; } catch { continue; }
    const r = p.report;
    hist[p.source] = p.running ? `Importando… ${p.done}/${p.total} títulos`
      : `${r.lines} ${UNIT[p.source]}, ${r.titles} títulos, ${r.matched} achados` + (r.unmatched.length ? ` · não achados: ${r.unmatched.join(', ')}` : '');
  } while (alive && (!p || p.running));
  following = false;
}
onBeforeUnmount(() => { alive = false; });
async function sendHistory(source, e, type) {
  const f = e.target.files[0];
  if (!f) return;
  hist[source] = 'Enviando…';
  try {
    const d = await api(`/history/${source}`, { method: 'POST', type, body: await f.text() });
    hist[source] = `Importando… 0/${d.total} títulos`;
    followImport();
  } catch (err) { hist[source] = `Falhou: ${err.message}`; }
  e.target.value = '';
}
async function saveAnilist() {
  hist.anilist = 'Lendo a lista…';
  try {
    const d = await api('/history/anilist', { method: 'PUT', body: { user: hist.anilistUser } });
    hist.anilist = d.anilist_user ? `ligado · ${d.anilist_entries ?? 0} títulos usados` : 'desligado';
  } catch (err) { hist.anilist = `Falhou: ${err.message}`; }
}
// o favorito da Crunchyroll: o código do arquivo vira um link "javascript:"
const crHref = 'javascript:' + encodeURIComponent('(' + crSource.replace(/^(\s*\/\/.*\n)+/, '') + ')()');
const crClick = () => alert('Arraste este botão para a barra de favoritos e use-o no crunchyroll.com.');

// ---- "gostei" (Anubis e Jellyfin) e ocultos: tirar vale na hora
const kindName = (k) => (k === 'movie' ? 'filme' : 'série');
const liked = ref(null), hidden = ref(null), removing = ref({});
async function loadLiked() { try { liked.value = (await api('/liked')).slice().reverse(); } catch {} }
async function loadHidden() { try { hidden.value = (await api('/hidden')).slice().reverse(); } catch {} }
async function unlike(h) { removing.value['l' + h.kind + h.id] = true; await fetch(`/liked/${h.kind}/${h.id}`, { method: 'DELETE' }); loadLiked(); }
async function unhide(h) { removing.value['h' + h.kind + h.id] = true; await fetch(`/hidden/${h.kind}/${h.id}`, { method: 'DELETE' }); loadHidden(); }

onMounted(() => { load(); loadHistory(); loadLiked(); loadHidden(); });
</script>

<template>
  <div class="prefs">
    <header class="pbar"><div class="bar"><RouterLink to="/" title="Voltar à busca" aria-label="Voltar à busca"><i class="i i-back"></i></RouterLink><h1>Preferências</h1></div></header>
    <main>
      <section>
        <h2>Idioma do áudio</h2>
        <p class="help">Vale para os pedidos e para o "Assistir".</p>
        <p v-if="!edit" class="empty">Carregando…</p>
        <template v-else>
          <template v-for="[key, title] in [['movies_audio', 'Filmes'], ['series_audio', 'Séries e animes']]" :key="key">
            <div v-if="edit[key] != null" class="field">
              <label :for="key">{{ title }}</label>
              <select :id="key" v-model="edit[key]" @change="touched">
                <option v-for="(r, v) in RULES" :key="v" :value="v">{{ r[0] }}</option>
              </select>
              <span class="desc">{{ RULES[edit[key]][1] }}</span>
            </div>
          </template>
          <p v-if="edit.movies_audio == null && edit.series_audio == null" class="empty">Radarr e Sonarr não configurados.</p>
        </template>
        <details class="how"><summary>Como funciona</summary>
          A decisão fica com o Radarr (filmes) e o Sonarr (séries e animes), com estas regras. Série ou anime em "obrigatório" sem versão dublada em nenhuma fonte: o cartão pergunta se pode instalar o legendado (e troca pelo dublado se ele surgir). Sem português, só o idioma original do título (legendado), nunca a dublagem em outro idioma. Entre as legendadas, as com legenda em português vêm primeiro.
        </details>
      </section>

      <section>
        <h2>Qualidade</h2>
        <p class="help">A ordem é a preferência: o Radarr e o Sonarr pegam a primeira da fila que tiver uma versão boa. O que está fora da fila é recusado.</p>
        <template v-if="edit">
          <template v-for="[key, title] in [['movies_quality', 'Filmes'], ['series_quality', 'Séries e animes']]" :key="key">
            <div v-if="edit[key]">
              <h3>{{ title }}</h3>
              <Queue :list="edit[key].queue" :others="edit[key].others" :name="qName" :hint="qHint" :goal="edit[key].cutoff" add-label="Adicionar qualidade"
                @change="touched" @drop="(x, i) => qualityDrop(key, x, i)" @add="(x) => qualityAdd(key, x)" />
              <div class="field">
                <label :for="key + '-cut'">Alvo</label>
                <select :id="key + '-cut'" v-model="edit[key].cutoff" @change="touched">
                  <option v-for="x in edit[key].queue" :key="x" :value="x">{{ qName(x) }}</option>
                </select>
                <span class="desc">Com {{ qName(edit[key].cutoff) }} ou melhor, não troca sozinho.</span>
              </div>
            </div>
          </template>
        </template>
        <details class="how"><summary>O que é o alvo</summary>
          Com um arquivo no alvo (ou acima dele), o Radarr/Sonarr não trocam sozinhos por uma versão melhor; quando você clica em Assistir, o player oferece a troca pelas que estão acima na fila. Abaixo do alvo, eles trocam sozinhos quando surge uma melhor. No "Assistir", uma versão com poucas fontes perde para uma bem semeada, e o player sugere uma mais leve se o download não acompanhar.
        </details>
      </section>

      <section>
        <h2>Legendas</h2>
        <template v-if="edit && !edit.subs"><p class="help">Bazarr não configurado no Anubis.</p></template>
        <template v-else>
          <p class="help">O Bazarr procura legendas para tudo o que entra na biblioteca, nesta ordem de preferência.</p>
          <template v-if="edit">
            <Queue :list="edit.subs.languages" :others="subsOthers" :name="lName" add-label="Adicionar idioma" @change="touched" />
            <label class="switch"><span>Não baixar legenda quando o áudio já está no idioma<span class="desc" style="display:block">Filme dublado dispensa legenda.</span></span>
              <input v-model="edit.subs.skip_if_audio" type="checkbox" @change="touched"></label>
          </template>
        </template>
      </section>

      <section>
        <h2>Gostei</h2>
        <p class="help">O 👍 do Anubis e os favoritos do Jellyfin: um vale pelo outro (marcar aqui marca lá). O que se parece com eles aparece mais no "Para você".</p>
        <ul class="items">
          <li v-if="!liked" class="empty">Carregando…</li>
          <li v-else-if="!liked.length" class="empty">Nada marcado.</li>
          <li v-for="h in liked || []" :key="h.kind + h.id">
            <span>{{ h.title }}<small>{{ kindName(h.kind) }}<template v-if="h.at"> · {{ h.at }}</template> ·
              <span v-if="h.anubis" class="src"><i class="i i-like"></i> Anubis</span> <span v-if="h.jellyfin" class="src jf"><i class="i i-heart"></i> Jellyfin</span></small></span>
            <button class="ic" :disabled="removing['l' + h.kind + h.id]" :title="`Tirar dos gostei${h.jellyfin ? ' (e dos favoritos do Jellyfin)' : ''}`" :aria-label="`Tirar ${h.title}`" @click="unlike(h)"><i class="i i-x"></i></button>
          </li>
        </ul>
      </section>

      <section>
        <h2>Ocultos da vitrine</h2>
        <p class="help">O que você marcou com 👎. Não aparece mais na vitrine, e o que se parece com eles aparece menos no "Para você". A busca continua achando.</p>
        <ul class="items">
          <li v-if="!hidden" class="empty">Carregando…</li>
          <li v-else-if="!hidden.length" class="empty">Nada oculto.</li>
          <li v-for="h in hidden || []" :key="h.kind + h.id">
            <span>{{ h.title }}<small>{{ kindName(h.kind) }} · {{ h.at }}</small></span>
            <button type="button" :disabled="removing['h' + h.kind + h.id]" @click="unhide(h)">Mostrar</button>
          </li>
        </ul>
      </section>

      <section>
        <h2>Histórico de outros serviços</h2>
        <p class="help">Alimenta o "Para você" com o que você assiste fora daqui.</p>
        <div class="svc">
          <div class="lbl">Netflix</div>
          <p class="note">Em <a href="https://www.netflix.com/viewingactivity" target="_blank" rel="noopener">Conta → Perfil → Atividade de visualização</a>, use "Baixar tudo" e envie o arquivo. Enviar de novo substitui o anterior.</p>
          <label class="btn file"><i class="i i-upload"></i> Enviar CSV da Netflix<input type="file" accept=".csv,text/csv" @change="sendHistory('netflix', $event, 'text/csv')"></label>
          <p class="note">{{ hist.netflix }}</p>
        </div>
        <div class="svc">
          <div class="lbl">Crunchyroll</div>
          <p class="note">No computador, arraste este botão para a barra de favoritos:
            <a :href="crHref" class="btn" style="min-height:36px;padding:4px 10px" @click.prevent="crClick">Histórico Crunchyroll → Anubis</a>.
            Depois abra <a href="https://www.crunchyroll.com" target="_blank" rel="noopener">crunchyroll.com</a> (logado) e clique no favorito: ele baixa <code>crunchyroll-historico.json</code>. A senha não passa pelo Anubis.</p>
          <label class="btn file"><i class="i i-upload"></i> Enviar histórico da Crunchyroll<input type="file" accept=".json,application/json" @change="sendHistory('crunchyroll', $event, 'application/json')"></label>
          <p class="note">{{ hist.crunchyroll }}</p>
        </div>
        <div class="svc">
          <div class="lbl">AniList</div>
          <p class="note">Com a extensão <a href="https://malsync.moe" target="_blank" rel="noopener">MAL-Sync</a> ligada a uma conta do <a href="https://anilist.co" target="_blank" rel="noopener">AniList</a>, o que você assiste na Crunchyroll vai sozinho para lá. Aqui, o usuário do AniList (lista pública).</p>
          <div class="inline"><input v-model="hist.anilistUser" type="text" placeholder="usuário do AniList" autocomplete="off"><button type="button" @click="saveAnilist">Ligar</button></div>
          <p class="note">{{ hist.anilist }}</p>
        </div>
      </section>
    </main>
    <div class="savebar"><div class="bar">
      <span class="smsg" :class="shown.cls">{{ shown.text }}</span>
      <button type="button" :disabled="!dirty || saving" @click="undo">Desfazer</button>
      <button type="button" class="primary" :disabled="!dirty || saving" @click="save">Salvar</button>
    </div></div>
  </div>
</template>

<style>
.prefs { font-size: 15px; line-height: 1.5; }
.prefs header.pbar { position: sticky; top: 0; z-index: 3; padding: 10px 16px; background: color-mix(in srgb, var(--bg) 92%, transparent); backdrop-filter: blur(6px); border-bottom: 1px solid var(--line); }
.prefs .bar { gap: 10px; max-width: 760px; }
.prefs .pbar a { display: inline-flex; align-items: center; justify-content: center; width: 44px; height: 44px; border-radius: 10px; color: var(--muted); text-decoration: none; font-size: 18px; }
.prefs .pbar a:hover { color: var(--accent); background: var(--panel); }
.prefs h1 { margin: 0; font-size: 17px; font-weight: 600; }
.prefs main { max-width: 760px; padding: 16px 16px 96px; }
.prefs section { margin: 0 0 16px; padding: 16px; border: 1px solid var(--line); border-radius: 14px; background: var(--panel); }
.prefs h2 { margin: 0 0 4px; font-size: 16px; text-transform: none; letter-spacing: 0; color: var(--text); }
.prefs h3 { margin: 18px 0 8px; font-size: 13px; font-weight: 600; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
.prefs section > div:first-of-type > h3, .prefs section > h3:first-of-type { margin-top: 12px; }
.prefs .help { margin: 0 0 8px; color: var(--muted); font-size: 13px; }
.prefs details.how { margin: 4px 0 0; color: var(--muted); font-size: 13px; }
.prefs details.how summary { cursor: pointer; color: var(--info); padding: 4px 0; }
.prefs a { color: var(--info); }
.prefs button, .prefs .btn { display: inline-flex; align-items: center; justify-content: center; gap: 8px; min-height: 44px; padding: 8px 16px; border-radius: 10px; border: 1px solid var(--line); background: var(--raised); color: var(--text); font: inherit; cursor: pointer; text-decoration: none; }
.prefs button:hover, .prefs .btn:hover { border-color: var(--accent); background: var(--raised); }
.prefs button:disabled { opacity: .4; cursor: default; border-color: var(--line); }
.prefs button.primary { background: var(--accent); border-color: var(--accent); color: #111; font-weight: 600; }
.prefs button.ic { width: 40px; min-height: 40px; padding: 0; border-radius: 8px; color: var(--muted); }
/* campo: rótulo em cima, controle, explicação embaixo */
.prefs .field { display: grid; gap: 6px; padding: 10px 0; border-top: 1px solid var(--line); }
.prefs .field:first-of-type { border-top: 0; }
.prefs .field > label { font-weight: 600; }
.prefs select, .prefs input[type=text] { width: 100%; min-height: 44px; padding: 8px 12px; border-radius: 10px; border: 1px solid var(--line); background: var(--bg); color: var(--text); font: inherit; }
.prefs select:focus, .prefs input:focus, .prefs button:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
.prefs .desc { color: var(--muted); font-size: 13px; }
/* fila de prioridade */
.prefs ol.queue { list-style: none; margin: 0; padding: 0; display: grid; gap: 6px; }
.prefs ol.queue li { display: grid; grid-template-columns: 26px 1fr auto; gap: 10px; align-items: center; padding: 6px 6px 6px 10px; border: 1px solid var(--line); border-radius: 10px; background: var(--raised); }
.prefs ol.queue li.goal { border-color: var(--accent); }
.prefs ol.queue .pos { color: var(--muted); font-variant-numeric: tabular-nums; text-align: center; }
.prefs ol.queue .what small { display: block; color: var(--muted); font-size: 12px; line-height: 1.3; }
.prefs button.more { width: 100%; min-height: 40px; margin-top: 6px; border-style: dashed; color: var(--muted); }
.prefs .qtag { margin-left: 6px; font-size: 11px; padding: 1px 6px; border-radius: 99px; border: 1px solid var(--accent); color: var(--accent); white-space: nowrap; vertical-align: 1px; }
.prefs .qtag .i { margin-right: 3px; }
.prefs ol.queue .moves { display: flex; gap: 4px; }
.prefs .addrow { margin-top: 8px; }
/* interruptor */
.prefs .switch { display: flex; gap: 12px; align-items: center; justify-content: space-between; min-height: 44px; cursor: pointer; }
.prefs .switch input { appearance: none; position: relative; flex: none; width: 46px; height: 28px; margin: 0; border-radius: 99px; background: var(--line); cursor: pointer; transition: background .15s; }
.prefs .switch input::after { content: ""; position: absolute; top: 3px; left: 3px; width: 22px; height: 22px; border-radius: 50%; background: var(--text); transition: transform .15s; }
.prefs .switch input:checked { background: var(--accent); }
.prefs .switch input:checked::after { transform: translateX(18px); background: #111; }
/* listas (gostei, ocultos) */
.prefs ul.items { list-style: none; margin: 0; padding: 0; }
.prefs ul.items li { display: grid; grid-template-columns: 1fr auto; gap: 10px; align-items: center; padding: 6px 0; border-top: 1px solid var(--line); }
.prefs ul.items li:first-child { border-top: 0; }
.prefs ul.items small { display: block; color: var(--muted); font-size: 12px; }
.prefs .src { font-size: 11px; padding: 0 6px; border-radius: 99px; border: 1px solid var(--line); color: var(--muted); white-space: nowrap; }
.prefs .src.jf { border-color: color-mix(in srgb, var(--info) 50%, transparent); color: var(--info); }
.prefs .empty { color: var(--muted); font-size: 13px; margin: 0; }
.prefs .svc { padding: 12px 0; border-top: 1px solid var(--line); }
.prefs .svc:first-of-type { border-top: 0; }
.prefs .svc .lbl { font-weight: 600; }
.prefs .file input { display: none; }
.prefs .inline { display: grid; grid-template-columns: 1fr auto; gap: 8px; }
.prefs .note { color: var(--muted); font-size: 13px; }
/* barra de salvar: sempre à vista; apagada sem mudanças */
.prefs .savebar { position: fixed; left: 0; right: 0; bottom: 0; z-index: 4; padding: 10px 16px calc(10px + env(safe-area-inset-bottom)); background: color-mix(in srgb, var(--panel) 96%, transparent); backdrop-filter: blur(6px); border-top: 1px solid var(--line); }
.prefs .savebar .bar { justify-content: flex-end; gap: 8px; }
.prefs .smsg { margin-right: auto; font-size: 13px; color: var(--muted); }
.prefs .smsg.dirty { color: var(--accent); }
.prefs .smsg.ok { color: var(--ok); } .prefs .smsg.err { color: var(--err); }
</style>
