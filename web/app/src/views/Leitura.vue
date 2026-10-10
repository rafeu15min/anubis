<script setup>
// Leitura: estante do Drive (com o progresso do Kindle/kosync), "Para você",
// pedidos de download e a busca (estante + Open Library). API: src/leitura.rs.
import { ref, computed, watch, nextTick, onActivated, onDeactivated } from 'vue';
import Rail from '../components/Rail.vue';
import Modal from '../components/Modal.vue';
import { sections } from '../lib/sections.js';
import { L, loadShelf, loadJobs, busy } from '../lib/leitura.js';
import { api, pct } from '../lib/util.js';

defineOptions({ name: 'Leitura' });
const S = sections.leitura;
// as pastas do Drive de cada aba (Livros junta Ficção e Técnicos)
const FOLDERS = { livros: ['Ficção', 'Técnicos'], artigos: ['Artigos'], 'mangas-hqs': ['Mangás e HQs'] };
const ALL_FOLDERS = ['Ficção', 'Técnicos', 'Artigos', 'Mangás e HQs'];
const shelf = computed(() => L.shelves[S.tab]);

// cor da capa desenhada: estável por título (o Drive não dá capa pública)
function hue(s) { let h = 0; for (const c of s) h = (h * 31 + c.codePointAt(0)) % 360; return h; }
const coverColor = (t) => ({ '--c': `hsl(${hue(t)} 28% 24%)` });

const reading = computed(() => (shelf.value?.estante || []).filter((b) => b.progress != null && !b.lido));
const rest = computed(() => (shelf.value?.estante || []).filter((b) => !(b.progress != null && !b.lido)));
// as fileiras como as dos filmes (src/leitura_recs.rs)
const rows = computed(() => [['Para você', 'para_voce'], ['Destaques', 'destaques'], ['Lançamentos', 'lancamentos'], ['Populares no Brasil', 'populares']]
  .map(([title, k]) => ({ title, items: shelf.value?.[k] || [] }))
  .filter((r) => r.items.length));

// ---- pedidos: andamento a cada 3 s enquanto algum trabalha; um livro que
// chega ao Drive faz a estante relistar
const LABEL = { procurando: 'procurando', baixando: 'baixando', conferindo: 'conferindo', enviando_ao_drive: 'enviando ao Drive', no_drive: 'no Drive', sem_portugues: 'sem português', sem_fonte: 'sem fonte', falhou: 'falhou' };
const jobs = computed(() => L.jobs.filter((j) => (FOLDERS[S.tab] || []).includes(j.folder)).slice(0, 8));
let jobTimer, wasBusy = false, active = false;
async function pollJobs() {
  clearTimeout(jobTimer);
  await loadJobs();
  const now = busy();
  if (wasBusy && !now) loadShelf(S.tab);
  wasBusy = now;
  if (now && active) jobTimer = setTimeout(pollJobs, 3000);
}
const ingles = ref({});
async function acceptEnglish(id) {
  ingles.value[id] = true;
  try { await api(`/leitura/pedidos/${encodeURIComponent(id)}/ingles`, { method: 'POST' }); } catch {}
  pollJobs();
}

// ---- estante: o que já se tem aparece na hora; atualiza por trás. "Para
// você" ainda montando no servidor: pergunta de novo em 15 s
let retry;
async function refresh() {
  clearTimeout(retry);
  await loadShelf(S.tab);
  if ((shelf.value?.preparando || shelf.value?.montando) && active) retry = setTimeout(refresh, 10000);
}

