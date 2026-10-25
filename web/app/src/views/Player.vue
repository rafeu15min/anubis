<script setup>
// Player: toca enquanto baixa (direto, ou convertido na hora pelo ffmpeg do
// Anubis), com legendas, qualidade automática, gestos de toque e progresso
// gravado no Jellyfin. API: /watch/{kind}/{id}/… (src/api.rs).
import { ref, reactive, computed, watch, onMounted, onBeforeUnmount } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { jf, stored, store } from '../lib/util.js';

const route = useRoute();
const router = useRouter();
const KIND = route.params.kind, ID = route.params.id;
const base = `/watch/${KIND}/${ID}`;
const RESUME = `anubis.pos.${base}`;

const v = ref(null), root = ref(null), bar = ref(null), menuEl = ref(null);
const ui = reactive({ hidden: false, waiting: true, locked: false });
const title = ref('…'), release = ref('');
const panel = reactive({ text: 'Preparando…', jf: '' });
const msg = reactive({ on: false, text: '' });
const nextEp = ref(null), nextBusy = ref(false);
const hint = reactive({ text: '', cls: '' });
const paused = ref(true), muted = ref(false), volume = ref(1), rate = ref(1);
const time = reactive({ cur: '0:00', rem: '-0:00', frac: 0, tip: '0:00', tipAt: 0 });
const bufs = ref([]), have = ref([]);
const dragging = ref(false);

// ---- limpeza: tudo o que o player liga fora dele sai junto quando ele sai
const offs = [];
const on = (t, ev, fn, opts) => { t.addEventListener(ev, fn, opts); offs.push(() => t.removeEventListener(ev, fn, opts)); };
const timers = new Set();
const later = (fn, ms) => { const t = setTimeout(() => { timers.delete(t); fn(); }, ms); timers.add(t); return t; };
const cancel = (t) => { clearTimeout(t); timers.delete(t); };

const fmt = (s) => {
  if (!isFinite(s) || s < 0) s = 0;
  const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = Math.floor(s % 60);
  return (h ? `${h}:${String(m).padStart(2, '0')}` : m) + ':' + String(x).padStart(2, '0');
};

// ---- tempo: direto, o <video> sabe tudo; convertido (ffmpeg), o vídeo
// começa em `offset` e a duração vem do ffprobe
let converted = false, offset = 0, duration = 0;
// posição pedida enquanto o stream novo ainda não começou
let target = null, seekTimer;
const now = () => target ?? offset + (v.value.currentTime || 0);
const total = () => (converted ? duration : v.value.duration || duration || 0);
// a tela mostra a posição nova na hora (o <video> só avisa quando o stream
// novo começa). `wait`: toques seguidos em voltar/avançar se somam e só a
// posição final abre um stream, já que cada um é um ffmpeg novo
function seek(t, wait = 0) {
  t = Math.max(0, Math.min(total() - 1, t));
  paint(t);
  if (!converted) { v.value.currentTime = t; return; }
  target = t;
  ui.waiting = true;
  cancel(seekTimer);
  seekTimer = later(() => {
    target = null;
    offset = t;
    setSrc(streamUrl(t));
    v.value.play().catch(() => {});
    applySub();
  }, wait);
}

// ---- formato e escolhas do menu. M: o que o ffprobe viu (altura, faixas de
// áudio). Faixa de áudio que não é a primeira, qualidade abaixo da do
// arquivo ou dublagem corrigida obrigam a converter mesmo um vídeo que o
// navegador tocaria direto
const M = ref({});
let T = 720; // altura do convertido sem escolha (padrão do servidor)
const aud = ref(0), quality = ref('auto'), autoH = ref(null);
let forceConv = false;
const QKEY = 'anubis.qualidade', AKEY = 'anubis.audio', RKEY = 'anubis.velocidade';
const effH = () => (quality.value === 'auto' ? autoH.value : quality.value);
const needsConvert = () => (!!M.value.mode && M.value.mode !== 'direct') || forceConv || aud.value > 0 || (!!effH() && !!M.value.height && effH() < M.value.height);
const streamUrl = (t) => `${base}/stream.mp4?t=${t.toFixed(1)}` + (aud.value ? `&a=${aud.value}` : '') + (effH() ? `&h=${effH()}` : '');
// abre o vídeo em `t`: direto (o <video> busca sozinho) ou convertido a partir de `t`
function openAt(t) {
  converted = needsConvert();
  if (converted) { offset = t; setSrc(streamUrl(t)); }
  else {
    offset = 0;
    setSrc(`${base}/video`);
    if (t) v.value.addEventListener('loadedmetadata', () => { v.value.currentTime = t; }, { once: true });
  }
  v.value.play().catch(() => {});
}
// troca de qualidade/áudio/dublagem: continua do ponto atual
function reopen() {
  const t = now();
  target = null;
  cancel(seekTimer);
  ui.waiting = true;
  openAt(t);
  applySub();
}

// onde a pessoa parou no Jellyfin (qualquer aparelho, ou o próprio Jellyfin)
const savedAt = fetch(`${base}/progress`).then((r) => (r.ok ? r.json() : null)).then((d) => d && d.t).catch(() => null);

let started = false;
async function start(media) {
  M.value = media || {};
  duration = M.value.duration || 0;
  const q = stored(QKEY);
  if (q && q !== 'auto' && M.value.height) quality.value = Math.min(+q, M.value.height);
  const a = (M.value.audios || []).findIndex((x) => x.lang && x.lang === stored(AKEY));
  if (a > 0) aud.value = a;
  const r = +stored(RKEY);
  if (r) v.value.defaultPlaybackRate = v.value.playbackRate = r;
  let pos = 0;
  const p = parseFloat(stored(RESUME));
  if (p > 5) pos = p;
  // vale o do Jellyfin; o deste aparelho só se estiver perto (é salvo mais
  // vezes, então é mais exato) ou se o Jellyfin não souber
  const jfAt = await Promise.race([savedAt, new Promise((ok) => setTimeout(() => ok(null), 2500))]);
  if (!alive) return;
  if (jfAt > 5 && !(pos && Math.abs(pos - jfAt) < 30)) pos = jfAt;
  openAt(pos);
}

// ---- dublagem: se o Anubis mede um atraso com o vídeo já tocando, recomeça
// o stream na posição atual (o ffmpeg passa a corrigir a dublagem)
const dubSecs = (d) => (d && d.state === 'offset' ? d.seconds : 0);
let dubApplied = null;
function syncDub(dub) {
  const s = dubSecs(dub);
  if (dubApplied === null) { dubApplied = s; return; }
  if (s === dubApplied) return;
  dubApplied = s;
  forceConv = true;
  reopen();
}

// ---- progresso no Jellyfin: o player avisa onde está (a cada 15 s tocando,
// ao pausar, ao sair da aba, ao sair do player e no fim)
let reportedAt = 0;
function report(beacon = false, t = now()) {
  const d = total();
  if (!started || !d) return;
  reportedAt = performance.now();
  const body = JSON.stringify({ t, duration: d });
  // saindo da página, só o sendBeacon garante a entrega
  if (beacon) navigator.sendBeacon(`${base}/progress`, new Blob([body], { type: 'application/json' }));
  else fetch(`${base}/progress`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body }).catch(() => {});
}

