<script setup>
// Temporadas e episódios da série aberta, com o que as fontes têm de cada
// episódio que falta ("só legendado", "sem fonte"); a temporada ganha o selo
// quando ele vale para todos os que faltam nela.
import { ref, watch } from 'vue';
import { V, aired } from '../../lib/videos.js';
import Actions from './Actions.vue';
import DubTag from './DubTag.vue';

const AVAIL = { subbed: ['sub', 'só legendado'], nothing: ['none', 'sem fonte'] };
// temporadas abertas (a primeira, de início); continuam abertas ao recarregar
const opened = ref(new Set());
watch(() => V.series?.k, () => { opened.value = new Set(); });
watch(() => V.series?.seasons, (s) => { if (s && s.length && !opened.value.size) opened.value.add(s[0].number); });
function toggle(n, e) { if (e.target.open) opened.value.add(n); else opened.value.delete(n); }

const avail = (e) => (e.has_file || !aired(e) ? null : V.series.epAvail[e.id] || null);
function seasonTag(x) {
  const states = x.episodes.filter((e) => !e.has_file && aired(e)).map((e) => V.series.epAvail[e.id]).filter(Boolean);
  const all = states.length && states.every((a) => a === states[0]) ? states[0] : null;
  return all && AVAIL[all];
}
// ⬇ que ninguém tem vira 🔔 (pede e fica de olho)
const epAvail = (e) => (V.series.epAvail[e.id] === 'nothing' ? 'nothing' : e.avail);
const epSt = (e) => V.status[`episode:${e.id}`] || (e.downloading != null ? { state: e.downloading >= 1 ? 'importing' : 'downloading', progress: e.downloading } : e.st);
// o que o usuário do Jellyfin já viu: assistido, ou parado no meio (a fatia mostra quanto)
const slice = (p) => `i-s${Math.min(8, Math.max(1, Math.round(p / 12.5)))}`;
</script>

<template>
  <div v-if="V.series" class="eps">
    <p v-if="V.series.error" class="msg err">Falhou: {{ V.series.error }}</p>
    <p v-else-if="!V.series.seasons" class="msg">Carregando episódios…</p>
    <template v-else>
      <p v-if="V.series.note" class="epscheck">{{ V.series.note }}</p>
      <details v-for="x in V.series.seasons" :key="x.number" :open="opened.has(x.number)" @toggle="toggle(x.number, $event)">
        <summary>{{ x.number === 0 ? 'Especiais' : `Temporada ${x.number}` }} <span class="meta">· {{ x.episodes.length }} episódios</span>
          <span class="tags"><span v-if="seasonTag(x)" class="tag" :class="seasonTag(x)[0]">{{ seasonTag(x)[1] }}</span></span></summary>
        <div v-for="e in x.episodes" :key="e.id" class="ep">
          <span class="n">{{ e.number }}</span>
          <div>
            {{ e.title || 'Episódio ' + e.number }}
            <span class="tags">
              <span v-if="e.has_file" class="tag">na biblioteca</span>
              <DubTag v-if="e.has_file && e.portuguese === false" :k="`episode:${e.id}`" />
              <template v-if="e.watch">
                <span v-if="e.watch.percent" class="tag watching" :title="e.watch.source ? `Visto na ${e.watch.source}` : null"><i class="i" :class="slice(e.watch.percent)"></i>assistindo {{ e.watch.percent }}%<template v-if="e.watch.source"> · {{ e.watch.source }}</template></span>
                <span v-else class="tag seen" :title="e.watch.source ? `Visto na ${e.watch.source}` : null"><i class="i i-check"></i>assistido<template v-if="e.watch.source"> · {{ e.watch.source }}</template></span>
              </template>
              <span v-if="avail(e) && AVAIL[avail(e)]" class="tag" :class="AVAIL[avail(e)][0]">{{ AVAIL[avail(e)][1] }}</span>
            </span>
            <small>{{ e.air_date || '' }}</small>
            <small v-if="V.watching[`episode:${e.id}`]" class="st">{{ V.watching[`episode:${e.id}`] }}</small>
          </div>
          <span v-if="!aired(e)" class="meta">estreia {{ e.air_date }}</span>
          <Actions v-else :k="`episode:${e.id}`" :available="e.has_file" :avail="epAvail(e)" :st="epSt(e)" />
        </div>
      </details>
      <p v-if="!V.series.seasons.length" class="msg">Nenhum episódio.</p>
    </template>
  </div>
</template>

<style>
.eps { margin-top: 10px; }
.epscheck { color: var(--muted); font-size: 13px; margin: 0 0 6px; }
.eps details { border-top: 1px solid var(--line); padding: 8px 0; }
.eps summary { cursor: pointer; font-weight: 600; padding: 6px 0; }
.eps summary .tags { display: inline-flex; gap: 4px; margin-left: 6px; vertical-align: 1px; }
.ep { display: grid; grid-template-columns: 3.2em 1fr auto; gap: 10px; align-items: center; padding: 6px 0; }
.ep .n { color: var(--muted); font-variant-numeric: tabular-nums; }
.ep .tags { display: inline-flex; flex-wrap: wrap; gap: 4px; vertical-align: 1px; margin-left: 4px; }
.ep small { color: var(--muted); display: block; }
.ep small.st { color: var(--info); }
</style>
