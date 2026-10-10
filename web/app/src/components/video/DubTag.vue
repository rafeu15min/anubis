<script setup>
// 🎙 entre os selos: está no servidor sem português; um toque procura a dublagem
import { V, dubNow } from '../../lib/videos.js';
defineProps({ k: String, label: { type: String, default: 'sem dublagem' } });
</script>

<template>
  <button class="tag dub" :disabled="!!V.dub[k]" :title="V.dub[k]?.title || 'Procurar dublagem agora'" :aria-label="V.dub[k]?.title || 'Sem dublagem: procurar agora'" @click="dubNow(k)">
    <i class="i i-microphone"></i>{{ V.dub[k]?.label || label }}
  </button>
</template>
