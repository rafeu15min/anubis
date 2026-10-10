<script setup>
// Janela de um título: o cartão, 👍/👎 e, na série, os episódios.
import { V, itemOf, closeItem, toggleLike, hide } from '../../lib/videos.js';
import Modal from '../Modal.vue';
import VideoCard from './VideoCard.vue';
import Episodes from './Episodes.vue';
</script>

<template>
  <Modal :open="!!V.open" @close="closeItem">
    <template v-if="V.open && itemOf(V.open)">
      <div class="grid"><VideoCard :k="V.open" in-dialog /></div>
      <div class="row">
        <button class="icon" :class="{ on: V.liked[V.open] }" :disabled="!!V.busy['like:' + V.open]" :aria-pressed="!!V.liked[V.open]"
          title="Gostei (o que se parece com ele aparece mais no Para você)" aria-label="Gostei" @click="toggleLike(V.open)"><i class="i i-like"></i></button>
        <button class="icon quiet" :disabled="!!V.busy['hide:' + V.open]" title="Não quero ver (sai da vitrine)" aria-label="Não quero ver" @click="hide(V.open)"><i class="i i-dislike"></i></button>
      </div>
      <Episodes v-if="itemOf(V.open).kind === 'series'" />
    </template>
  </Modal>
</template>
