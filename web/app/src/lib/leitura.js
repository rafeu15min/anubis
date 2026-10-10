// Dados da Leitura (src/leitura.rs): estante e "Para você" por categoria,
// pedidos de download. Guardados aqui, fora da tela: ela abre com o que já
// se tem e atualiza por trás (o cabeçalho também pede antes de você abrir).
import { reactive } from 'vue';
import { api } from './util.js';
import { sections } from './sections.js';

export const L = reactive({
  shelves: {},   // categoria → { estante, para_voce, preparando, error, at }
  jobs: [],
});

const DONE = ['no_drive', 'sem_fonte', 'falhou', 'sem_portugues'];
export const busy = () => L.jobs.some((j) => !DONE.includes(j.stage));

const loading = {};
export async function loadShelf(cat) {
  if (loading[cat]) return loading[cat];
  loading[cat] = (async () => {
    try {
      const j = await api(`/leitura/${cat}`);
      L.shelves[cat] = { ...j, error: null, at: Date.now() };
    } catch (e) {
      if (!L.shelves[cat]) L.shelves[cat] = { estante: [], para_voce: [], preparando: false, error: e.message, at: Date.now() };
      else L.shelves[cat].error = e.message;
    } finally { delete loading[cat]; }
  })();
  return loading[cat];
}

export async function loadJobs() {
  try { L.jobs = await api('/leitura/pedidos'); } catch {}
}

export function prefetch() {
  if (!L.shelves[sections.leitura.tab]) loadShelf(sections.leitura.tab);
  loadJobs();
}
