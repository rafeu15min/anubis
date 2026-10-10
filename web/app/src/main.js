import { createApp } from 'vue';
import { createRouter, createWebHistory } from 'vue-router';
import App from './App.vue';
import Videos from './views/Videos.vue';
import Leitura from './views/Leitura.vue';
import './style.css';

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', component: Videos, meta: { section: 'video', title: 'Anubis' } },
    { path: '/leitura', component: Leitura, meta: { section: 'leitura', title: 'Anubis · Leitura' } },
    { path: '/preferencias', component: () => import('./views/Prefs.vue'), meta: { title: 'Anubis · Preferências' } },
    { path: '/watch/:kind/:id', component: () => import('./views/Player.vue'), meta: { title: 'Anubis · Player', bare: true } },
    { path: '/:rest(.*)*', redirect: '/' },
  ],
  // voltar: onde estava; tela nova: do topo
  scrollBehavior: (to, from, saved) => saved || (to.meta.section && to.meta.section === from.meta.section ? false : { top: 0 }),
});
router.afterEach((to) => {
  document.documentElement.classList.toggle('leitura', to.meta.section === 'leitura');
  if (to.meta.title) document.title = to.meta.title;
});

createApp(App).use(router).mount('#app');

// escala de caminhos: cabo direto » rede local » Tailscale. O /health lista os
// endereços nessa ordem; os mais rápidos que o atual são testados juntos e a
// página passa para o melhor que responder (uma vez por sessão).
(async () => {
  try { if (sessionStorage.getItem('anubis.caminho')) return; sessionStorage.setItem('anubis.caminho', '1'); } catch {}
  let addrs = [];
  try { addrs = (await (await fetch('/health')).json()).addresses || []; } catch { return; }
  const here = addrs.indexOf(location.host);
  const better = here < 0 ? addrs : addrs.slice(0, here);
  const reach = (a) => {
    const ctl = new AbortController();
    setTimeout(() => ctl.abort(), 1500);
    // sem CORS: só interessa se o endereço responde
    return fetch(`http://${a}/health`, { mode: 'no-cors', cache: 'no-store', signal: ctl.signal }).then(() => a);
  };
  const res = await Promise.allSettled(better.map(reach));
  const best = res.find((r) => r.status === 'fulfilled');
  if (best && best.value !== location.host) location.replace(`http://${best.value}${location.pathname}${location.search}${location.hash}`);
})();