// ---- barra: branca = já no disco, amarela = carregado no player, roxa = assistido
let dragTo = null; // destino de um arrasto na tela, enquanto o dedo está nela
function paint(t = dragTo ?? now()) {
  const d = total();
  time.frac = d ? t / d : 0;
  time.cur = fmt(t);
  time.rem = '-' + fmt(d - t);
}
// o que o <video> já tem carregado (no convertido, contado a partir de `offset`)
function paintBuf() {
  const d = total(), b = v.value.buffered, from = converted ? offset : 0;
  const out = [];
  if (d && target === null) for (let i = 0; i < b.length; i++) {
    const a = (from + b.start(i)) / d, z = Math.min(1, (from + b.end(i)) / d);
    out.push([a, z]);
  }
  if (JSON.stringify(out) !== JSON.stringify(bufs.value)) bufs.value = out;
}
const frac = (e) => { const r = bar.value.getBoundingClientRect(); return Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)); };
function barDown(e) { poke(); dragging.value = true; bar.value.setPointerCapture(e.pointerId); paint(frac(e) * total()); }
function barMove(e) {
  const f = frac(e);
  time.tipAt = f;
  time.tip = fmt(f * total());
  if (dragging.value) paint(f * total());
}
function barUp(e) { dragging.value = false; if (total()) seek(frac(e) * total()); }

// ---- controles
const toggle = () => (v.value.paused ? v.value.play() : v.value.pause());
const skip = (s) => { seek(now() + s, 600); poke(); };
function back() { if (history.state && history.state.back) router.back(); else router.push('/'); }
const full = () => (document.fullscreenElement ? document.exitFullscreen() : document.documentElement.requestFullscreen()).catch(() => {});
const pip = () => (document.pictureInPictureElement ? document.exitPictureInPicture() : v.value.requestPictureInPicture()).catch(() => {});
function setVolume(e) { v.value.volume = +e.target.value; v.value.muted = v.value.volume === 0; }
// mouse (PC): clique pausa, clique duplo alterna a tela cheia. O toque gera
// esses eventos também; ele é tratado à parte, mais abaixo
let pointer = 'mouse';
const videoClick = () => { if (pointer === 'mouse') toggle(); };
const videoDbl = () => { if (pointer === 'mouse') full(); };

// ---- controles somem sem movimento (com o vídeo tocando)
let hideTimer;
function poke() {
  ui.hidden = false;
  cancel(hideTimer);
  hideTimer = later(() => { if (!v.value.paused && !dragging.value && !menuOpen.value) ui.hidden = true; }, 3000);
}
function hide() { cancel(hideTimer); ui.hidden = true; }

// aviso no meio da tela (`ms` = 0: fica até o próximo)
let hintTimer;
function say(text, where, ms = 900) {
  hint.text = text;
  hint.cls = where;
  cancel(hintTimer);
  if (ms) hintTimer = later(() => { hint.cls = ''; }, ms);
}

// ---- toque (celular): um toque mostra/esconde os controles, sem pausar.
// Dois toques seguidos numa lateral voltam/avançam 10 s e, enquanto a série
// dura, cada toque a mais no mesmo lado soma 10 s (como no YouTube); dois
// toques no centro alternam a tela cheia. O toque simples espera o intervalo
// do duplo, para os dois não dispararem juntos.
// Gestos: segurar = 2× enquanto segura; arrastar na horizontal busca (a
// largura da tela vale 2 min); na vertical, na metade direita, muda o volume
// (onde a página pode: no iPhone o volume do vídeo é só leitura); pinça
// alterna entre preencher a tela e mostrar o quadro inteiro. Com a tela
// bloqueada, um toque só mostra o cadeado
const UI = 'button, input, a, .seekbar, .menu, .slow, .panel, .msg';
const TAP_GAP = 280, SERIES = 800, HOLD = 500, MOVE = 12, SEEK_SPAN = 120;
const CAN_VOLUME = (() => { const a = document.createElement('video'); a.volume = 0.5; return a.volume === 0.5; })();
let down = null, tapTimer, lastTap = -1e9, series = null, pinch = null;
const touches = new Map(); // dedos na tela: id → [x, y]
const zone = (x) => (x < innerWidth * 0.35 ? -1 : x > innerWidth * 0.65 ? 1 : 0);
const signed = (x) => `${x < 0 ? '−' : '+'}${fmt(Math.abs(x))}`;
function seekTap(dir) {
  if (!series || series.dir !== dir) series = { dir, n: 0 };
  series.n++;
  cancel(series.timer);
  series.timer = later(() => { series = null; }, SERIES);
  seek(now() + dir * 10, 600);
  say(dir < 0 ? `« ${series.n * 10} s` : `${series.n * 10} s »`, dir < 0 ? 'l' : 'r', SERIES);
}
const FILLKEY = 'anubis.preencher';
const fill = ref(stored(FILLKEY) === '1');
function setFill(onOff, tell = true) {
  fill.value = onOff;
  store(FILLKEY, onOff ? 1 : 0);
  if (tell) say(onOff ? 'Preenchendo a tela' : 'Quadro inteiro', 'm');
}

function touchDown(e) {
  pointer = e.pointerType;
  if (pointer === 'mouse') return;
  touches.set(e.pointerId, [e.clientX, e.clientY]);
  if (touches.size === 2 && !ui.locked) {
    // segundo dedo: pinça (o primeiro deixa de ser toque, segurar ou arrasto)
    const [a, b] = [...touches.values()];
    pinch = { d: Math.hypot(a[0] - b[0], a[1] - b[1]), done: false };
    if (down) { cancel(down.hold); if (down.mode === 'hold') v.value.playbackRate = down.rate; }
    down = null; dragTo = null;
    return;
  }
  if (touches.size > 1) return;
  const isUi = !!e.target.closest(UI);
  down = { x: e.clientX, y: e.clientY, t: e.timeStamp, ui: isUi, mode: null };
  if (isUi) { poke(); return; }
  if (ui.locked) return;
  down.hold = later(() => {
    if (!down || down.mode || v.value.paused) return;
    down.mode = 'hold';
    down.rate = v.value.playbackRate;
    v.value.playbackRate = 2;
    say('2× »', 'c', 0);
  }, HOLD);
}
function touchMove(e) {
  if (e.pointerType === 'mouse' || !touches.has(e.pointerId)) return;
  touches.set(e.pointerId, [e.clientX, e.clientY]);
  if (pinch) {
    if (touches.size < 2 || pinch.done) return;
    const [a, b] = [...touches.values()];
    const k = Math.hypot(a[0] - b[0], a[1] - b[1]) / pinch.d;
    if (k > 1.15 || k < 0.87) { pinch.done = true; setFill(k > 1); }
    return;
  }
  if (!down || down.ui || ui.locked) return;
  const dx = e.clientX - down.x, dy = e.clientY - down.y;
  if (!down.mode) {
    if (Math.hypot(dx, dy) < MOVE) return;
    cancel(down.hold);
    if (Math.abs(dx) > Math.abs(dy)) { down.mode = 'seek'; down.from = now(); }
    else if (down.x > innerWidth / 2 && CAN_VOLUME) { down.mode = 'volume'; down.vol = v.value.muted ? 0 : v.value.volume; }
    else down.mode = 'none';
  }
  if (down.mode === 'seek' && total()) {
    dragTo = Math.max(0, Math.min(total() - 1, down.from + (dx / innerWidth) * SEEK_SPAN));
    paint();
    say(`${signed(dragTo - down.from)} · ${fmt(dragTo)}`, 'm', 0);
  } else if (down.mode === 'volume') {
    const vol = Math.max(0, Math.min(1, down.vol - dy / (innerHeight * 0.6)));
    v.value.volume = vol;
    v.value.muted = vol === 0;
    say(`Volume ${Math.round(vol * 100)}%`, 'm', 0);
  }
}
function touchEnd(e) {
  if (e.pointerType === 'mouse') return;
  touches.delete(e.pointerId);
  if (pinch) { if (!touches.size) pinch = null; return; } // depois da pinça, nada de toque
  if (!down) return;
  const g = down;
  down = null;
  cancel(g.hold);
  if (g.mode === 'hold') { v.value.playbackRate = g.rate; hint.cls = ''; return; }
  if (g.mode === 'seek') {
    const to = dragTo;
    dragTo = null;
    say(hint.text, 'm', 500);
    if (e.type === 'pointerup' && to !== null) seek(to); else paint();
    return;
  }
  if (g.mode) { if (g.mode === 'volume') say(hint.text, 'm', 500); return; }
  if (e.type !== 'pointerup' || e.timeStamp - g.t >= HOLD || g.ui || menuOpen.value) return;
  if (ui.locked) { poke(); return; }
  const z = zone(e.clientX);
  if (series && z === series.dir) return seekTap(z);
  if (e.timeStamp - lastTap < TAP_GAP) {
    cancel(tapTimer);
    lastTap = -1e9;
    return z ? seekTap(z) : full();
  }
  lastTap = e.timeStamp;
  tapTimer = later(() => (ui.hidden ? poke() : hide()), TAP_GAP);
}

