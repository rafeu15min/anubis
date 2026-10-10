<script setup>
// Fila de prioridade: a primeira é a preferida; ↑ ↓ mudam a ordem, ✕ tira,
// "Adicionar" põe no fim. Qualidade tem alvo; legendas, não. Fila longa:
// as primeiras e um botão para o resto (o fim da fila de qualidade é de
// definição padrão, quase nunca usado).
import { ref, computed, nextTick } from 'vue';

const props = defineProps({
  list: Array,             // editada aqui mesmo (é a do estado da tela)
  others: Array,           // fora da fila (para "Adicionar")
  name: Function,
  hint: { type: Function, default: () => '' },
  goal: String,            // o alvo (só qualidade)
  addLabel: String,
});
const emit = defineEmits(['change', 'drop', 'add']);
const SHORT = 6;
const open = ref(false);
const cut = computed(() => (props.goal ? props.list.indexOf(props.goal) : -1));
const shown = computed(() => (open.value || props.list.length <= SHORT + 1 ? props.list.length : Math.max(SHORT, cut.value + 1)));
const more = computed(() => props.list.length - shown.value);
const box = ref(null);

async function move(i, by) {
  const l = props.list, j = i + by;
  if (j < 0 || j >= l.length) return;
  [l[i], l[j]] = [l[j], l[i]];
  emit('change');
  // o foco segue o item movido (dá para subir várias posições seguidas)
  await nextTick();
  const b = box.value?.querySelector(`li[data-i="${j}"] [data-move="${by}"]`);
  if (b && !b.disabled) b.focus();
}
function drop(i) {
  if (props.list.length < 2) return;
  const [x] = props.list.splice(i, 1);
  emit('drop', x, i);
  emit('change');
}
function add(e) {
  const x = e.target.value;
  e.target.value = '';
  if (!x) return;
  props.list.push(x);
  open.value = true; // o novo vai para o fim: a fila aparece inteira
  emit('add', x);
  emit('change');
}
</script>

<template>
  <div ref="box">
    <ol class="queue">
      <li v-for="(x, i) in list.slice(0, shown)" :key="x" :data-i="i" :class="{ goal: i === cut }">
        <span class="pos">{{ i + 1 }}</span>
        <span class="what">{{ name(x) }}<span v-if="i === cut" class="qtag"><i class="i i-goal"></i>alvo</span><small v-if="hint(x)">{{ hint(x) }}</small></span>
        <span class="moves">
          <button class="ic" data-move="-1" :disabled="i === 0" title="Subir" :aria-label="`Subir ${name(x)}`" @click="move(i, -1)"><i class="i i-arrow-up"></i></button>
          <button class="ic" data-move="1" :disabled="i === list.length - 1" title="Descer" :aria-label="`Descer ${name(x)}`" @click="move(i, 1)"><i class="i i-arrow-down"></i></button>
          <button class="ic" :disabled="list.length === 1" title="Tirar da fila" :aria-label="`Tirar ${name(x)}`" @click="drop(i)"><i class="i i-x"></i></button>
        </span>
      </li>
    </ol>
    <button v-if="more > 0" class="more" @click="open = true">Mostrar mais {{ more }}</button>
    <button v-else-if="open" class="more" @click="open = false">Mostrar menos</button>
    <div v-if="others.length" class="addrow">
      <!-- escolher na lista já põe no fim da fila -->
      <select :aria-label="addLabel" @change="add">
        <option value="">＋ {{ addLabel }}…</option>
        <option v-for="x in others" :key="x" :value="x">{{ name(x) }}{{ hint(x) ? ` — ${hint(x)}` : '' }}</option>
      </select>
    </div>
  </div>
</template>
