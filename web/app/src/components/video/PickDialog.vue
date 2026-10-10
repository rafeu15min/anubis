<script setup>
// "Nenhuma versão passa no perfil": as versões recusadas, para assistir uma
// mesmo assim (ou o legendado, quando só faltou a dublagem).
import { ref } from 'vue';
import { V, watchNow, retestIndexers } from '../../lib/videos.js';
import Modal from '../Modal.vue';
import Where from './Where.vue';

const chosen = ref(null);
function pick(c) { chosen.value = c.guid; watchNow(V.pick.k, c.guid); }
</script>

<template>
  <Modal :open="!!V.pick" @close="V.pick = null">
    <template v-if="V.pick">
      <h3>{{ V.pick.title }}</h3>
      <p class="meta">{{ V.pick.msg }}</p>
      <p v-if="V.pick.down.length" class="meta" style="color: var(--warn)">
        <template v-if="V.pick.downStill">Continuam fora do ar: <b>{{ V.pick.down.join(', ') }}</b>.</template>
        <template v-else>Fora do ar agora: <b>{{ V.pick.down.join(', ') }}</b> — a busca pode ter deixado versões de fora.
          <button :disabled="V.pick.retesting" @click="retestIndexers">{{ V.pick.retesting ? 'Testando (até 1–2 min)…' : 'Testar de novo' }}</button></template>
      </p>
      <div v-for="(c, i) in V.pick.candidates" :key="c.guid" class="rel">
        <div>
          <b>{{ c.title }}</b>
          <small>{{ c.sizeText }} · {{ c.seeders }} fontes<template v-if="c.playable === 0"> · H.265: talvez não toque no navegador</template></small>
          <small v-if="!V.pick.onlyDub" class="why">{{ c.reasons.join(' · ') }}</small>
        </div>
        <button :class="{ primary: V.pick.onlyDub && i === 0 }" :disabled="!!V.watching[V.pick.k]" @click="pick(c)">
          {{ V.watching[V.pick.k] && chosen === c.guid ? 'Abrindo…' : V.pick.onlyDub ? (i === 0 ? 'Assistir legendado' : 'Esta') : 'Assistir esta' }}
        </button>
      </div>
      <Where v-if="V.pick.where" :kind="V.pick.where[0]" :id="V.pick.where[1]" />
    </template>
  </Modal>
</template>

<style>
.rel { display: grid; grid-template-columns: 1fr auto; gap: 8px; align-items: center; padding: 10px 0; border-top: 1px solid var(--line); }
.rel b { font-weight: 500; word-break: break-word; }
.rel small { color: var(--muted); display: block; }
.rel .why { color: var(--warn); }
</style>