// ---- bloqueio de tela: nada responde ao toque além do cadeado
function lock() { menuOpen.value = false; ui.locked = true; say('Tela bloqueada', 'c', 1200); poke(); }
function unlock() { ui.locked = false; say('Tela desbloqueada', 'c', 1200); poke(); }

// ---- de volta à aba: o Chrome no celular desliga o vídeo de abas em segundo
// plano e, ao religar, o vídeo pode voltar fora de sincronia com o som.
// Reposiciona no ponto atual (no convertido, reabre o stream ali). Pausado,
// espera o play
let hiddenAt = 0, stale = false;
function resync() { stale = false; if (converted) seek(now()); else v.value.currentTime = v.value.currentTime; }

// ---- legendas: arquivos do Bazarr/torrent e faixas embutidas, em WebVTT
const SUBKEY = 'anubis.legenda'; // idioma escolhido da última vez ('' = desligada)
const subs = ref([]), subSel = ref(-1);
const wantLang = stored(SUBKEY);
let subTrack = null;
function applySub() {
  const vid = v.value;
  vid.querySelectorAll('track').forEach((t) => t.remove());
  if (subSel.value < 0) return;
  const s = subs.value[subSel.value];
  const t = document.createElement('track');
  t.kind = 'subtitles';
  t.srclang = s.lang;
  t.label = s.label;
  // vídeo convertido começa em `offset`: a legenda é adiantada junto
  t.src = `${base}/subs/${subSel.value}?shift=${converted ? offset.toFixed(1) : 0}`;
  t.default = true;
  vid.appendChild(t);
  t.addEventListener('load', () => { t.track.mode = 'showing'; fixCues(t.track); });
  t.track.mode = 'showing';
  subTrack = t;
}
function pickSub(i) {
  subSel.value = i;
  store(SUBKEY, i >= 0 ? subs.value[i].lang : '');
  applySub();
}
// tamanho e posição valem para todo vídeo; o atraso, só para este
const SIZEKEY = 'anubis.legenda.tamanho', POSKEY = 'anubis.legenda.posicao', DELAYKEY = `anubis.legenda.atraso.${base}`;
const SIZES = [[0.75, 'Pequena'], [1, 'Normal'], [1.25, 'Grande'], [1.5, 'Maior'], [2, 'Enorme']];
const POSITIONS = [['auto', 'Embaixo'], ['80', 'Mais acima'], ['65', 'Bem acima']];
const subSize = ref(+stored(SIZEKEY) || 1), subPos = ref(stored(POSKEY) || 'auto'), subDelay = ref(+stored(DELAYKEY) || 0);
// o tamanho vale para o ::cue do vídeo: uma folha de estilo só do player
const cueStyle = document.createElement('style');
watch(subSize, (s) => { cueStyle.textContent = `.player video::cue { font-size: calc(clamp(16px, 2.6vw, 30px) * ${s}); }`; }, { immediate: true });
// posição e atraso mexem nas falas já carregadas (o atraso soma a diferença
// para o que já foi aplicado nesta faixa)
function fixCues(tr) {
  if (!tr || !tr.cues) return;
  const dd = subDelay.value - (tr.anubisDelay || 0);
  for (const c of tr.cues) {
    if (dd) { c.startTime += dd; c.endTime += dd; }
    if (subPos.value === 'auto') { c.snapToLines = true; c.line = 'auto'; } else { c.snapToLines = false; c.line = +subPos.value; }
  }
  tr.anubisDelay = subDelay.value;
}
const fixAllCues = () => [...v.value.textTracks].forEach(fixCues);
function sizeStep(by) {
  const i = SIZES.findIndex(([x]) => x === subSize.value) + by;
  subSize.value = SIZES[Math.max(0, Math.min(SIZES.length - 1, i))][0];
  store(SIZEKEY, subSize.value);
}
function setPos(p) { subPos.value = p; store(POSKEY, p); fixAllCues(); }
function delay(by) { subDelay.value = by ? Math.round((subDelay.value + by) * 10) / 10 : 0; store(DELAYKEY, subDelay.value); fixAllCues(); }
async function loadSubs() {
  try {
    const r = await fetch(`${base}/subs`);
    if (!r.ok) return;
    const list = await r.json();
    if (JSON.stringify(list) === JSON.stringify(subs.value)) return;
    const cur = subSel.value >= 0 ? subs.value[subSel.value].label : null;
    subs.value = list;
    subSel.value = cur ? list.findIndex((s) => s.label === cur) : -1;
    // primeira vez: liga no idioma escolhido antes
    if (subSel.value < 0 && wantLang) { subSel.value = list.findIndex((s) => s.lang === wantLang); if (subSel.value >= 0) applySub(); }
  } catch {}
}

