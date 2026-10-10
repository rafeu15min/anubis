<script setup>
// Vídeos: pedidos em andamento, vitrine da aba (prontos + carrosséis do
// TMDB) e a busca. Os dados e as ações estão em lib/videos.js.
import { computed, watch, onActivated } from 'vue';
import Rail from '../components/Rail.vue';
import VideoCard from '../components/video/VideoCard.vue';
import ItemDialog from '../components/video/ItemDialog.vue';
import PickDialog from '../components/video/PickDialog.vue';
import { sections } from '../lib/sections.js';
import { V, start, showCategory, search, clearSearch, openItem, itemOf } from '../lib/videos.js';

defineOptions({ name: 'Videos' });
const S = sections.video;
const sc = computed(() => V.showcase[S.tab]);

// ---- vitrine: o que já se tem aparece na hora; passados 10 min, atualiza por trás
function load() {
  const cur = V.showcase[S.tab];
  if (!cur || Date.now() - (cur.at || 0) > 10 * 60e3) showCategory(S.tab);
}
watch(() => S.tab, load);
onActivated(() => { start(); load(); });

// ---- busca
let timer;
watch(() => S.query, (term) => {
  clearTimeout(timer);
  if (term.length < 2) { clearSearch(); return; }
  timer = setTimeout(() => search(term), 400);
});

// ---- pedidos em andamento (pronto sai daqui: aparece em "Prontos para assistir")
const tracking = computed(() => Object.entries(V.tracked)
  .filter(([k, t]) => { const st = V.status[k]; return !(st && st.state === 'available') && !(!st && t.done); })
  .sort((a, b) => (b[1].at || 0) - (a[1].at || 0))
  .map(([k]) => k));

// ---- carrosséis
const tileNone = (k, ready) => !ready && itemOf(k)?.avail === 'nothing';
function tileMeta(k, ready) {
  const it = itemOf(k);
  const badge = ready || it.available ? ['ok', 'pronto'] : it.requested ? [null, 'na lista']
    : it.avail === 'subbed' ? ['sub', 'só legendado']
    : it.avail === 'nothing' ? [null, 'sem fonte']
    : it.avail === 'unknown' ? [null, 'a conferir'] : null;
  return { year: it.year || '', badge };
}
const shelves = computed(() => {
  if (!sc.value) return [];
  const out = [];
  if (sc.value.ready.length) out.push({ title: 'Prontos para assistir', keys: sc.value.ready, ready: true });
  for (const row of sc.value.rows) if (row.keys.length) out.push({ ...row, ready: false });
  return out;
});

// resumo do que as fontes têm, no topo (cada título uma vez; fora os já pedidos)
const CATNAME = { filmes: 'Filmes', series: 'Séries', animes: 'Animes' };
function eta(s) {
  if (s < 60) return 'menos de 1 min';
  const m = Math.round(s / 60);
  if (m < 60) return `cerca de ${m} min`;
  const h = Math.floor(m / 60), r = m % 60;
  return `cerca de ${h} h${r >= 5 ? ` ${r} min` : ''}`;
}
const summary = computed(() => {
  if (!sc.value) return null;
  const seen = new Set(), n = { dubbed: 0, subbed: 0, nothing: 0, unknown: 0 };
  for (const row of sc.value.rows) for (const k of row.keys) {
    const it = itemOf(k);
    if (seen.has(k) || !it || !it.avail) continue;
    seen.add(k);
    n[it.avail]++;
  }
  const parts = [[n.dubbed, 'dublado', 'dublados'], [n.subbed, 'só legendado', 'só legendados'], [n.nothing, 'sem fonte', 'sem fonte'], [n.unknown, 'a conferir', 'a conferir']]
    .filter(([c]) => c).map(([c, one, many]) => `${c} ${c === 1 ? one : many}`);
  const p = sc.value.checking;
  if (!parts.length && !p) return null;
  return {
    line: parts.length ? parts.join(' · ') : '',
    queue: p ? `Conferindo as fontes: ${p} ${p === 1 ? 'título' : 'títulos'} na fila, ${eta(sc.value.eta)}. As etiquetas se atualizam sozinhas.` : '',
  };
});
const rowKey = (k) => k;
</script>

<template>
  <main>
    <section v-if="tracking.length">
      <h2>Pedidos em andamento</h2>
      <div class="grid"><VideoCard v-for="k in tracking" :key="k" :k="k" /></div>
    </section>

    <div v-if="V.results">
      <p v-if="V.results.loading" class="msg">Buscando…</p>
      <p v-else-if="V.results.error" class="msg err">Falhou: {{ V.results.error }}</p>
      <template v-else>
        <template v-if="V.results.movies.length"><h2>Filmes</h2><div class="grid"><VideoCard v-for="k in V.results.movies" :key="k" :k="k" /></div></template>
        <template v-if="V.results.series.length"><h2>Séries</h2><div class="grid"><VideoCard v-for="k in V.results.series" :key="k" :k="k" /></div></template>
        <p v-if="V.results.errors.length" class="msg err"><template v-for="(e, i) in V.results.errors" :key="i">{{ e }}<br></template></p>
        <p v-if="!V.results.movies.length && !V.results.series.length && !V.results.errors.length" class="msg">Nada encontrado.</p>
      </template>
    </div>

    <div v-else>
      <p v-if="!sc" class="msg">Carregando…</p>
      <template v-else>
        <p v-if="summary" class="msg summary"><template v-if="summary.line"><b>{{ CATNAME[S.tab] }}:</b> {{ summary.line }}</template><template v-if="summary.line && summary.queue"><br></template>{{ summary.queue }}</p>
        <section v-for="sh in shelves" :key="sh.title" class="shelf">
          <h2>{{ sh.title }}</h2>
          <Rail :items="sh.keys" :item-key="rowKey" v-slot="{ item: k, copy }">
            <button class="tile" :class="{ none: tileNone(k, sh.ready) }" :tabindex="copy ? -1 : null" @click="openItem(k)">
              <img v-if="itemOf(k).poster" :src="itemOf(k).poster" alt="" loading="lazy"><div v-else class="noposter"></div>
              <span class="t">{{ itemOf(k).title }}</span>
              <span class="m">{{ tileMeta(k, sh.ready).year }}<template v-if="tileMeta(k, sh.ready).badge"> · <span :class="tileMeta(k, sh.ready).badge[0]">{{ tileMeta(k, sh.ready).badge[1] }}</span></template></span>
            </button>
          </Rail>
        </section>
        <p v-if="sc.errors.length || sc.error" class="msg err"><template v-for="(e, i) in [...sc.errors, ...(sc.error ? [sc.error] : [])]" :key="i">{{ e }}<br></template></p>
        <p v-if="!shelves.length && !sc.errors.length && !sc.error" class="msg">Nada por aqui ainda.</p>
      </template>
    </div>

    <ItemDialog />
    <PickDialog />
  </main>
</template>

<style>
.shelf h2 { margin-top: 20px; }
.summary { padding: 12px 0 0; }
.summary b { font-weight: 600; }
/* sem fonte: apagado (continua clicável) */
.tile.none img, .tile.none .noposter { opacity: .35; filter: grayscale(1); }
.tile.none .t { color: var(--muted); }
</style>
