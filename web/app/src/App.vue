<script setup>
// Casca do app: o cabeçalho de Vídeos e Leitura fica sempre montado (a marca
// com o seletor anima de verdade, sem recarregar) e só a tela troca. As duas
// seções ficam guardadas em memória (KeepAlive): voltar a uma é instantâneo.
import { computed, onMounted } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import Marca from './components/Marca.vue';
import { sections, pickTab } from './lib/sections.js';
import { prefetch as prefetchVideos, setNavigate } from './lib/videos.js';
import { prefetch as prefetchLeitura } from './lib/leitura.js';

const route = useRoute();
const router = useRouter();
const section = computed(() => route.meta.section);
setNavigate((path) => router.push(path));
const S = computed(() => sections[section.value]);

function toggle() {
  router.push(section.value === 'leitura' ? '/' : '/leitura');
}

// a outra seção já fica com os dados prontos (a primeira troca também não carrega)
onMounted(() => {
  const idle = window.requestIdleCallback || ((f) => setTimeout(f, 1500));
  idle(() => { prefetchVideos(); prefetchLeitura(); });
});
</script>

<template>
  <header v-if="section" class="top">
    <div class="bar">
      <Marca :leitura="section === 'leitura'" @toggle="toggle" />
      <Transition name="cabec" mode="out-in">
        <nav class="tabs" :key="section">
          <button v-for="[slug, label] in S.tabs" :key="slug" :class="{ on: S.tab === slug }" @click="pickTab(section, slug)">{{ label }}</button>
        </nav>
      </Transition>
      <input v-model.trim="S.query" type="search" :placeholder="S.placeholder" autocomplete="off" :aria-label="S.placeholder">
      <Transition name="cabec">
        <RouterLink v-if="section === 'video'" class="gear" to="/preferencias" title="Preferências" aria-label="Preferências"><i class="i i-gear"></i></RouterLink>
      </Transition>
    </div>
  </header>
  <RouterView v-slot="{ Component, route: r }">
    <Transition name="tela" mode="out-in">
      <KeepAlive include="Videos,Leitura">
        <component :is="Component" :key="r.meta.section || r.fullPath" />
      </KeepAlive>
    </Transition>
  </RouterView>
</template>