// ---- menu: qualidade, áudio, legendas, velocidade
const LANG = { por: 'Português', eng: 'Inglês', spa: 'Espanhol', jpn: 'Japonês', fre: 'Francês', fra: 'Francês', ger: 'Alemão', deu: 'Alemão', ita: 'Italiano', kor: 'Coreano', chi: 'Chinês', zho: 'Chinês', rus: 'Russo' };
const CH = { 1: 'mono', 2: 'estéreo', 6: '5.1', 8: '7.1' };
const audioName = (a) => LANG[a.lang] || a.lang || `Faixa ${a.n + 1}`;
const audioLabel = (a) => [audioName(a), a.title, CH[a.channels]].filter(Boolean).join(' · ');
const RATES = [0.5, 0.75, 1, 1.25, 1.5, 1.75, 2];
const rateLabel = (r) => (r === 1 ? 'Normal' : `${String(r).replace('.', ',')}×`);
const secs = (x) => `${x > 0 ? '+' : x < 0 ? '−' : ''}${Math.abs(x).toFixed(1).replace('.', ',')} s`;
// alturas oferecidas: a do arquivo e as comuns abaixo dela
const heights = computed(() => (M.value.height ? [M.value.height, ...[1080, 720, 480, 360].filter((h) => h < M.value.height)] : []));
// onde a automática começa: o convertido sai em T; o resto, na altura do arquivo
const autoStart = () => (M.value.mode === 'transcode' ? Math.min(T, M.value.height) : M.value.height);
const curHeight = () => effH() || autoStart();
const hLabel = (h) => (h === M.value.height ? `Original · ${h}p` : `${h}p`);
const qualityLabel = computed(() => (quality.value === 'auto' ? `Automática (${curHeight()}p)` : hLabel(quality.value)));
const sizeIdx = computed(() => SIZES.findIndex(([x]) => x === subSize.value));
const menuOpen = ref(false), menuPage = ref('main');
watch(menuPage, () => { if (menuEl.value) menuEl.value.scrollTop = 0; });
function openMenu(page) {
  if (menuOpen.value && menuPage.value === page) { menuOpen.value = false; return; }
  menuPage.value = page;
  menuOpen.value = true;
  poke();
}
function setQuality(q) { quality.value = q; autoH.value = null; store(QKEY, q); reopen(); menuPage.value = 'main'; }
function setAudio(n) { aud.value = n; store(AKEY, M.value.audios[n].lang); reopen(); menuPage.value = 'main'; }
function setRate(r) { v.value.defaultPlaybackRate = v.value.playbackRate = r; store(RKEY, r); menuPage.value = 'main'; }

// ---- teclado
function key(e) {
  if (e.target.tagName === 'INPUT') return;
  const k = e.key.toLowerCase();
  const vid = v.value;
  if (k === ' ' || k === 'k') { e.preventDefault(); toggle(); }
  else if (k === 'arrowleft' || k === 'j') skip(-10);
  else if (k === 'arrowright' || k === 'l') skip(10);
  else if (k === 'arrowup') { e.preventDefault(); vid.volume = Math.min(1, vid.volume + 0.1); }
  else if (k === 'arrowdown') { e.preventDefault(); vid.volume = Math.max(0, vid.volume - 0.1); }
  else if (k === 'f') full();
  else if (k === 'm') vid.muted = !vid.muted;
  else if (k === 'c') {
    const want = subs.value.findIndex((s) => s.lang === (wantLang || 'pt-BR'));
    pickSub(subSel.value >= 0 ? -1 : want >= 0 ? want : subs.value.length ? 0 : -1);
  }
  // como no YouTube: < > velocidade, 0–9 vai a 0–90 %, , . quadro a quadro (pausado)
  else if (k === '<' || k === '>') {
    const i = RATES.indexOf(vid.playbackRate);
    const r = RATES[Math.max(0, Math.min(RATES.length - 1, (i < 0 ? RATES.indexOf(1) : i) + (k === '>' ? 1 : -1)))];
    vid.defaultPlaybackRate = vid.playbackRate = r;
    store(RKEY, r);
    say(rateLabel(r), 'm');
  }
  else if (/^[0-9]$/.test(k) && !e.ctrlKey && !e.altKey && !e.metaKey) { if (total()) seek((total() * +k) / 10); }
  else if ((k === ',' || k === '.') && vid.paused) {
    // o convertido chega como um fluxo só, sem como voltar dentro dele
    const t = Math.max(0, vid.currentTime + (k === '.' ? 1 : -1) / 24);
    const ok = [...Array(vid.seekable.length).keys()].some((i) => t >= vid.seekable.start(i) && t <= vid.seekable.end(i));
    if (ok) vid.currentTime = t; else say('Quadro a quadro só com o vídeo sem conversão', 'm', 1500);
  }
  else if (k === 'n' && e.shiftKey) { if (nextEp.value) goNext(); }
  else if (k === 'v') {
    const n = (M.value.audios || []).length;
    if (n > 1) { aud.value = (aud.value + 1) % n; store(AKEY, M.value.audios[aud.value].lang); reopen(); say(`Áudio: ${audioName(M.value.audios[aud.value])}`, 'm', 1500); }
  }
  else if (k === '+' || k === '=' || k === '-') {
    sizeStep(k === '-' ? -1 : 1);
    say(`Legenda: ${SIZES.find(([x]) => x === subSize.value)[1]}`, 'm');
  }
  poke();
}

