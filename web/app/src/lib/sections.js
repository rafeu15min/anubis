// Vídeos e Leitura: a aba e a busca de cada uma. O cabeçalho (App.vue) e a
// tela da seção leem e escrevem aqui; cada seção guarda o seu.
import { reactive } from 'vue';
import { stored, store } from './util.js';

function section(key, tabs, placeholder) {
  const saved = stored(key);
  return { key, tabs, placeholder, tab: tabs.some(([s]) => s === saved) ? saved : tabs[0][0], query: '' };
}

export const sections = reactive({
  video: section('anubis.categoria', [['filmes', 'Filmes'], ['series', 'Séries'], ['animes', 'Animes']], 'Buscar filme ou série…'),
  leitura: section('leitura.aba', [['ficcao', 'Ficção'], ['tecnicos', 'Técnicos'], ['artigos', 'Artigos'], ['mangas-hqs', 'Mangás e HQs']], 'Buscar livro, autor, HQ…'),
});

// trocar de aba volta à vitrine (limpa a busca)
export function pickTab(name, tab) {
  const s = sections[name];
  s.tab = tab;
  s.query = '';
  store(s.key, tab);
}
