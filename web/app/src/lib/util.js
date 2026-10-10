// Utilidades sem tela.

export const pct = (p) => `${Math.round(p * 100)}%`;
export const size = (b) => (b >= 2 ** 30 ? `${(b / 2 ** 30).toFixed(1).replace('.', ',')} GB` : `${Math.round(b / 2 ** 20)} MB`);

// o Jellyfin pelo mesmo caminho desta página (ele escuta nos mesmos endereços)
export const jf = (u) => { try { const x = new URL(u); x.hostname = location.hostname; return x.href; } catch { return u; } };

// localStorage pode faltar (aba anônima, dados bloqueados): nunca quebra a tela
export const stored = (k, d = null) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } };
export const store = (k, v) => { try { localStorage.setItem(k, String(v)); } catch {} };
export const storedJson = (k, d) => { try { return JSON.parse(localStorage.getItem(k)) ?? d; } catch { return d; } };
export const storeJson = (k, v) => { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} };

// GET/POST/… com JSON; erro do servidor vira exceção com a mensagem dele
export async function api(url, { method = 'GET', body, type } = {}) {
  const opts = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': type || 'application/json' };
    opts.body = type ? body : JSON.stringify(body);
  }
  const r = await fetch(url, opts);
  let d = null;
  try { d = await r.json(); } catch {}
  if (!r.ok) throw Object.assign(new Error((d && d.error) || `HTTP ${r.status}`), { status: r.status, data: d });
  return d;
}

export const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