// ---- próximo episódio
async function goNext() {
  const [id] = nextEp.value;
  nextBusy.value = true;
  const r = await fetch('/watch', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ kind: 'episode', id }) });
  const d = await r.json();
  if (r.ok) router.push('/' + d.player.replace(/^\//, ''));
  else router.push('/'); // sem versão aprovada: a busca mostra as opções
}

// ---- qualidade automática: se o vídeo trava sem o download ser o motivo
// (o trecho à frente já está no disco), a conversão ou a conexão não dá
// conta: desce um degrau (720p → 480p → 360p) e reabre no ponto atual. Conta
// como travada uma espera de mais de 2,5 s ou 3 esperas em 1 minuto, fora os
// primeiros 5 s depois de abrir o vídeo
let srcAt = 0, stallAt = 0, stalls = [], phase = null, settled = false;
function setSrc(u) { srcAt = performance.now(); stallAt = 0; v.value.src = u; }
// travada sem voltar a tocar (chamada a cada 2 s)
function stuck() { if (stallAt && performance.now() - stallAt > 5000) { stallAt = 0; degrade(); } }
function degrade() {
  if (quality.value !== 'auto' || !M.value.height) return;
  if (phase !== 'done' && aheadSecs() < 30) return; // é o download: o aviso de versão mais leve cuida
  const lower = [720, 480, 360].find((h) => h < curHeight());
  if (!lower) return;
  autoH.value = lower;
  stalls = [];
  reopen();
}

// ---- o download acompanha o vídeo? Se ficar ~30 s abaixo do que o vídeo
// consome, com pouco já baixado à frente, oferece uma versão mais leve
let slowFor = 0, slowOffered = false;
const mb = (b) => (b >= 2 ** 30 ? `${(b / 2 ** 30).toFixed(1)} GB` : `${Math.round(b / 2 ** 20)} MB`);
function aheadSecs() {
  const dur = total();
  if (!dur) return Infinity;
  const f = now() / dur;
  const r = have.value.find(([a, b]) => a <= f + 1e-6 && f < b);
  return r ? (r[1] - f) * dur : 0;
}
// a caixa de troca de versão: mais leve (o download não acompanha) ou melhor
// (tocando da biblioteca abaixo do topo da fila de qualidade)
const slow = ref(null); // { head, loading, list, chosen, error }
function checkPace(d) {
  const dur = total();
  if (slowOffered || d.phase !== 'downloading' || !d.size || !dur || d.speed_mbps == null) { slowFor = 0; return; }
  const need = d.size / dur / 2 ** 20; // MB/s que o vídeo consome
  if (d.speed_mbps < need && aheadSecs() < 60) slowFor += 2; else slowFor = 0;
  if (slowFor >= 30) { slowOffered = true; offerLighter(d, need); }
}
async function offerLighter(d, need) {
  slow.value = { head: `O download não está acompanhando esta versão: chega ${d.speed_mbps.toFixed(1)} MB/s e o vídeo precisa de ${need.toFixed(1)} MB/s. Vai travar à frente.`, loading: true, list: [], empty: 'Nenhuma versão mais leve com fontes suficientes agora.' };
  let list = [];
  try { const r = await fetch(`${base}/lighter`); if (r.ok) list = await r.json(); } catch {}
  if (slow.value) { slow.value.list = list; slow.value.loading = false; }
}
// (a busca leva um tempo; o vídeo já está tocando)
async function offerBetter() {
  let list = [];
  try { const r = await fetch(`${base}/better`); if (r.ok) list = await r.json(); } catch {}
  if (!list.length || slow.value || !alive) return;
  slow.value = { head: `Há uma versão melhor desta, mais acima na sua fila de qualidade (${list[0].quality.replace('-', ' ')}). A troca baixa a nova (dá para continuar assistindo enquanto baixa) e ela substitui a atual na biblioteca.`, loading: false, list: list.slice(0, 3) };
}
async function swap(c) {
  slow.value.chosen = c.guid;
  // a posição fica salva: a nova versão continua do mesmo ponto
  store(RESUME, String(now()));
  const r = await fetch('/watch', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ kind: KIND, id: +ID, guid: c.guid }) });
  const res = await r.json();
  if (r.ok) router.push('/' + res.player.replace(/^\//, ''));
  else { slow.value.chosen = null; slow.value.error = `Não deu: ${res.error || 'versão recusada'}`; }
}

// ---- andamento do download (e o formato, para começar a tocar)
const PHASE = { downloading: 'Baixando', importing: 'Entrando na biblioteca…', done: 'Na biblioteca', failed: 'Falhou' };
const MODE = { remux: ' · adaptando o formato', transcode: ' · convertendo o vídeo' };
let waitedMedia = 0, alive = true;
async function info() {
  try {
    const r = await fetch(`${base}/info`);
    const d = await r.json();
    if (!alive) return;
    if (!r.ok) { panel.text = d.error || 'Nada sendo assistido aqui.'; return; }
    title.value = d.title; document.title = `${d.title} · Anubis`;
    release.value = d.release;
    have.value = d.have || [];
    if (d.transcode_height) T = d.transcode_height;
    phase = d.phase;
    // baixado e com a dublagem conferida: só o link do Jellyfin pode mudar
    settled = d.phase === 'done' && !(d.dub && d.dub.state === 'checking');
    // começa quando o formato é conhecido. O servidor sempre responde (o
    // ffprobe tem 2 min; sem ele, "toca direto"): só sem resposta nenhuma em
    // ~130 s o player arrisca tocar o arquivo como está
    if (!started && (d.media || ++waitedMedia > 65)) { started = true; start(d.media); if (d.phase === 'done') offerBetter(); }
    // começou sem o formato e ele chegou: se o navegador não toca o arquivo
    // como está (ex. áudio E-AC3 no Android: a imagem vai e o som não),
    // passa para o convertido no ponto atual
    else if (started && d.media && !M.value.mode) {
      M.value = d.media;
      if (needsConvert()) reopen();
    }
    if (started) syncDub(d.dub);
    if (d.media && d.media.duration) duration = d.media.duration;
    if (d.next && !nextEp.value) nextEp.value = d.next;
    let t = PHASE[d.phase] || d.phase;
    if (d.phase === 'downloading') {
      t += ` ${Math.round(d.progress * 100)}%`;
      if (d.speed_mbps != null) t += ` · ${d.speed_mbps.toFixed(1)} MB/s`;
      if (d.peers != null) t += ` · ${d.peers} fontes`;
    }
    checkPace(d);
    stuck();
    if (d.media && MODE[d.media.mode]) t += MODE[d.media.mode];
    if (d.dub && d.dub.state === 'checking') t += ' · conferindo a sincronia da dublagem';
    if (d.dub && d.dub.state === 'offset') t += ` · dublagem ressincronizada (${Math.abs(d.dub.seconds).toFixed(1).replace('.', ',')} s)`;
    if (autoH.value) t += ` · qualidade automática em ${autoH.value}p (o vídeo travava)`;
    if (d.error) t += ` — ${d.error}`;
    panel.text = t;
    panel.jf = d.jellyfin_url ? jf(d.jellyfin_url) : '';
  } catch {}
}

onMounted(() => {
  const vid = v.value;
  document.documentElement.classList.add('player-on');
  document.head.appendChild(cueStyle);
  if (fill.value) vid.style.objectFit = 'cover';
  watch(fill, (f) => { vid.style.objectFit = f ? 'cover' : ''; });

  on(vid, 'waiting', () => { ui.waiting = true; if (!vid.paused && performance.now() - srcAt > 5000) stallAt = performance.now(); });
  // voltou a tocar (pulou para outro ponto, trocou a qualidade): o aviso de erro sai
  on(vid, 'playing', () => {
    ui.waiting = false;
    msg.on = false;
    if (!stallAt) return;
    const t = performance.now(), long = t - stallAt > 2500;
    stallAt = 0;
    stalls = stalls.filter((x) => t - x < 60000);
    stalls.push(t);
    if (long || stalls.length >= 3) degrade();
  });
  on(vid, 'canplay', () => { ui.waiting = false; });
  on(vid, 'play', () => { paused.value = false; if (stale) resync(); });
  on(vid, 'pause', () => { paused.value = true; poke(); if (target === null && !vid.ended) report(); });
  on(vid, 'ratechange', () => { rate.value = vid.playbackRate; });
  on(vid, 'volumechange', () => { muted.value = vid.muted || vid.volume === 0; volume.value = vid.muted ? 0 : vid.volume; });
  // um tropeço (rede, trecho ainda não baixado) costuma passar reabrindo no
  // ponto atual: o aviso só aparece se continuar falhando
  let errTries = 0, errTimer;
  on(vid, 'error', () => {
    if (!vid.src || vid.error?.code === 1) return; // 1: carregamento cancelado por nós
    ui.waiting = false;
    if (errTries < 2) { errTries++; cancel(errTimer); errTimer = later(reopen, 1500 * errTries); return; }
    msg.text = phase === 'downloading'
      ? 'Este arquivo não tocou no navegador. O download continua; quando terminar, ele abre no Jellyfin.'
      : 'Este trecho não tocou no navegador. Volte ou avance um pouco, ou abra no Jellyfin.';
    msg.on = true;
  });
  let savedPos = 0;
  on(vid, 'timeupdate', () => {
    // tocando de verdade: o aviso de erro não fica por cima
    if (!vid.paused && !vid.error) {
      msg.on = false;
      if (errTries && performance.now() - srcAt > 10000) errTries = 0;
    }
    paint();
    paintBuf();
    if (Math.abs(now() - savedPos) > 5) { savedPos = now(); store(RESUME, String(savedPos)); }
    if (!vid.paused && target === null && performance.now() - reportedAt > 15000) report();
  });
  on(vid, 'progress', paintBuf);
  on(vid, 'ended', () => {
    report(true, total());
    try { localStorage.removeItem(RESUME); } catch {}
    if (nextEp.value) goNext();
  });
  on(vid, 'contextmenu', (e) => e.preventDefault()); // segurar não abre o menu do navegador

  on(document, 'keydown', key);
  on(document, 'pointermove', (e) => { if (e.pointerType === 'mouse') poke(); });
  on(document, 'pointerdown', touchDown, true);
  on(document, 'pointermove', touchMove);
  on(document, 'pointerup', touchEnd);
  on(document, 'pointercancel', touchEnd);
  on(document, 'click', (e) => { if (!e.target.closest('.menu, .cc, .pgear')) menuOpen.value = false; });
  on(document, 'visibilitychange', () => {
    if (document.hidden) { report(true); hiddenAt = Date.now(); return; }
    if (!vid.src || Date.now() - hiddenAt < 1000) return;
    if (vid.paused) stale = true; else resync();
  });
  on(window, 'pagehide', () => report(true));

  poke();
  loadSubs();
  // /info a cada 2 s enquanto baixa; depois de pronto, a cada 10 s (em série, cada /info vai ao Sonarr)
  let infoTimer;
  const tick = async () => {
    await info();
    if (alive) infoTimer = setTimeout(tick, settled ? 10000 : 2000);
  };
  tick();
  const i1 = setInterval(loadSubs, 15000);
  offs.push(() => { clearInterval(i1); clearTimeout(infoTimer); });
});

onBeforeUnmount(() => {
  alive = false;
  report(true);
  offs.forEach((f) => f());
  timers.forEach(clearTimeout);
  const vid = v.value;
  // para o download do vídeo (e o ffmpeg do outro lado)
  vid.pause();
  vid.removeAttribute('src');
  vid.load();
  cueStyle.remove();
  document.documentElement.classList.remove('player-on');
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
});
</script>

<template>
  <div ref="root" class="player" :class="{ hidden: ui.hidden, waiting: ui.waiting, locked: ui.locked }">
    <video ref="v" playsinline preload="auto" @click="videoClick" @dblclick="videoDbl"></video>

    <div class="osd top">
      <button title="Voltar à busca" aria-label="Voltar à busca" @click="back"><svg viewBox="0 0 24 24"><path d="M20 11H7.8l5.6-5.6L12 4l-8 8 8 8 1.4-1.4L7.8 13H20z"/></svg></button>
      <div style="min-width:0;flex:1"><h1>{{ title }}</h1><small>{{ release }}</small></div>
      <button class="lock" title="Bloquear a tela (só o cadeado responde)" aria-label="Bloquear a tela" @click="lock"><svg viewBox="0 0 24 24"><path d="M18 8h-1V6A5 5 0 0 0 7 6v2H6a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V10a2 2 0 0 0-2-2zm-6 9a2 2 0 1 1 0-4 2 2 0 0 1 0 4zm3.1-9H8.9V6a3.1 3.1 0 0 1 6.2 0v2z"/></svg></button>
    </div>
    <button class="unlock" title="Desbloquear" aria-label="Desbloquear" @click="unlock"><svg viewBox="0 0 24 24"><path d="M18 8h-1V6A5 5 0 0 0 7 6v2H6a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V10a2 2 0 0 0-2-2zm-6 9a2 2 0 1 1 0-4 2 2 0 0 1 0 4zm3.1-9H8.9V6a3.1 3.1 0 0 1 6.2 0v2z"/></svg></button>

    <div class="panel">{{ panel.text }}<template v-if="panel.jf"> · <a :href="panel.jf">Abrir no Jellyfin</a></template></div>

    <!-- o clique não sobe: o "clique fora fecha" não o confere -->
    <div v-show="menuOpen" ref="menuEl" class="menu" @click.stop="poke">
      <template v-if="menuPage === 'main'">
        <button v-if="M.height" @click="menuPage = 'quality'"><span>Qualidade</span><span class="val">{{ qualityLabel }} ›</span></button>
        <button v-if="(M.audios || []).length > 1" @click="menuPage = 'audio'"><span>Áudio</span><span class="val">{{ audioName(M.audios[aud]) }} ›</span></button>
        <button @click="menuPage = 'subs'"><span>Legendas</span><span class="val">{{ subSel >= 0 ? subs[subSel].label : 'Desligada' }} ›</span></button>
        <button @click="menuPage = 'speed'"><span>Velocidade</span><span class="val">{{ rateLabel(rate) }} ›</span></button>
      </template>
      <template v-else-if="menuPage === 'quality'">
        <button @click="menuPage = 'main'"><span>‹ Voltar</span></button>
        <button :class="{ sel: quality === 'auto' }" @click="setQuality('auto')"><span>Automática · começa em {{ autoStart() }}p</span></button>
        <button v-for="h in heights" :key="h" :class="{ sel: quality === h }" @click="setQuality(h)"><span>{{ hLabel(h) }}</span></button>
        <small>A automática desce sozinha (720p → 480p → 360p) se o vídeo travar sem ser por falta de download.</small>
      </template>
      <template v-else-if="menuPage === 'audio'">
        <button @click="menuPage = 'main'"><span>‹ Voltar</span></button>
        <button v-for="a in M.audios" :key="a.n" :class="{ sel: a.n === aud }" @click="setAudio(a.n)"><span>{{ audioLabel(a) }}</span></button>
      </template>
      <template v-else-if="menuPage === 'speed'">
        <button @click="menuPage = 'main'"><span>‹ Voltar</span></button>
        <button v-for="r in RATES" :key="r" :class="{ sel: r === rate }" @click="setRate(r)"><span>{{ rateLabel(r) }}</span></button>
      </template>
      <template v-else-if="menuPage === 'subs'">
        <button @click="menuPage = 'main'"><span>‹ Voltar</span></button>
        <!-- os ajustes vêm antes da lista: um arquivo pode ter dezenas de legendas -->
        <button @click="menuPage = 'subadj'"><span>Ajustar</span><span class="val">{{ SIZES[sizeIdx][1] }} · {{ POSITIONS.find(([x]) => x === subPos)[1] }} · {{ secs(subDelay) }} ›</span></button>
        <h3>Legenda</h3>
        <button :class="{ sel: subSel < 0 }" @click="pickSub(-1)"><span>Desligada</span></button>
        <button v-for="(x, i) in subs" :key="i" :class="{ sel: i === subSel }" @click="pickSub(i)"><span>{{ x.label }}</span></button>
        <small v-if="!subs.length">Nenhuma legenda ainda. O Bazarr procura sozinho; as embutidas aparecem quando o download termina.</small>
      </template>
      <template v-else-if="menuPage === 'subadj'">
        <button @click="menuPage = 'subs'"><span>‹ Legendas</span></button>
        <h3>Tamanho</h3>
        <div class="row"><button :disabled="sizeIdx <= 0" @click="sizeStep(-1)">A−</button><span class="mid" style="text-align:center">{{ SIZES[sizeIdx][1] }}</span><button :disabled="sizeIdx >= SIZES.length - 1" @click="sizeStep(1)">A+</button></div>
        <h3>Posição</h3>
        <div class="row"><button v-for="[x, l] in POSITIONS" :key="x" :class="{ sel: subPos === x }" @click="setPos(x)"><span>{{ l }}</span></button></div>
        <h3>Atraso</h3>
        <div class="row"><button @click="delay(-0.1)">−0,1</button><button class="mid" title="Zerar" @click="delay(0)">{{ secs(subDelay) }}</button><button @click="delay(0.1)">+0,1</button></div>
        <small>+ a legenda aparece depois; − aparece antes.</small>
      </template>
    </div>

    <button v-if="nextEp" class="next" :disabled="nextBusy" @click="goNext">{{ nextBusy ? 'Preparando o próximo…' : `Próximo: ${nextEp[1]} ▶` }}</button>

    <div v-if="slow" class="slow">
      <p>{{ slow.head }}</p>
      <small v-if="slow.loading">Procurando uma versão mais leve…</small>
      <template v-else>
        <button v-for="c in slow.list" :key="c.guid" :disabled="!!slow.chosen" @click="swap(c)">
          <template v-if="slow.chosen === c.guid">Trocando…</template>
          <template v-else>Trocar por {{ c.quality }} · {{ mb(c.size) }} · {{ c.seeders }} fontes<br><small>{{ c.title }}</small></template>
        </button>
        <small v-if="!slow.list.length">{{ slow.empty }}</small>
        <small v-if="slow.error">{{ slow.error }}</small>
      </template>
      <button @click="slow = null">Continuar com esta</button>
    </div>

    <div class="hint" :class="hint.cls">{{ hint.text }}</div>
    <div class="center"><div class="spin"></div><div class="msg" :class="{ on: msg.on }">{{ msg.text }}<p v-if="msg.on && panel.jf"><a :href="panel.jf">Abrir no Jellyfin</a></p></div></div>

    <div class="osd bottom">
      <div class="timeline">
        <span>{{ time.cur }}</span>
        <div ref="bar" class="seekbar" :class="{ drag: dragging }" @pointerdown="barDown" @pointermove="barMove" @pointerup="barUp">
          <div class="track"></div>
          <div><div v-for="([a, b], i) in have" :key="'h' + i" class="have" :style="{ left: a * 100 + '%', width: (b - a) * 100 + '%' }"></div></div>
          <div><div v-for="([a, b], i) in bufs" :key="'b' + i" class="buf" :style="{ left: a * 100 + '%', width: (b - a) * 100 + '%' }"></div></div>
          <div class="played" :style="{ width: time.frac * 100 + '%' }"></div>
          <div class="knob" :style="{ left: time.frac * 100 + '%' }"></div>
          <div class="tip" :style="{ left: time.tipAt * 100 + '%' }">{{ time.tip }}</div>
        </div>
        <span>{{ time.rem }}</span>
      </div>
      <div class="controls">
        <button title="Voltar 10 s (←)" aria-label="Voltar 10 s" @click="skip(-10)"><svg viewBox="0 0 24 24"><path d="M11.99 5V1l-5 5 5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6h-2c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/><text x="12" y="16" font-size="6.5" text-anchor="middle" font-family="sans-serif">10</text></svg></button>
        <button class="big" title="Play/pausa (espaço)" aria-label="Play/pausa" @click="toggle"><svg viewBox="0 0 24 24"><path :d="paused ? 'M8 5v14l11-7z' : 'M6 19h4V5H6v14zm8-14v14h4V5h-4z'"/></svg></button>
        <button title="Avançar 10 s (→)" aria-label="Avançar 10 s" @click="skip(10)"><svg viewBox="0 0 24 24"><path d="M12.01 5V1l5 5-5 5V7c-3.31 0-6 2.69-6 6s2.69 6 6 6 6-2.69 6-6h2c0 4.42-3.58 8-8 8s-8-3.58-8-8 3.58-8 8-8z"/><text x="12" y="16" font-size="6.5" text-anchor="middle" font-family="sans-serif">10</text></svg></button>
        <span class="spacer"></span>
        <button class="cc" :class="{ on: subSel >= 0 }" title="Legendas (C)" aria-label="Legendas" @click.stop="openMenu('subs')"><svg viewBox="0 0 24 24"><path d="M19 4H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2zm-8 7H9.5v-.5h-2v3h2V13H11v1a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1v-4a1 1 0 0 1 1-1h3a1 1 0 0 1 1 1v1zm7 0h-1.5v-.5h-2v3h2V13H18v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-4a1 1 0 0 1 1-1h3a1 1 0 0 1 1 1v1z"/></svg></button>
        <button class="pgear" title="Qualidade, áudio, legendas e velocidade" aria-label="Qualidade, áudio, legendas e velocidade" @click.stop="openMenu('main')"><svg viewBox="0 0 24 24"><path d="M19.14 12.94c.04-.3.06-.61.06-.94s-.02-.64-.07-.94l2.03-1.58a.49.49 0 0 0 .12-.61l-1.92-3.32a.49.49 0 0 0-.59-.22l-2.39.96a7 7 0 0 0-1.62-.94l-.36-2.54a.48.48 0 0 0-.48-.41h-3.84a.48.48 0 0 0-.47.41l-.36 2.54a7.4 7.4 0 0 0-1.62.94l-2.39-.96a.48.48 0 0 0-.59.22L2.74 8.87a.47.47 0 0 0 .12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58a.49.49 0 0 0-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54a7 7 0 0 0 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32a.48.48 0 0 0-.12-.61l-2.01-1.58zM12 15.6a3.6 3.6 0 1 1 0-7.2 3.6 3.6 0 0 1 0 7.2z"/></svg></button>
        <button title="Som (M)" aria-label="Som" @click="v.muted = !v.muted"><svg viewBox="0 0 24 24"><path :d="muted ? 'M16.5 12A4.5 4.5 0 0 0 14 8v2.2l2.5 2.4v-.6zM19 12c0 .9-.2 1.8-.5 2.6l1.5 1.5A8.8 8.8 0 0 0 21 12a9 9 0 0 0-7-8.8v2.1a7 7 0 0 1 5 6.7zM4.3 3 3 4.3 7.7 9H3v6h4l5 5v-6.7l4.3 4.2c-.7.5-1.4.9-2.3 1.2v2.1a9 9 0 0 0 3.7-1.8l2 2 1.3-1.3-9-9L4.3 3zM12 4 9.9 6.1 12 8.2V4z' : 'M3 9v6h4l5 5V4L7 9H3zm13.5 3A4.5 4.5 0 0 0 14 8v8a4.5 4.5 0 0 0 2.5-4zM14 3.2v2.1a7 7 0 0 1 0 13.4v2.1a9 9 0 0 0 0-17.6z'"/></svg></button>
        <input class="vol" type="range" min="0" max="1" step="0.05" :value="volume" title="Volume" aria-label="Volume" @input="setVolume">
        <button class="desk" title="Imagem na imagem" aria-label="Imagem na imagem" @click="pip"><svg viewBox="0 0 24 24"><path d="M19 7h-8v6h8V7zm2-4H3a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h18a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2zm0 16H3V5h18v14z"/></svg></button>
        <button title="Tela cheia (F)" aria-label="Tela cheia" @click="full"><svg viewBox="0 0 24 24"><path d="M7 14H5v5h5v-2H7v-3zm-2-4h2V7h3V5H5v5zm12 7h-3v2h5v-5h-2v3zM14 5v2h3v3h2V5h-5z"/></svg></button>
      </div>
    </div>
  </div>
</template>

<style>
/* toque: os gestos do player (arrastar, segurar, pinça) no lugar de rolar e dar zoom na página */
html.player-on, html.player-on body { height: 100%; overflow: hidden; background: #000; touch-action: none; -webkit-touch-callout: none; user-select: none; }
.player { --played: #a855f7; --buf: rgba(250,204,21,.9); --have: rgba(255,255,255,.55); --track: rgba(255,255,255,.18); --ptext: #f2f2f2; --pmuted: rgba(255,255,255,.7);
  position: fixed; inset: 0; z-index: 10; background: #000; color: var(--ptext); font: 15px/1.4 system-ui, sans-serif; }
.player video { position: fixed; inset: 0; width: 100%; height: 100%; background: #000; }
.player .osd { position: fixed; left: 0; right: 0; transition: opacity .25s; }
.player.hidden .osd, .player.hidden .panel { opacity: 0; pointer-events: none; }
.player.hidden { cursor: none; }
.player .top { top: 0; display: flex; align-items: center; gap: 12px; padding: 16px 20px 40px; background: linear-gradient(rgba(0,0,0,.75), transparent); }
.player .top h1 { margin: 0; font-size: 18px; font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.player .top small { color: var(--pmuted); font-size: 12px; display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.player .bottom { bottom: 0; padding: 40px 20px 14px; background: linear-gradient(transparent, rgba(0,0,0,.85)); }
.player .timeline { display: flex; align-items: center; gap: 12px; font-variant-numeric: tabular-nums; font-size: 14px; }
.player .seekbar { position: relative; flex: 1; height: 22px; cursor: pointer; touch-action: none; }
.player .seekbar .track, .player .seekbar .have, .player .seekbar .buf, .player .seekbar .played { position: absolute; left: 0; top: 50%; height: 4px; margin-top: -2px; border-radius: 2px; transition: height .12s, margin .12s; }
.player .seekbar .track { right: 0; background: var(--track); }
.player .seekbar .have { background: var(--have); }
.player .seekbar .buf { background: var(--buf); }
.player .seekbar .played { background: var(--played); }
.player .seekbar .knob { position: absolute; top: 50%; width: 14px; height: 14px; margin: -7px 0 0 -7px; border-radius: 50%; background: var(--played); transform: scale(0); transition: transform .12s; }
.player .seekbar:hover .track, .player .seekbar:hover .have, .player .seekbar:hover .buf, .player .seekbar:hover .played,
.player .seekbar.drag .track, .player .seekbar.drag .have, .player .seekbar.drag .buf, .player .seekbar.drag .played { height: 6px; margin-top: -3px; }
.player .seekbar:hover .knob, .player .seekbar.drag .knob { transform: scale(1); }
.player .seekbar .tip { position: absolute; bottom: 24px; padding: 2px 6px; border-radius: 4px; background: rgba(0,0,0,.8); font-size: 12px; transform: translateX(-50%); display: none; white-space: nowrap; }
.player .seekbar:hover .tip { display: block; }
.player .controls { display: flex; align-items: center; gap: 4px; margin-top: 6px; }
.player .controls .spacer { flex: 1; }
.player button { display: inline-flex; align-items: center; justify-content: center; width: 44px; height: 44px; min-height: 0; padding: 0; border: 0; border-radius: 50%; background: transparent; color: var(--ptext); cursor: pointer; font: inherit; }
.player button:hover { background: rgba(255,255,255,.12); }
.player button svg { width: 26px; height: 26px; fill: currentColor; }
.player button.big svg { width: 34px; height: 34px; }
.player input[type=range] { width: 100px; accent-color: var(--ptext); }
.player .panel { position: fixed; top: 68px; right: 20px; max-width: min(360px, calc(100vw - 40px)); padding: 10px 14px; border-radius: 10px; background: rgba(20,20,20,.75); font-size: 13px; transition: opacity .25s; }
.player .panel a, .player .msg a { color: #7fd0ff; }
.player .cc.on { color: var(--played); }
/* o menu rola sozinho e não herda o touch-action da página: sem isto, toques
   rápidos (atraso +0,1 três vezes) viram toque duplo e o segundo se perde */
.player .menu, .player button { touch-action: manipulation; }
.player .menu { position: fixed; right: 20px; bottom: 90px; width: min(330px, calc(100vw - 24px)); max-height: 62vh; overflow: auto; padding: 6px; border-radius: 10px; background: rgba(20,20,20,.98); z-index: 5; }
.player .menu button { display: flex; justify-content: space-between; gap: 10px; width: 100%; height: auto; min-height: 42px; padding: 8px 12px; border-radius: 6px; text-align: left; }
.player .menu button.sel { color: var(--played); font-weight: 600; }
.player .menu .val { color: var(--pmuted); font-size: 13px; text-align: right; }
.player .menu small { display: block; padding: 6px 12px; color: var(--pmuted); }
.player .menu h3 { margin: 10px 12px 4px; font-size: 12px; font-weight: 600; color: var(--pmuted); text-transform: uppercase; letter-spacing: .04em; }
.player .menu .row { display: flex; align-items: center; gap: 4px; padding: 0 4px; margin: 0; flex-wrap: nowrap; }
.player .menu .row button { flex: 1; justify-content: center; text-align: center; min-height: 38px; padding: 6px; }
.player .menu .row .mid { flex: 1.6; }
.player video::cue { background: rgba(0,0,0,.65); color: #fff; font-size: clamp(16px, 2.6vw, 30px); line-height: 1.3; }
.player button.next { position: fixed; right: 20px; bottom: 120px; width: auto; height: auto; padding: 10px 16px; border-radius: 8px; background: rgba(20,20,20,.85); border: 1px solid rgba(255,255,255,.25); transition: opacity .25s; }
.player.hidden button.next { opacity: 0; pointer-events: none; }
.player .center { position: fixed; inset: 0; display: grid; place-items: center; pointer-events: none; }
.player .spin { width: 56px; height: 56px; border: 4px solid rgba(255,255,255,.2); border-top-color: var(--played); border-radius: 50%; animation: girar 1s linear infinite; display: none; }
.player.waiting .spin { display: block; }
/* total somado dos toques duplos, do lado tocado */
.player .hint { position: fixed; top: 50%; transform: translateY(-50%); padding: 10px 18px; border-radius: 24px; background: rgba(0,0,0,.55); font-size: 17px; font-weight: 600; font-variant-numeric: tabular-nums; opacity: 0; transition: opacity .2s; pointer-events: none; }
.player .hint.l { left: 10%; opacity: 1; }
.player .hint.r { right: 10%; opacity: 1; }
/* c: em cima (2× enquanto segura); m: no meio (arrasto, volume, atalhos) */
.player .hint.c { left: 50%; top: 14%; transform: translateX(-50%); opacity: 1; }
.player .hint.m { left: 50%; transform: translate(-50%, -50%); opacity: 1; }
/* tela bloqueada: só o cadeado, que aparece num toque. Embaixo: no alto,
   no centro, o Vivaldi põe o botão de imagem na imagem por cima do vídeo */
.player .unlock { display: none; position: fixed; bottom: 32px; left: 50%; transform: translateX(-50%); background: rgba(0,0,0,.55); z-index: 7; transition: opacity .25s; }
.player.locked .unlock { display: inline-flex; }
.player.locked.hidden .unlock { opacity: 0; pointer-events: none; }
.player.locked .osd, .player.locked .panel, .player.locked .menu, .player.locked button.next, .player.locked .slow { display: none !important; }
@media (hover: hover) and (pointer: fine) { .player .lock { display: none; } }
.player .msg { max-width: 520px; margin: 0 20px; padding: 18px 20px; border-radius: 12px; background: rgba(20,20,20,.9); text-align: center; pointer-events: auto; display: none; color: var(--ptext); }
.player .msg.on { display: block; }
.player .slow { position: fixed; left: 50%; top: 80px; transform: translateX(-50%); width: min(520px, calc(100vw - 32px)); padding: 14px 16px; border-radius: 12px; background: rgba(20,20,20,.94); z-index: 6; font-size: 14px; }
.player .slow p { margin: 0 0 10px; }
.player .slow button { display: block; width: 100%; height: auto; min-height: 44px; margin-top: 6px; padding: 8px 12px; border-radius: 8px; border: 1px solid rgba(255,255,255,.25); text-align: left; }
.player .slow small { color: var(--pmuted); }
@media (max-width: 600px) { .player .vol, .player .desk { display: none; } .player .bottom { padding: 30px 10px 8px; } .player .menu { right: 12px; bottom: 80px; } }
</style>
