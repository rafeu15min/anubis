<script setup>
// Onde o título passa oficialmente no Brasil (sem [tmdb] na config, some)
import { ref, watch } from 'vue';
import { api } from '../../lib/util.js';

const props = defineProps({ kind: String, id: [Number, String] });
const d = ref(null);
watch(() => [props.kind, props.id], async ([kind, id]) => {
  d.value = null;
  if (!id) return;
  try { d.value = await api(`/where/${kind}/${id}`); } catch {}
}, { immediate: true });
</script>

<template>
  <template v-if="d">
    <h4>Onde assistir no Brasil</h4>
    <template v-if="d.offers.length">
      <div class="where">
        <template v-for="o in d.offers" :key="o.kind">
          <small class="meta">{{ o.kind }}</small>
          <div class="prov"><span v-for="p in o.providers" :key="p.name"><img v-if="p.logo" :src="p.logo" alt="">{{ p.name }}</span></div>
        </template>
      </div>
      <p class="meta">O TMDB não informa o idioma do áudio; nos serviços brasileiros a dublagem costuma estar disponível.
        <template v-if="d.link"><a :href="d.link" target="_blank" rel="noopener">Links para cada serviço</a> · </template>dados do JustWatch via TMDB</p>
    </template>
    <p v-else class="meta">Não está em nenhum serviço de streaming no Brasil agora.</p>
  </template>
</template>

<style>
.where { display: grid; grid-template-columns: auto 1fr; gap: 6px 12px; align-items: center; padding: 6px 0; }
.where .prov { display: flex; flex-wrap: wrap; gap: 6px; }
.where .prov span { display: inline-flex; align-items: center; gap: 6px; padding: 3px 8px 3px 3px; border: 1px solid var(--line); border-radius: 8px; font-size: 13px; }
.where img { width: 26px; height: 26px; border-radius: 6px; }
</style>
