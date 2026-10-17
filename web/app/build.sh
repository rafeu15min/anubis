#!/bin/sh
# Monta o front-end (web/app → web/dist) num contêiner Node: nada instalado na
# máquina; as dependências ficam no volume anubis-web-modules. Deixa um .gz ao
# lado de cada .js e .css (o servidor entrega esse quando o navegador aceita).
set -e
cd "$(dirname "$0")/.."
docker volume inspect anubis-web-modules >/dev/null 2>&1 || {
  docker volume create anubis-web-modules >/dev/null
  docker run --rm -v anubis-web-modules:/m node:26-slim chown "$(id -u):$(id -g)" /m
}
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD:/web" -v anubis-web-modules:/web/app/node_modules -w /web/app node:26-slim \
  sh -c 'npm ci --no-fund --no-audit --loglevel=error && npm run build && find /web/dist/assets -type f \( -name "*.js" -o -name "*.css" \) -exec gzip -9 -n -k -f {} +'
