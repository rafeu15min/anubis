<script setup>
// O andamento de um título no servidor: estado, barra, dublagem embutindo,
// e a oferta do legendado quando não há versão dublada.
import { computed } from 'vue';
import { V, acceptSubbed } from '../../lib/videos.js';
import { pct } from '../../lib/util.js';

const props = defineProps({ k: String, st: Object });

const STATE = { not_requested: 'Ainda não pedido', searching: 'Procurando uma versão compatível…', downloading: 'Baixando', importing: 'Entrando na biblioteca…', available: 'Disponível' };
// a dublagem automática (ou um pedido avulso pela API) embutindo o áudio PT-BR
const MUX = { downloading: 'Dublagem: baixando', checking: 'Dublagem: conferindo a sincronia…', joining: 'Dublagem: embutindo…', importing: 'Dublagem: entrando na biblioteca…', done: 'Dublagem embutida', failed: 'Não deu para embutir a dublagem' };

// pedido parado em "procurando": o que os indexadores têm e por que foi recusado
function versionsText(v) {
  let t;
  if (v.approved > 0) t = 'Versão aceita encontrada — o download deve começar em instantes';
  else if (v.found === 0) t = 'Nenhuma versão nos indexadores agora';
  else {
    const why = v.reasons.map(([r, n]) => `${n} ${r}`);
    if (v.no_seeders) why.push(`${v.no_seeders} sem fontes`);
    t = `Nenhuma versão aceita: ${v.found} encontradas — ${why.join(', ')}`;
  }
  return v.approved > 0 || !v.indexers_down?.length ? t : `${t} · fora do ar: ${v.indexers_down.join(', ')}`;
}

const mux = computed(() => (props.st?.mux && props.st.mux.step !== 'done' ? props.st.mux : null));
const muxText = computed(() => {
  const m = mux.value;
  let t = MUX[m.step] || m.step;
  if (m.step === 'downloading') t += m.auto ? ` ${pct(m.audio.progress)}` : ` · vídeo ${pct(m.video.progress)} · dublagem ${pct(m.audio.progress)}`;
  if (m.sync && m.step !== 'failed') {
    // positivo: a dublagem começava atrasada (o Anubis corta esse início)
    t += Math.abs(m.sync.offset) < 0.05 ? ' · já em sincronia'
      : ` · dublagem ${m.sync.offset > 0 ? 'atrasada' : 'adiantada'} ${Math.abs(m.sync.offset).toFixed(2)} s, corrigida`;
    if (Math.abs(m.sync.speed - 1) > 1e-6) t += ', velocidade ajustada';
  }
  return t;
});
const muxDone = computed(() => (mux.value.auto ? mux.value.audio.progress : (mux.value.video.progress + mux.value.audio.progress) / 2));

const text = computed(() => {
  const st = props.st;
  let t = STATE[st.state] || st.state;
  if (st.state === 'searching' && st.versions) t = versionsText(st.versions);
  if (st.state === 'downloading' && st.progress != null) t += ` ${pct(st.progress)}` + (st.eta ? ` · falta ${st.eta}` : '');
  if (st.episodes) t += ` · ${st.episodes[0]}/${st.episodes[1]} episódios`;
  return t;
});
</script>

<template>
  <template v-if="mux">
    <span class="state">{{ muxText }}</span>
    <div v-if="mux.step === 'downloading'" class="progress"><i :style="{ width: (muxDone * 100).toFixed(1) + '%' }"></i></div>
    <small v-if="mux.error" class="err">{{ mux.error }}</small>
  </template>
  <template v-else-if="st && st.state !== 'available'">
    <span class="state">{{ text }}</span>
    <div v-if="st.progress != null" class="progress"><i :style="{ width: (st.progress * 100).toFixed(1) + '%' }"></i></div>
    <small v-if="st.note" class="meta wide">{{ st.note }}</small>
    <template v-if="st.offerSubbed">
      <span class="state wide" style="color: var(--warn)">Dublagem não encontrada em nenhuma fonte.</span>
      <button :disabled="V.busy['subbed:' + k]" @click="acceptSubbed(k)">{{ V.busy['subbed:' + k] ? 'Instalando…' : 'Instalar legendado' }}</button>
      <small class="meta wide">Baixa a versão legendada agora e troca pela dublada se ela surgir.</small>
    </template>
  </template>
</template>

<style>
.wide { flex-basis: 100%; }
</style>
