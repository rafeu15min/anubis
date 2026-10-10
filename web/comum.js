// O que as duas páginas (vídeos e leitura) compartilham:
//  - a marca do Anubis com o seletor Vídeos ↔ Leitura:
//      <anubis-marca modo="video|leitura"></anubis-marca>
//    O clique desliza o seletor e muda a cor (dourado = vídeos, azul =
//    leitura; o lado não escolhido fica cinza); depois navega, e a página
//    nova entra por View Transition (CSS das páginas).
//  - o carrossel em anel: as setas das fileiras (.railbox > .nav[data-scroll]
//    + .rail). No fim, a seta da direita segue para o começo, sempre para a
//    direita; no começo, a seta da esquerda não aparece.
(() => {
  const css = `
  anubis-marca { --video: #c9a24a; --leitura: #7cc4ee; --cinza: #6e6a7c; --cor: var(--video); display: inline-flex; flex: none; view-transition-name: marca; }
  anubis-marca.leitura { --cor: var(--leitura); }
  anubis-marca a { display: inline-flex; align-items: center; gap: 10px; min-height: 44px; color: var(--cor); text-decoration: none; transition: color .45s ease; }
  anubis-marca .nome { font-size: 18px; font-weight: 700; letter-spacing: .08em; }
  anubis-marca .trilho { position: relative; display: inline-flex; width: 64px; height: 32px; border-radius: 99px; border: 1px solid color-mix(in srgb, var(--cor) 45%, var(--line, #2c2a36)); background: var(--panel, #1b1a22); transition: border-color .45s ease; }
  anubis-marca .trilho .i { position: relative; z-index: 1; flex: 1; display: inline-flex; align-items: center; justify-content: center; font-size: 14px; color: var(--cinza); transition: color .45s ease; }
  anubis-marca:not(.leitura) .i-film { color: var(--video); }
  anubis-marca.leitura .i-book { color: var(--leitura); }
  anubis-marca .botao { position: absolute; top: 2px; left: 2px; width: 26px; height: 26px; border-radius: 50%;
    background: color-mix(in srgb, var(--cor) 28%, transparent); box-shadow: 0 0 10px color-mix(in srgb, var(--cor) 45%, transparent);
    transition: transform .45s cubic-bezier(.65, 0, .35, 1), background-color .45s ease, box-shadow .45s ease; }
  anubis-marca.leitura .botao { transform: translateX(32px); }
  anubis-marca a:hover .trilho, anubis-marca a:focus-visible .trilho { border-color: var(--cor); }
  anubis-marca a:focus-visible { outline: none; }
  .railbox:not(.andou) .nav[data-scroll="-1"], .railbox.cabe .nav { display: none !important; }
  @media (prefers-reduced-motion: reduce) { anubis-marca * { transition: none !important; } }`;
  document.head.insertAdjacentHTML('beforeend', `<style>${css}</style>`);
  const reduce = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

  customElements.define('anubis-marca', class extends HTMLElement {
    connectedCallback() {
      const leitura = this.getAttribute('modo') === 'leitura';
      const href = leitura ? '/' : '/leitura';
      const label = leitura ? 'Ir para filmes, séries e animes' : 'Ir para a leitura: livros, HQs, RPG';
      this.classList.toggle('leitura', leitura);
      this.innerHTML = `<a href="${href}" role="switch" aria-checked="${leitura}" title="${label}" aria-label="${label}">
        <span class="nome">ANUBIS</span><span class="trilho"><span class="botao"></span><i class="i i-film"></i><i class="i i-book"></i></span></a>`;
      this.querySelector('a').addEventListener('click', (e) => {
        if (e.ctrlKey || e.metaKey || e.shiftKey || e.button) return; // nova aba: sem animação
        e.preventDefault();
        this.classList.toggle('leitura', !leitura);
        setTimeout(() => { location.href = href; }, reduce() ? 0 : 420);
      });
      // voltar pelo navegador (página do cache): o seletor volta ao lugar
      addEventListener('pageshow', () => this.classList.toggle('leitura', leitura));
    }
  });

  // ---- carrossel em anel
  // Fileira maior que a tela: depois do último item vêm cópias do começo (o
  // bastante para uma tela), então a última página se completa com o que vem
  // "à frente". Fileira que cabe inteira não gira nem tem setas (repetir os
  // mesmos itens lado a lado parece defeito). Parada
  // a rolagem dentro das cópias, a posição vira a mesma nos originais (a
  // mesma imagem, sem salto). A seta da esquerda só existe depois de andar.
  const GAP = (rail) => parseFloat(getComputedStyle(rail).columnGap) || 0;
  const originals = (rail) => [...rail.children].filter((c) => !c.hasAttribute('data-copia'));

  function ring(rail) {
    rail.querySelectorAll(':scope > [data-copia]').forEach((c) => c.remove());
    const items = originals(rail);
    rail.dataset.anel = items.length;
    rail.dataset.largura = rail.clientWidth;
    if (!items.length || !rail.clientWidth) return;
    const step = items[1] ? items[1].getBoundingClientRect().left - items[0].getBoundingClientRect().left : items[0].offsetWidth + GAP(rail);
    const lap = items.length * step; // uma volta inteira
    const fits = lap - GAP(rail) <= rail.clientWidth + 1;
    rail.closest('.railbox')?.classList.toggle('cabe', fits);
    delete rail.dataset.volta;
    if (fits) return;
    // cópias até cobrir uma tela depois da volta
    const need = Math.ceil(rail.clientWidth / step) + 1;
    const frag = document.createDocumentFragment();
    for (let i = 0; i < need; i++) {
      const c = items[i % items.length].cloneNode(true);
      c.setAttribute('data-copia', '');
      c.setAttribute('aria-hidden', 'true');
      c.removeAttribute('id');
      c.querySelectorAll('[id]').forEach((x) => x.removeAttribute('id'));
      c.querySelectorAll('a, button, input, [tabindex]').forEach((x) => x.setAttribute('tabindex', '-1'));
      if (c.matches('a, button')) c.setAttribute('tabindex', '-1');
      frag.append(c);
    }
    rail.dataset.volta = lap;
    rail.append(frag);
  }
  // dentro das cópias: a mesma posição nos originais
  function settle(rail) {
    const lap = +rail.dataset.volta;
    if (lap && rail.scrollLeft >= lap - 1) {
      rail.style.scrollSnapType = 'none';
      rail.scrollTo({ left: rail.scrollLeft - lap, behavior: 'instant' });
      requestAnimationFrame(() => { rail.style.scrollSnapType = ''; });
    }
  }
  // fileiras novas ou refeitas (as páginas usam innerHTML), e tela redimensionada
  let pending = false;
  const scan = () => {
    pending = false;
    document.querySelectorAll('.rail').forEach((rail) => {
      if (+rail.dataset.anel !== originals(rail).length || +rail.dataset.largura !== rail.clientWidth) ring(rail);
    });
  };
  const later = () => { if (!pending) { pending = true; requestAnimationFrame(scan); } };
  new MutationObserver((list) => { if (list.some((m) => !(m.target.matches?.('.rail') && [...m.addedNodes, ...m.removedNodes].every((n) => n.nodeType !== 1 || n.hasAttribute('data-copia'))))) later(); })
    .observe(document.documentElement, { childList: true, subtree: true });
  addEventListener('resize', later);
  addEventListener('DOMContentLoaded', later);

  // espera a rolagem parar (o scrollend nem sempre vem na hora certa)
  const still = (rail) => new Promise((done) => {
    const t0 = performance.now();
    let last = NaN, n = 0;
    const tick = () => {
      const now = rail.scrollLeft;
      n = Math.abs(now - last) < 0.5 ? n + 1 : 0;
      last = now;
      if (n >= 3 || performance.now() - t0 > 2000) done();
      else requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  let timer;
  document.addEventListener('scroll', (e) => {
    const rail = e.target;
    if (!(rail instanceof Element) || !rail.matches('.rail')) return;
    rail.closest('.railbox')?.classList.toggle('andou', rail.scrollLeft > 4);
    // rolagem à mão (touchpad, dedo): ao parar, volta aos originais
    if (!rail.dataset.andando) { clearTimeout(timer); timer = setTimeout(() => settle(rail), 160); }
  }, true);

  async function step(rail, dir) {
    if (rail.dataset.andando) return;
    rail.dataset.andando = '1';
    rail.style.scrollSnapType = 'none';
    rail.scrollBy({ left: dir * rail.clientWidth * 0.9, behavior: reduce() ? 'auto' : 'smooth' });
    await still(rail);
    delete rail.dataset.andando;
    rail.style.scrollSnapType = '';
    settle(rail);
  }

  document.addEventListener('click', (e) => {
    const nav = e.target.closest('.railbox > .nav[data-scroll]');
    const rail = nav?.parentElement.querySelector('.rail');
    if (rail) step(rail, Math.sign(+nav.dataset.scroll));
  });
})();
