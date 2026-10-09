# Kindle: pasta Livros do Drive + progresso único

`drivesync.koplugin` é um plugin do KOReader. A biblioteca inteira de
`Meu Drive/Livros` aparece em `/mnt/us/Livros`; o livro não baixado é um
"fantasma" (0 byte, ☁ na lista) que baixa ao abrir. Sincroniza quando o
Wi-Fi conecta. Regras e detalhes no cabeçalho de `main.lua`.

## Instalação (Kindle no USB)

- `koreader/plugins/drivesync.koplugin/` — este diretório.
- `extensions/drivesync/rclone` — rclone linux-arm-v7 (estático).
- `extensions/drivesync/rclone.conf` — remoto `livros:` só leitura
  (`scope = drive.readonly`, `root_folder_id` = pasta Livros), autorizado com
  o client_id próprio (`rclone authorize drive ID SECRET --drive-scope drive.readonly`).
- `extensions/drivesync/kosync-bootstrap.lua` — opcional: configura o
  "Progress sync" do KOReader para o `/kosync` do Anubis na primeira
  abertura e é apagado.

## Teste fora do Kindle

```sh
rclone lsjson -R --files-only --hash --hash-type md5 --no-mimetype livros: > remote.json
# converter remote.json para uma tabela Lua (remote.lua), copiar a pasta Livros, e:
LC_ALL=C DRIVESYNC_LIB=./Livros DRIVESYNC_INDEX=./index.json luajit test/harness.lua drivesync.koplugin/main.lua remote.lua
```
