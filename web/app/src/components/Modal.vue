<script setup>
// Janela (<dialog> modal): abre e fecha por `open`; Esc, o ✕ e um clique
// fora dela avisam `close`.
import { ref, watch, onMounted } from 'vue';

const props = defineProps({ open: Boolean });
const emit = defineEmits(['close']);
const dlg = ref(null);

function sync(open) {
  const d = dlg.value;
  if (!d) return;
  if (open && !d.open) d.showModal();
  else if (!open && d.open) d.close();
}
watch(() => props.open, sync, { flush: 'post' });
onMounted(() => sync(props.open));

// clique no fundo escuro (fora do retângulo da janela)
function outside(e) {
  if (e.target !== dlg.value) return;
  const r = dlg.value.getBoundingClientRect();
  if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) emit('close');
}
</script>

<template>
  <dialog ref="dlg" @close="emit('close')" @click="outside">
    <button class="x" type="button" title="Fechar" aria-label="Fechar" @click="emit('close')"><i class="i i-x"></i></button>
    <div class="body"><slot /></div>
  </dialog>
</template>
