--[[--
Drive (Livros): a pasta Livros do Google Drive no Kindle.

A biblioteca inteira aparece em /mnt/us/Livros. O livro ainda não baixado é
um "fantasma" (arquivo de 0 byte com o nome real, marcado ☁ na lista) e baixa
quando é aberto; depois abre sem internet. Sincroniza quando o Wi-Fi conecta,
ao acordar com Wi-Fi e pelo menu. O Drive manda: o que sai de lá sai daqui,
mas os .sdr (progresso, notas) nunca são apagados.

Regras:
- na mesma pasta, o mesmo livro em EPUB e PDF fica só em EPUB;
- livro em andamento fica preso ao arquivo que está sendo lido (nem a regra
  acima nem o controle de espaço mexem nele);
- espaço: acima de 85% de uso, os livros lidos há mais tempo voltam a
  fantasma até 75%, fora os abertos há menos de 7 dias.

O rclone (só leitura no Drive) fica em /mnt/us/extensions/drivesync.
--]]

local DocSettings = require("docsettings")
local InfoMessage = require("ui/widget/infomessage")
local NetworkMgr = require("ui/network/manager")
local ProgressbarDialog = require("ui/widget/progressbardialog")
local ReadHistory = require("readhistory")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiutil = require("ffi/util")
local lfs = require("libs/libkoreader-lfs")
local logger = require("logger")
local rapidjson = require("rapidjson")
local util = require("util")
local T = ffiutil.template

local LIB = os.getenv("DRIVESYNC_LIB") or "/mnt/us/Livros" -- outra pasta só no teste
local BIN = "/mnt/us/extensions/drivesync"
local INDEX_FILE = os.getenv("DRIVESYNC_INDEX") or (require("datastorage"):getSettingsDir() .. "/drivesync.json")
local RCLONE = "RCLONE_CONFIG=" .. BIN .. "/rclone.conf SSL_CERT_FILE=/mnt/us/koreader/data/ca-bundle.crt"
    .. " XDG_CACHE_HOME=" .. BIN .. "/cache HOME=" .. BIN .. " GOGC=50 "
    .. BIN .. "/rclone --contimeout 15s --timeout 60s --low-level-retries 3 "
local READABLE = { epub = true, pdf = true, cbz = true, mobi = true, azw3 = true, azw = true, djvu = true, fb2 = true, txt = true }
local START_PCT, STOP_PCT, MIN_AGE_DAYS = 85, 75, 7
local MIN_INTERVAL = 60 -- segundos entre sincronias automáticas

-- o módulo é carregado uma vez e serve ao gerenciador de arquivos e ao leitor
local state = { syncing = false, last = 0, downloading = {} }
local index -- { files = { [caminho local relativo] = {remote, id, size, md5, ghost} }, last_sync }

local function loadIndex()
    if index then return index end
    local f = io.open(INDEX_FILE, "r")
    if f then
        local ok, data = pcall(rapidjson.decode, f:read("*a"))
        f:close()
        if ok and type(data) == "table" and type(data.files) == "table" then index = data end
    end
    index = index or { files = {} }
    return index
end

local function saveIndex()
    local tmp = INDEX_FILE .. ".tmp"
    local f = io.open(tmp, "w")
    if not f then return end
    f:write(rapidjson.encode(index))
    f:close()
    os.rename(tmp, INDEX_FILE)
end

-- FAT não aceita : ? " * < > | \ nem espaço/ponto no fim do nome
local FAT_SAFE = { [":"] = "：", ["?"] = "？", ['"'] = "＂", ["*"] = "＊", ["<"] = "＜", [">"] = "＞", ["|"] = "｜", ["\\"] = "＼" }
local function localRel(remote)
    local parts = {}
    for part in remote:gmatch("[^/]+") do
        part = part:gsub('[:?"*<>|\\]', FAT_SAFE):gsub("[%s%.]+$", "")
        table.insert(parts, part)
    end
    return table.concat(parts, "/")
end

