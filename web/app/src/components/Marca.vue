<script setup>
// A marca do Anubis com o seletor Vídeos ↔ Leitura: o botão desliza para o
// lado escolhido, que ganha a cor dele (dourado = vídeos, azul = leitura); o
// outro lado fica cinza. É um link de verdade: clique do meio abre outra aba.
const props = defineProps({ leitura: Boolean });
const emit = defineEmits(['toggle']);
const label = () => (props.leitura ? 'Ir para filmes, séries e animes' : 'Ir para a leitura: livros, HQs, RPG');
function click(e) {
  if (e.ctrlKey || e.metaKey || e.shiftKey || e.button) return;
  e.preventDefault();
  emit('toggle');
}
</script>

<template>
  <a class="marca" :class="{ leitura }" :href="leitura ? '/' : '/leitura'" role="switch" :aria-checked="leitura" :title="label()" :aria-label="label()" @click="click">
    <span class="nome">ANUBIS</span>
    <span class="trilho"><span class="botao"></span><i class="i i-film"></i><i class="i i-book"></i></span>
  </a>
</template>

<style>
.marca { --cor: var(--video); display: inline-flex; flex: none; align-items: center; gap: 10px; min-height: 44px; color: var(--cor); text-decoration: none; transition: color var(--troca) ease; }
.marca.leitura { --cor: var(--leitura); }
.marca .nome { font-size: 18px; font-weight: 700; letter-spacing: .08em; }
.marca .trilho { position: relative; display: inline-flex; width: 64px; height: 32px; border-radius: 99px; border: 1px solid color-mix(in srgb, var(--cor) 45%, var(--line)); background: var(--panel); transition: border-color var(--troca) ease; }
.marca .trilho .i { position: relative; z-index: 1; flex: 1; display: inline-flex; align-items: center; justify-content: center; font-size: 14px; color: var(--cinza); transition: color var(--troca) ease; }
.marca:not(.leitura) .i-film { color: var(--video); }
.marca.leitura .i-book { color: var(--leitura); }
.marca .botao { position: absolute; top: 2px; left: 2px; width: 26px; height: 26px; border-radius: 50%;
  background: color-mix(in srgb, var(--cor) 28%, transparent); box-shadow: 0 0 10px color-mix(in srgb, var(--cor) 45%, transparent);
  transition: transform var(--troca) cubic-bezier(.65, 0, .35, 1), background-color var(--troca) ease, box-shadow var(--troca) ease; }
.marca.leitura .botao { transform: translateX(32px); }
.marca:hover .trilho, .marca:focus-visible .trilho { border-color: var(--cor); }
.marca:focus-visible { outline: none; }
</style>