// ---- temas: a pessoa põe e tira; as fileiras se montam de novo
const adding = ref(false), newTheme = ref(''), themeInput = ref(null), themeBusy = ref(false);
async function openAdd() { adding.value = true; await nextTick(); themeInput.value?.focus(); }
async function addTheme() {
  const nome = newTheme.value.trim();
  if (!nome) { adding.value = false; return; }
  themeBusy.value = true;
  try {
    await api(`/leitura/temas/${S.tab}`, { method: 'POST', body: { nome } });
    newTheme.value = '';
    adding.value = false;
    await refresh();
  } catch (e) { alert(`Não deu: ${e.message}`); } finally { themeBusy.value = false; }
}
async function removeTheme(nome) {
  themeBusy.value = true;
  try {
    await api(`/leitura/temas/${S.tab}/${encodeURIComponent(nome)}`, { method: 'DELETE' });
    await refresh();
  } catch (e) { alert(`Não deu: ${e.message}`); } finally { themeBusy.value = false; }
}
const ORIGEM = { config: 'área de estudo', drive: 'do que está no Drive', gosto: 'do seu gosto (o que assiste e lê)', voce: 'posto por você' };
watch(() => S.tab, () => { refresh(); });
onActivated(() => { active = true; refresh(); pollJobs(); });
onDeactivated(() => { active = false; clearTimeout(jobTimer); clearTimeout(retry); });

// ---- busca: estante (todas as pastas) + Open Library
const results = ref(null);
let qTimer, qSeq = 0;
watch(() => S.query, (term) => {
  clearTimeout(qTimer);
  qSeq++;
  if (term.length < 2) { results.value = null; return; }
  results.value = { loading: true };
  const seq = qSeq;
  qTimer = setTimeout(async () => {
    try {
      const j = await api(`/leitura/busca?q=${encodeURIComponent(term)}`);
      if (seq === qSeq) results.value = j;
    } catch (e) {
      if (seq === qSeq) results.value = { error: e.message };
    }
  }, 400);
});

// ---- cartões
const card = ref(null); // { book } | { sug, fromSearch }
const cat = ref(S.tab);
function openBook(b) { card.value = { book: b }; }
function openSug(s, fromSearch) {
  // já está no Drive: o cartão do livro (se ele é desta aba)
  const b = s.estante && (shelf.value?.estante || []).find((x) => x.path === s.estante);
  if (b) { openBook(b); return; }
  // a pasta sugerida (Livros: pelo assunto); a pessoa troca se quiser
  cat.value = s.pasta || FOLDERS[S.tab][0];
  card.value = { sug: s, fromSearch };
}
const working = ref(false);

async function download() {
  const s = card.value.sug;
  const pasta = cat.value;
  // a aba da pasta escolhida (Ficção e Técnicos são Livros)
  const categoria = Object.keys(FOLDERS).find((t) => FOLDERS[t].includes(pasta)) || S.tab;
  working.value = true;
  try {
    await api('/leitura/baixar', { method: 'POST', body: { title: s.title, author: s.author, categoria, pasta, pdf: s.pdf || null } });
    card.value = null;
    // o pedido aparece na aba da pasta escolhida
    if (categoria !== S.tab || S.query) { S.query = ''; S.tab = categoria; }
    pollJobs();
  } catch (e) {
    alert(`Não consegui pedir: ${e.message}`);
  } finally { working.value = false; }
}

async function markRead() {
  const b = card.value.book;
  working.value = true;
  try {
    await api(`/leitura/lido/${b.doc}`, { method: 'PUT', body: { lido: !b.lido } });
    card.value = null;
    loadShelf(S.tab);
  } catch (e) {
    alert(`Não consegui marcar: ${e.message}`);
  } finally { working.value = false; }
}

const bookKey = (b) => b.path;
const sugKey = (s) => s.key;
</script>

