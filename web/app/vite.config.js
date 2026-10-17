import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// o build vai para web/dist, embutido no binário (src/api.rs); `npm run dev`
// fala com o Anubis do FeuVault
const anubis = 'http://10.10.10.2:7070';
export default defineConfig({
  plugins: [vue()],
  build: { outDir: '../dist', emptyOutDir: true, assetsDir: 'assets' },
  server: {
    proxy: Object.fromEntries(
      ['/search', '/prefs', '/history', '/hidden', '/library', '/liked', '/request', '/events', '/discover', '/availability', '/indexers',
        '/jellyfin', '/requests', '/status', '/series', '/movie', '/where', '/autodub', '/health', '/icones.woff2', '/kosync']
        .map((p) => [p, anubis])
        .concat([
          // /leitura e /watch: a página é do app; o resto, da API
          ['^/leitura/.+', anubis],
          ['^/watch(/[^/]+/[^/]+/.+)?$', anubis],
        ]),
    ),
  },
});
