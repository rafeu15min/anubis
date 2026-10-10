<script setup>
// Cartão de um filme ou série (busca, pedidos e a janela do título).
import { computed } from 'vue';
import { V, itemOf, seriesInfo } from '../../lib/videos.js';
import Actions from './Actions.vue';
import StatusLine from './StatusLine.vue';
import DubTag from './DubTag.vue';

const props = defineProps({ k: String, inDialog: Boolean });
const it = computed(() => itemOf(props.k));
const st = computed(() => V.status[props.k]);
const available = computed(() => (st.value && st.value.state !== 'not_requested' ? st.value.state === 'available' || !!it.value.available : !!it.value.available));
const requested = computed(() => !!(it.value.requested || V.tracked[props.k]) && !available.value);
// na janela da série, com os episódios carregados: as ações da série
const info = computed(() => (props.inDialog && it.value.kind === 'series' ? seriesInfo() : null));
const tags = computed(() => [it.value.kind === 'movie' ? 'filme' : it.value.anime ? 'anime' : 'série', ...(it.value.genres || []).slice(0, 2)]);
const noSource = computed(() => it.value.avail === 'nothing' && !it.value.requested && !V.tracked[props.k] && !it.value.available);
const dub = computed(() => {
  if (V.dub[props.k]) return 'sem dublagem';
  if (info.value && info.value.noPt > 0) return info.value.noPt === 1 ? '1 episódio sem dublagem' : `${info.value.noPt} episódios sem dublagem`;
  if (it.value.kind === 'movie' && st.value && st.value.state === 'available' && st.value.portuguese === false) return 'sem dublagem';
  return null;
});
</script>

<template>
  <article v-if="it" class="card">
    <img v-if="it.poster" :src="it.poster" alt="" loading="lazy">
    <div v-else class="noposter"></div>
    <div>
      <h3>{{ it.title }}<span v-if="it.year" class="meta"> ({{ it.year }})</span></h3>
      <div class="row">
        <span v-for="t in tags" :key="t" class="tag">{{ t }}</span>
        <span v-if="it.avail === 'subbed'" class="tag sub" title="Os indexadores só têm no idioma original, com legenda">somente legendado</span>
        <span v-else-if="it.avail === 'nothing'" class="tag">sem fonte</span>
        <span v-else-if="it.avail === 'unknown'" class="tag">{{ it.uncertain ? 'não deu para conferir agora — tento de novo em 1 h' : 'conferindo as fontes…' }}</span>
        <span v-if="it.available" class="tag">na biblioteca</span>
        <DubTag v-if="dub" :k="k" :label="dub" />
      </div>
      <p class="ov">{{ it.overview }}</p>
      <div class="row">
        <Actions v-if="info" :k="k" :available="info.have > 0" :avail="it.avail" :requested="!(info.have > 0) && !!(it.requested || V.tracked[k])" :st="st"
          :play="info.next ? 'next' : 'none'" :next="info.next" :missing="info.have > 0 && !(st && ['downloading', 'importing'].includes(st.state)) ? info.missing : 0" />
        <Actions v-else :k="k" :available="available" :avail="it.avail" :requested="requested" :st="st" :play="it.kind === 'movie' ? 'watch' : 'open'" />
        <small v-if="noSource" class="meta wide">Nenhuma das fontes conferidas tem este título agora. Pedindo (🔔), o {{ it.kind === 'movie' ? 'Radarr' : 'Sonarr' }} fica de olho e baixa quando ele aparecer.</small>
      </div>
      <div class="row status">
        <span v-if="V.watching[k]" class="state">{{ V.watching[k] }}</span>
        <StatusLine v-else :k="k" :st="st" />
      </div>
    </div>
  </article>
</template>

<style>
.card { display: grid; grid-template-columns: 92px 1fr; gap: 12px; padding: 10px; border: 1px solid var(--line); border-radius: 12px; background: var(--panel); }
.card img, .noposter { width: 92px; aspect-ratio: 2/3; object-fit: cover; border-radius: 8px; background: var(--line); }
.card h3 { margin: 0 0 2px; font-size: 16px; }
.ov { margin: 6px 0; font-size: 13px; color: #c9c5d3; display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden; }
@media (max-width: 480px) { .card { grid-template-columns: 72px 1fr; } .card img, .noposter { width: 72px; } }
</style>