<template>
  <main>
    <template v-if="results">
      <p v-if="results.loading" class="msg">Buscando…</p>
      <p v-else-if="results.error" class="msg err">A busca falhou: {{ results.error }}</p>
      <template v-else>
        <template v-if="results.estante.length">
          <h2>Na estante</h2>
          <Rail :items="results.estante" :item-key="bookKey" v-slot="{ item, copy }">
            <button class="tile" :tabindex="copy ? -1 : null" @click="openBook(item)">
              <div class="cover" :style="coverColor(item.title)"><b>{{ item.title }}</b><small>{{ item.author }}</small><span class="ext">{{ item.ext.toUpperCase() }}</span></div>
              <span class="t">{{ item.title }}</span><span class="m">{{ item.category }}</span>
            </button>
          </Rail>
        </template>
        <template v-if="results.obras.length">
          <h2>Para baixar</h2>
          <Rail :items="results.obras" :item-key="sugKey" v-slot="{ item, copy }">
            <button class="tile" :tabindex="copy ? -1 : null" @click="openSug(item, true)">
              <img v-if="item.cover" :src="item.cover" alt="" loading="lazy">
              <div v-else class="cover" :style="coverColor(item.title)"><b>{{ item.title }}</b><small>{{ item.author }}</small></div>
              <span class="t">{{ item.title }}</span>
              <span class="m"><span v-if="item.portuguese" class="pt" title="Tem edição em português">PT</span>{{ item.author }}</span>
            </button>
          </Rail>
        </template>
        <p v-if="!results.estante.length && !results.obras.length" class="msg">Nada para “{{ S.query }}”.</p>
      </template>
    </template>

    <template v-else>
      <template v-if="jobs.length">
        <h2>Pedidos</h2>
        <div class="jobs">
          <div v-for="j in jobs" :key="j.id" class="job">
            <b>{{ j.title }}</b>
            <span class="st" :class="{ ok: j.stage === 'no_drive', bad: j.stage === 'sem_fonte' || j.stage === 'falhou' }">
              {{ LABEL[j.stage] || j.stage }}<template v-if="j.stage === 'baixando'"> {{ pct(j.progress) }}</template>
              <button v-if="j.stage === 'sem_portugues'" class="icon" :disabled="ingles[j.id]" title="Não há em português: baixar a versão em inglês" aria-label="Baixar em inglês" @click="acceptEnglish(j.id)"><i class="i i-download"></i></button>
            </span>
            <small>{{ j.detail }}</small>
          </div>
        </div>
      </template>

      <p v-if="!shelf" class="msg">Carregando…</p>
      <template v-else>
        <p v-if="shelf.error && !shelf.estante.length" class="msg err">Não consegui ler o Drive: {{ shelf.error }}</p>
        <template v-if="reading.length">
          <h2>Lendo agora</h2>
          <Rail :items="reading" :item-key="bookKey" v-slot="{ item, copy }">
            <button class="tile" :tabindex="copy ? -1 : null" @click="openBook(item)">
              <div class="cover" :style="coverColor(item.title)"><b>{{ item.title }}</b><small>{{ item.author }}</small><span class="ext">{{ item.ext.toUpperCase() }}</span></div>
              <div class="bar-prog"><i :style="{ width: pct(item.progress) }"></i></div>
              <span class="t">{{ item.title }}</span><span class="m">{{ pct(item.progress) }}</span>
            </button>
          </Rail>
        </template>
        <div class="perfil">
          <span v-for="a in shelf.temas || []" :key="a.nome" class="chip" :class="a.origem" :title="`${a.nome} — ${ORIGEM[a.origem] || ''}`">
            {{ a.nome }}<button class="chip-x" type="button" :disabled="themeBusy" :title="`Tirar o tema ${a.nome}`" :aria-label="`Tirar o tema ${a.nome}`" @click="removeTheme(a.nome)"><i class="i i-x"></i></button>
          </span>
          <form v-if="adding" class="chip-add" @submit.prevent="addTheme">
            <input ref="themeInput" v-model="newTheme" type="text" maxlength="60" placeholder="Novo tema" aria-label="Novo tema" @keydown.esc="adding = false" @blur="!newTheme && (adding = false)">
          </form>
          <button v-else class="chip-plus" type="button" :disabled="themeBusy" title="Adicionar tema de busca" aria-label="Adicionar tema de busca" @click="openAdd"><i class="i i-plus"></i></button>
          <span v-if="shelf.montando" class="meta">montando as fileiras de novo…</span>
        </div>
        <p v-if="shelf.preparando && !rows.length" class="msg">Montando Para você, Destaques, Lançamentos e Populares no Brasil (o primeiro leva alguns minutos)…</p>
        <template v-for="r in rows" :key="r.title">
          <h2>{{ r.title }}</h2>
          <Rail :items="r.items" :item-key="sugKey" v-slot="{ item, copy }">
            <button class="tile" :tabindex="copy ? -1 : null" @click="openSug(item, false)">
              <img v-if="item.cover" :src="item.cover" alt="" loading="lazy">
              <div v-else class="cover" :style="coverColor(item.title)"><b>{{ item.title }}</b><small>{{ item.author }}</small></div>
              <span class="t">{{ item.title }}</span>
              <span class="m"><span v-if="item.estante" class="ok">na estante · </span><span v-else-if="item.portuguese" class="pt" title="Tem edição em português">PT</span>{{ item.author }}</span>
            </button>
          </Rail>
        </template>
        <template v-if="rest.length">
          <h2>Na estante</h2>
          <Rail :items="rest" :item-key="bookKey" v-slot="{ item, copy }">
            <button class="tile" :tabindex="copy ? -1 : null" @click="openBook(item)">
              <div class="cover" :style="coverColor(item.title)"><b>{{ item.title }}</b><small>{{ item.author }}</small><span class="ext">{{ item.ext.toUpperCase() }}</span></div>
              <span class="t">{{ item.title }}</span>
              <span class="m"><span v-if="item.lido" class="ok"><i class="i i-check"></i> lido</span><template v-else>{{ item.author }}</template></span>
            </button>
          </Rail>
        </template>
        <p v-if="!shelf.error && !reading.length && !rest.length && !rows.length && !shelf.preparando" class="msg">Nada aqui ainda.</p>
      </template>
    </template>

    <Modal :open="!!card" @close="card = null">
      <template v-if="card?.book">
        <h3>{{ card.book.title }}</h3>
        <p class="meta">{{ card.book.author || 'autor desconhecido' }} · {{ card.book.ext.toUpperCase() }} · {{ (card.book.size / 1048576).toFixed(1) }} MB</p>
        <p>{{ card.book.lido ? 'Lido.' : card.book.progress == null ? 'Ainda não aberto no Kindle.' : `Em ${pct(card.book.progress)} (sincronizado pelo KOReader).` }}</p>
        <p class="meta">{{ card.book.path }}</p>
        <div class="acts">
          <a class="icon" :href="`https://drive.google.com/file/d/${encodeURIComponent(card.book.drive_id)}/view`" target="_blank" rel="noopener" title="Abrir no Drive" aria-label="Abrir no Drive"><i class="i i-cloud"></i></a>
          <button class="icon" :class="{ on: card.book.lido, busy: working }" :disabled="working" :aria-pressed="card.book.lido"
            :title="card.book.lido ? 'Lido (toque para desmarcar; vale no Kindle também)' : 'Marcar como lido (vale no Kindle também)'"
            :aria-label="card.book.lido ? 'Desmarcar lido' : 'Marcar como lido'" @click="markRead"><i class="i i-check"></i></button>
        </div>
      </template>
      <template v-else-if="card?.sug">
        <h3>{{ card.sug.title }}</h3>
        <p class="meta">{{ card.sug.author }}<template v-if="card.sug.year"> · {{ card.sug.year }}</template></p>
        <p v-if="card.sug.reason">{{ card.sug.reason }}.</p>
        <p v-if="card.sug.estante" class="meta">Já está na estante: {{ card.sug.estante }}</p>
        <p v-else-if="card.sug.key.startsWith('oa:')" class="meta">{{ card.sug.pdf ? 'Acesso livre: o PDF vai direto para o Drive.' : 'Sem PDF direto: abra na fonte.' }}</p>
        <p v-else class="meta">{{ card.sug.portuguese ? 'Tem edição em português.' : 'Sem edição em português conhecida (a busca tenta mesmo assim).' }}</p>
        <div class="acts">
          <select v-if="card.fromSearch || S.tab === 'livros'" v-model="cat" title="Pasta no Drive" aria-label="Pasta no Drive">
            <option v-for="f in (card.fromSearch ? ALL_FOLDERS : FOLDERS[S.tab])" :key="f" :value="f">{{ f }}</option>
          </select>
          <button v-if="!card.sug.estante && (!card.sug.key.startsWith('oa:') || card.sug.pdf)" class="icon" :class="{ busy: working }" :disabled="working" title="Baixar para o Drive (português primeiro)" aria-label="Baixar" @click="download"><i class="i i-download"></i></button>
          <a v-if="card.sug.link" class="icon" :href="card.sug.link" target="_blank" rel="noopener" title="Ver na fonte" aria-label="Ver na fonte"><i class="i i-book"></i></a>
        </div>
      </template>
    </Modal>
  </main>
