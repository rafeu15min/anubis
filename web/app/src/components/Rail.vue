<script setup>
// Carrossel em anel. Fileira maior que a tela: depois do último item vêm
// cópias do começo (o bastante para uma tela), então a última página se
// completa com o que vem "à frente" e a seta da direita nunca acaba. Parada a
// rolagem dentro das cópias, a posição vira a mesma nos originais (a mesma
// imagem, sem salto). Fileira que cabe inteira não gira nem tem setas
// (repetir os mesmos itens lado a lado parece defeito). A seta da esquerda só
// aparece depois de andar: do começo não se volta para o fim.
//
// <Rail :items="lista" :item-key="(x) => x.id" v-slot="{ item, copy }">…</Rail>
// `copy`: é uma cópia (fora da ordem do Tab).
import { ref, computed, watch, nextTick, onMounted, onBeforeUnmount, onActivated } from 'vue';

const props = defineProps({ items: { type: Array, required: true }, itemKey: { type: Function, required: true } });
const rail = ref(null);
const lap = ref(0);         // uma volta inteira, em px
const copies = ref(0);
const fits = ref(true);
const moved = ref(false);
const turning = ref(false); // rolando por uma seta

const copyItems = computed(() => Array.from({ length: copies.value }, (_, i) => props.items[i % props.items.length]));

function measure() {
  const el = rail.value;
  if (!el || !props.items.length || !el.clientWidth) return;
  const [a, b] = el.children;
  const gap = parseFloat(getComputedStyle(el).columnGap) || 0;
  const step = b && !b.classList.contains('copia') ? b.getBoundingClientRect().left - a.getBoundingClientRect().left : a.offsetWidth + gap;
  lap.value = props.items.length * step;
  fits.value = lap.value - gap <= el.clientWidth + 1;
  copies.value = fits.value ? 0 : Math.ceil(el.clientWidth / step) + 1;
}

// espera a rolagem parar (o scrollend nem sempre vem na hora certa)
const still = (el) => new Promise((done) => {
  const t0 = performance.now();
  let last = NaN, n = 0;
  const tick = () => {
    const now = el.scrollLeft;
    n = Math.abs(now - last) < 0.5 ? n + 1 : 0;
    last = now;
    if (n >= 3 || performance.now() - t0 > 2000) done();
    else requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
});

// dentro das cópias: a mesma posição nos originais
function settle() {
  const el = rail.value;
  if (el && lap.value && copies.value && el.scrollLeft >= lap.value - 1) {
    el.classList.add('girando');
    el.scrollTo({ left: el.scrollLeft - lap.value, behavior: 'instant' });
    requestAnimationFrame(() => el.classList.remove('girando'));
  }
}

async function step(dir) {
  const el = rail.value;
  if (!el || turning.value) return;
  turning.value = true;
  el.classList.add('girando');
  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  el.scrollBy({ left: dir * el.clientWidth * 0.9, behavior: reduce ? 'auto' : 'smooth' });
  await still(el);
  el.classList.remove('girando');
  turning.value = false;
  settle();
}

// rolagem à mão (touchpad, dedo): ao parar, volta aos originais
let idle;
function onScroll() {
  moved.value = rail.value.scrollLeft > 4;
  if (turning.value) return;
  clearTimeout(idle);
  idle = setTimeout(settle, 160);
}

let ro;
onMounted(() => {
  ro = new ResizeObserver(() => measure());
  ro.observe(rail.value);
  nextTick(measure);
});
onActivated(() => nextTick(measure));
onBeforeUnmount(() => { ro?.disconnect(); clearTimeout(idle); });
watch(() => props.items.map(props.itemKey).join('\u0000'), () => nextTick(measure));
</script>

<template>
  <div class="railbox" :class="{ andou: moved, cabe: fits }">
    <button class="nav prev" type="button" aria-label="Voltar" @click="step(-1)"><i class="i i-left"></i></button>
    <div ref="rail" class="rail" @scroll.passive="onScroll">
      <template v-for="item in items" :key="itemKey(item)"><slot :item="item" :copy="false" /></template>
      <div v-for="(item, i) in copyItems" :key="'copia' + i" class="copia" aria-hidden="true"><slot :item="item" :copy="true" /></div>
    </div>
    <button class="nav next" type="button" aria-label="Avançar" @click="step(1)"><i class="i i-right"></i></button>
  </div>
</template>
