<script setup>
// Ações em ícone, iguais em filmes, séries e episódios: ▶ sempre; ⬇ no que
// não está no servidor (🔔 se nenhuma fonte tem agora: pede e fica de olho);
// 🗑 no que está. Na série, o ▶ abre a janela com os episódios e, nela, vira
// "▶ T2·E3" (o próximo a assistir).
import { computed } from 'vue';
import { V, openItem, watchNow, request, requestEpisode, uninstall } from '../../lib/videos.js';
import { pct } from '../../lib/util.js';

const props = defineProps({
  k: String,
  available: Boolean,
  avail: String,
  requested: Boolean,
  st: Object,
  play: { type: String, default: 'watch' }, // watch | open | next | none
  next: Object,                             // play = next: { id, season, number }
  missing: Number,                          // série: episódios exibidos que faltam
});

const nk = computed(() => props.next && `episode:${props.next.id}`);
const nextLabel = computed(() => props.next && `T${props.next.season}·E${props.next.number}`);
const state = computed(() => props.st?.state);
function ask() {
  if (props.k.startsWith('episode:')) requestEpisode(+props.k.split(':')[1]);
  else request(props.k);
}
</script>

<template>
  <span class="acts">
    <button v-if="play === 'open'" class="icon primary" title="Episódios" aria-label="Episódios" @click="openItem(k)"><i class="i i-play"></i></button>
    <button v-else-if="play === 'next'" class="primary next" :disabled="!!V.watching[nk]" :title="`Assistir ${nextLabel}`" :aria-label="`Assistir ${nextLabel}`" @click="watchNow(nk)"><i class="i i-play"></i>{{ V.watching[nk] || nextLabel }}</button>
    <button v-else-if="play === 'none'" class="icon primary" disabled title="Nenhum episódio exibido ainda" aria-label="Nenhum episódio exibido ainda"><i class="i i-play"></i></button>
    <button v-else class="icon primary" :class="{ busy: V.watching[k] }" :disabled="!!V.watching[k]" title="Assistir" aria-label="Assistir" @click="watchNow(k)"><i class="i i-play"></i></button>

    <button v-if="available" class="icon danger" :class="{ busy: V.busy[k] }" :disabled="!!V.busy[k]" title="Apagar do servidor" aria-label="Apagar do servidor" @click="uninstall(k)"><i class="i i-trash"></i></button>
    <button v-else-if="state === 'downloading'" class="icon prog" disabled title="Baixando" aria-label="Baixando">{{ st.progress != null ? pct(st.progress) : '…' }}</button>
    <button v-else-if="state === 'importing'" class="icon busy" disabled title="Entrando na biblioteca" aria-label="Entrando na biblioteca"><i class="i i-download"></i></button>
    <button v-else-if="requested || state === 'searching'" class="icon quiet" disabled title="Pedido: baixa quando achar uma versão" aria-label="Pedido: baixa quando achar uma versão"><i class="i i-bell"></i></button>
    <button v-else class="icon" :class="{ busy: V.busy[k] }" :disabled="!!V.busy[k]" :title="avail === 'nothing' ? 'Pedir quando aparecer' : 'Baixar'" :aria-label="avail === 'nothing' ? 'Pedir quando aparecer' : 'Baixar'" @click="ask">
      <i class="i" :class="avail === 'nothing' ? 'i-bell' : 'i-download'"></i>
    </button>

    <button v-if="missing" class="icon" :class="{ busy: V.busy[k] }" :disabled="!!V.busy[k]" :title="`Baixar os ${missing} episódios que faltam`" :aria-label="`Baixar os ${missing} episódios que faltam`" @click="request(k)"><i class="i i-download"></i></button>
  </span>
</template>

<style>
/* ▶ da série: o episódio que vai tocar escrito ao lado */
button.primary.next { gap: 8px; padding: 8px 14px; font-variant-numeric: tabular-nums; }
</style>