local function relOf(path)
    local prefix = LIB .. "/"
    if path and path:sub(1, #prefix) == prefix then return path:sub(#prefix + 1) end
end

local function dirOf(path)
    return path:match("^(.*)/[^/]+$")
end

local function isGhost(path)
    local rel = relOf(path)
    local e = rel and loadIndex().files[rel]
    return e ~= nil and e.ghost == true and lfs.attributes(path, "size") == 0
end

-- sendo lido: aberto agora, ou com progresso e não terminado/abandonado
local function isPinned(path)
    if (lfs.attributes(path, "size") or 0) == 0 then return false end
    local ReaderUI = package.loaded["apps/reader/readerui"]
    if ReaderUI and ReaderUI.instance and ReaderUI.instance.document and ReaderUI.instance.document.file == path then
        return true
    end
    if not DocSettings:hasSidecarFile(path) then return false end
    local ds = DocSettings:open(path)
    local summary = ds:readSetting("summary")
    local status = summary and summary.status
    if status then return status == "reading" end
    return ds:readSetting("percent_finished") ~= nil
end

local function forgetCover(path)
    pcall(function() require("bookinfomanager"):deleteBookInfo(path) end)
end

local function makeGhost(path)
    local f = io.open(path, "wb")
    if f then f:close() end
    forgetCover(path)
    return f ~= nil
end

local function refreshFileManager()
    local FileManager = package.loaded["apps/filemanager/filemanager"]
    if FileManager and FileManager.instance and FileManager.instance.file_chooser then
        FileManager.instance.file_chooser:refreshPath()
    end
end

-- roda um comando sem travar a tela; on_exit(código, saída, erro)
local function runBackground(cmd, on_exit)
    local tag = string.format("%s/run-%d-%d", BIN, os.time(), math.random(1, 1e6))
    os.execute(string.format("( %s > %s.out 2> %s.err; echo $? > %s.rc ) &", cmd, tag, tag, tag))
    local function slurp(name)
        local f = io.open(name, "r")
        if not f then return "" end
        local s = f:read("*a")
        f:close()
        os.remove(name)
        return s
    end
    local poll
    poll = function()
        if not lfs.attributes(tag .. ".rc") then
            UIManager:scheduleIn(1, poll)
            return
        end
        local rc = tonumber(slurp(tag .. ".rc"))
        on_exit(rc, slurp(tag .. ".out"), slurp(tag .. ".err"))
    end
    UIManager:scheduleIn(1, poll)
end

local function lastLine(s)
    return (s or ""):gsub("%s+$", ""):match("[^\n]*$") or ""
end

-- Espaço: acima de START_PCT, devolve livros a fantasma até STOP_PCT.
local function usedPct()
    local u = util.diskUsage(LIB)
    if u.total and u.total > 0 then return u.used / u.total * 100 end
end

local function freeSpace()
    local pct = usedPct()
    if not pct or pct <= START_PCT then return 0 end
    local last = {}
    for _, h in ipairs(ReadHistory.hist or {}) do last[h.file] = h.time end
    local now, cands = os.time(), {}
    for rel, e in pairs(loadIndex().files) do
        local path = LIB .. "/" .. rel
        if not e.ghost and not isPinned(path) and now - (last[path] or 0) > MIN_AGE_DAYS * 86400 then
            table.insert(cands, { path = path, e = e, t = last[path] or 0 })
        end
    end
    table.sort(cands, function(a, b) return a.t < b.t end)
    local n = 0
    for _, c in ipairs(cands) do
        if (usedPct() or 0) <= STOP_PCT then break end
        if makeGhost(c.path) then
            c.e.ghost = true
            n = n + 1
        end
    end
    if n > 0 then saveIndex() end
    return n
end

-- Livros que já estavam no Kindle (de antes do plugin, ou de antes de uma
-- reorganização no Drive) vão para o caminho do Drive junto com o .sdr.
local function moveBook(from, to)
    util.makePath(dirOf(to))
    DocSettings.updateLocation(from, to) -- leva o .sdr (progresso, notas)
    os.rename(from, to)
    ReadHistory:updateItem(from, to)
    if G_reader_settings:readSetting("lastfile") == from then
        G_reader_settings:saveSetting("lastfile", to)
    end
end

local function scanLocal()
    local files = {}
    local function walk(dir)
        for name in lfs.dir(dir) do
            if name ~= "." and name ~= ".." and not name:match("%.sdr$") and not name:match("^%.") then
                local p = dir .. "/" .. name
                local mode = lfs.attributes(p, "mode")
                if mode == "directory" then
                    walk(p)
                elseif mode == "file" and not name:match("%.part$") then
                    table.insert(files, p)
                end
            end
        end
    end
    if lfs.attributes(LIB, "mode") == "directory" then walk(LIB) end
    return files
end

local function md5sum(path)
    local h = io.popen("md5sum " .. util.shell_escape({ path }) .. " 2>/dev/null")
    if not h then return end
    local out = h:read("*a")
    h:close()
    return out:match("^(%x+)")
end

local function entry(r, ghost)
    return { remote = r.Path, id = r.ID, size = r.Size, md5 = r.Hashes and r.Hashes.md5, ghost = ghost }
end

local function migrate(remote)
    local idx = loadIndex()
    local byName, bySize = {}, {}
    for _, r in ipairs(remote) do
        byName[r.Path:match("([^/]+)$") .. "\0" .. r.Size] = r
        bySize[r.Size] = bySize[r.Size] or {}
        table.insert(bySize[r.Size], r)
    end
    local moved = 0
    for _, path in ipairs(scanLocal()) do
        local rel = relOf(path)
        local size = lfs.attributes(path, "size") or 0
        if not idx.files[rel] and size > 0 then
            local r = byName[rel:match("([^/]+)$") .. "\0" .. size]
            if not r and bySize[size] and #bySize[size] == 1 then
                -- mesmo tamanho, outro nome: só se o conteúdo for igual
                local cand = bySize[size][1]
                if cand.Hashes and cand.Hashes.md5 and md5sum(path) == cand.Hashes.md5 then r = cand end
            end
            if r then
                local lrel = localRel(r.Path)
                local dest = LIB .. "/" .. lrel
                if dest ~= path and not lfs.attributes(dest) then
                    moveBook(path, dest)
                    moved = moved + 1
                end
                if lfs.attributes(dest, "size") == size and not idx.files[lrel] then
                    idx.files[lrel] = entry(r, false)
                end
            end
        end
    end
    return moved
end

-- mesma pasta + mesmo título (sem extensão, autor entre parênteses e sufixos
-- do Z-Library): é o mesmo livro em formatos diferentes
local function sameBookKey(rel)
    local dir, name = rel:match("^(.*)/([^/]+)$")
    if not dir then dir, name = "", rel end
    local base = name:gsub("%.[^.]*$", ""):lower()
    base = base:gsub("%b()", ""):gsub("_z_library_sk.*$", ""):gsub("[%s%p]", "")
    return dir .. "/" .. base
end

local function isEpub(r) return r.Path:lower():match("%.epub$") ~= nil end

local function chooseWanted(remote)
    local groups = {}
    for _, r in ipairs(remote) do
        local ext = r.Path:match("%.([^./]+)$")
        if ext and READABLE[ext:lower()] and (r.Size or 0) > 0 then
            local k = sameBookKey(r.Path)
            groups[k] = groups[k] or {}
            table.insert(groups[k], r)
        end
    end
    local wanted = {}
    for _, g in pairs(groups) do
        local pinned = false
        for _, r in ipairs(g) do
            local lrel = localRel(r.Path)
            if isPinned(LIB .. "/" .. lrel) then
                wanted[lrel] = r
                pinned = true
            end
        end
        if not pinned then
            table.sort(g, function(a, b)
                if isEpub(a) ~= isEpub(b) then return isEpub(a) end
                return (a.Size or 0) > (b.Size or 0)
            end)
            wanted[localRel(g[1].Path)] = g[1]
        end
    end
    return wanted
end

local function apply(remote)
    local idx = loadIndex()
    local stats = { new = 0, removed = 0, moved = migrate(remote) }
    local wanted = chooseWanted(remote)
    for rel in pairs(idx.files) do
        if not wanted[rel] then
            local path = LIB .. "/" .. rel
            if not isPinned(path) then -- o que está sendo lido sai só quando terminar
                if lfs.attributes(path, "mode") == "file" then
                    os.remove(path)
                    forgetCover(path)
                end
                idx.files[rel] = nil
                stats.removed = stats.removed + 1
            end
        end
    end
    for rel, r in pairs(wanted) do
        local path = LIB .. "/" .. rel
        local e = idx.files[rel]
        local attr = lfs.attributes(path)
        if not attr then
            util.makePath(dirOf(path))
            if makeGhost(path) then
                idx.files[rel] = entry(r, true)
                stats.new = stats.new + 1
            else
                logger.warn("drivesync: não consegui criar", path)
            end
        elseif e then
            local md5 = r.Hashes and r.Hashes.md5
            if not e.ghost and md5 and e.md5 and md5 ~= e.md5 and not isPinned(path) then
                e.ghost = makeGhost(path) -- mudou no Drive: baixa de novo ao abrir
            end
            e.remote, e.id, e.size, e.md5 = r.Path, r.ID, r.Size, md5
        elseif attr.size == r.Size then
            idx.files[rel] = entry(r, false) -- já estava aqui, igual
        elseif attr.size == 0 then
            idx.files[rel] = entry(r, true) -- fantasma de um índice perdido
        end
        -- outro arquivo com o mesmo nome, fora do índice: não é do plugin, fica
    end
    idx.last_sync = os.time()
    saveIndex()
    return stats
end

local function sync(manual)
    if state.syncing then return end
    if not manual and os.time() - state.last < MIN_INTERVAL then return end
    if not NetworkMgr:isOnline() then
        if manual then UIManager:show(InfoMessage:new{ text = "Sem internet.", timeout = 3 }) end
        return
    end
    state.syncing = true
    if manual then UIManager:show(InfoMessage:new{ text = "Sincronizando com o Drive…", timeout = 2 }) end
    runBackground(RCLONE .. "lsjson -R --files-only --hash --hash-type md5 --no-mimetype livros:", function(rc, out, err)
        state.syncing = false
        state.last = os.time()
        local ok, remote = pcall(rapidjson.decode, out)
        if rc ~= 0 or not ok or type(remote) ~= "table" then
            logger.warn("drivesync: listagem falhou", rc, err)
            if manual then
                UIManager:show(InfoMessage:new{ text = "Não consegui listar o Drive:\n" .. lastLine(err), timeout = 6 })
            end
            return
        end
        local stats = apply(remote)
        local freed = freeSpace()
        refreshFileManager()
        local parts = {}
        if stats.new > 0 then table.insert(parts, stats.new .. " novos") end
        if stats.removed > 0 then table.insert(parts, stats.removed .. " removidos") end
        if stats.moved > 0 then table.insert(parts, stats.moved .. " reorganizados") end
        if freed > 0 then table.insert(parts, freed .. " devolvidos à nuvem (espaço)") end
        if #parts > 0 or manual then
            UIManager:show(InfoMessage:new{
                text = "Drive: " .. (#parts > 0 and table.concat(parts, ", ") or "nada mudou"),
                timeout = 3,
            })
        end
    end)
end

local function download(path, on_done)
    local rel = relOf(path)
    local e = rel and loadIndex().files[rel]
    if not e or state.downloading[path] then return end
    state.downloading[path] = true
    local part = path .. ".part"
    local dlg = ProgressbarDialog:new{ title = "Baixando do Drive", subtitle = rel:match("([^/]+)$"), progress_max = e.size }
    dlg:show()
    local poll
    poll = function()
        if not state.downloading[path] then return end
        dlg:reportProgress(lfs.attributes(part, "size") or 0)
        UIManager:scheduleIn(1, poll)
    end
    UIManager:scheduleIn(1, poll)
    runBackground(RCLONE .. "copyto " .. util.shell_escape({ "livros:" .. e.remote, part }), function(rc, _, err)
        state.downloading[path] = nil
        dlg:close()
        if rc == 0 and lfs.attributes(part, "size") == e.size then
            os.remove(path)
            os.rename(part, path)
            e.ghost = false
            saveIndex()
            forgetCover(path)
            freeSpace()
            on_done()
        else
            os.remove(part)
            logger.warn("drivesync: download falhou", rc, err)
            UIManager:show(InfoMessage:new{ text = "Não consegui baixar do Drive.\n" .. lastLine(err), timeout = 6 })
        end
    end)
end

-- Uma vez só: abrir um fantasma baixa antes, e a lista mostra ☁ e o tamanho.
local hooked = false
local function hook()
    if hooked then return end
    hooked = true
    local ReaderUI = require("apps/reader/readerui")
    local showReader = ReaderUI.showReader
    ReaderUI.showReader = function(self, file, ...)
        if not isGhost(file) then return showReader(self, file, ...) end
        local args, n = { ... }, select("#", ...)
        NetworkMgr:runWhenOnline(function()
            download(file, function() showReader(self, file, unpack(args, 1, n)) end)
        end)
    end
    local FileChooser = require("ui/widget/filechooser")
    local mandatory = FileChooser.getMenuItemMandatory
    FileChooser.getMenuItemMandatory = function(fc, item, collate)
        local text = mandatory(fc, item, collate)
        if collate and item.path and isGhost(item.path) then
            local e = loadIndex().files[relOf(item.path)]
            text = "☁ " .. util.getFriendlySize(e.size or 0)
        end
        return text
    end
end

-- Progresso único: configura o kosync uma vez, com o arquivo deixado pela
-- instalação (servidor, usuário e chave), e o apaga.
local function bootstrapKosync()
    local file = BIN .. "/kosync-bootstrap.lua"
    if not lfs.attributes(file) then return end
    local ok, boot = pcall(dofile, file)
    if ok and type(boot) == "table" then
        local s = G_reader_settings:readSetting("kosync") or {}
        for k, v in pairs(boot) do s[k] = v end -- mesma tabela que o kosync já usa
        G_reader_settings:saveSetting("kosync", s)
        -- sem isso o kosync desliga a sincronia automática no Kindle
        G_reader_settings:saveSetting("wifi_enable_action", "turn_on")
        G_reader_settings:flush()
        os.remove(file)
        logger.info("drivesync: kosync configurado")
    end
end

local DriveSync = WidgetContainer:extend{
    name = "drivesync",
    is_doc_only = false,
}

function DriveSync:init()
    hook()
    bootstrapKosync()
    self.ui.menu:registerToMainMenu(self)
    if not self.ui.document then -- ao abrir o gerenciador de arquivos
        UIManager:nextTick(function() sync(false) end)
    end
end

function DriveSync:onNetworkConnected()
    sync(false)
end

function DriveSync:onResume()
    if NetworkMgr:isWifiOn() then
        UIManager:scheduleIn(5, function() sync(false) end)
    end
end

function DriveSync:addToMainMenu(menu_items)
    menu_items.drivesync = {
        text = "Drive (Livros)",
        sorting_hint = "tools",
        sub_item_table = {
            {
                text = "Sincronizar agora",
                callback = function() sync(true) end,
            },
            {
                text_func = function()
                    local idx = loadIndex()
                    local total, local_n = 0, 0
                    for _, e in pairs(idx.files) do
                        total = total + 1
                        if not e.ghost then local_n = local_n + 1 end
                    end
                    local when = idx.last_sync and os.date("%d/%m %H:%M", idx.last_sync) or "nunca"
                    return T("%1 livros, %2 no Kindle · última sincronia: %3", total, local_n, when)
                end,
                enabled_func = function() return false end,
            },
        },
    }
end

-- para o teste fora do Kindle
DriveSync._test = { apply = apply, localRel = localRel, sameBookKey = sameBookKey }

return DriveSync