</template>

<style>
.jobs { display: grid; gap: 8px; }
.job { display: grid; grid-template-columns: 1fr auto; gap: 4px 12px; align-items: center; padding: 10px 12px; border: 1px solid var(--line); border-radius: 10px; background: var(--panel); }
.job small { color: var(--muted); grid-column: 1 / -1; }
.job .st { display: inline-flex; align-items: center; gap: 8px; font-size: 13px; color: var(--info); }
.job .st.ok { color: var(--ok); } .job .st.bad { color: var(--err); }
/* capa desenhada: o Drive não dá capa pública */
.cover { position: relative; width: 100%; aspect-ratio: 2/3; border-radius: 8px; padding: 10px; display: flex; flex-direction: column; justify-content: space-between; overflow: hidden; background: var(--c, #2a2733); }
.cover b { font-size: 14px; line-height: 1.2; display: -webkit-box; -webkit-line-clamp: 6; -webkit-box-orient: vertical; overflow: hidden; }
.cover small { font-size: 11px; opacity: .8; }
.cover .ext { position: absolute; right: 8px; bottom: 8px; font-size: 10px; letter-spacing: .08em; opacity: .7; }
.perfil { display: flex; flex-wrap: wrap; gap: 6px; margin: 8px 0 0; }
.chip { display: inline-flex; align-items: center; gap: 2px; font-size: 12px; padding: 2px 2px 2px 8px; border-radius: 99px; border: 1px solid color-mix(in srgb, var(--accent) 45%, transparent); color: var(--accent); }
.chip.voce { background: color-mix(in srgb, var(--accent) 18%, transparent); }
.chip.drive, .chip.gosto { border-style: dashed; color: var(--muted); border-color: var(--line); }
.chip-x { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; min-height: 0; padding: 0; border: 0; border-radius: 50%; background: none; color: inherit; font-size: 10px; opacity: .55; }
.chip-x:hover { opacity: 1; background: color-mix(in srgb, var(--accent) 20%, transparent); }
.chip-plus { display: inline-flex; align-items: center; justify-content: center; width: 26px; height: 26px; min-height: 0; padding: 0; border-radius: 50%; border: 1px dashed color-mix(in srgb, var(--accent) 60%, transparent); font-size: 12px; }
.chip-add input { width: 170px; min-height: 26px; padding: 2px 10px; border-radius: 99px; border: 1px solid var(--accent); background: var(--bg); color: var(--text); font: inherit; font-size: 12px; }
.chip-add input:focus { outline: none; }
.pt { display: inline-block; font-size: 10px; padding: 0 5px; margin-right: 4px; border: 1px solid color-mix(in srgb, var(--ok) 50%, transparent); border-radius: 99px; color: var(--ok); }
.bar-prog { height: 4px; border-radius: 2px; background: var(--line); overflow: hidden; }
.bar-prog i { display: block; height: 100%; background: var(--info); }
select { min-height: 44px; padding: 0 10px; border-radius: 8px; border: 1px solid var(--line); background: var(--bg); color: var(--text); font: inherit; }
</style>
