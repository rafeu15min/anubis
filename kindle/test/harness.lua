-- Roda a sincronia do drivesync fora do Kindle, sobre uma cópia da pasta
-- Livros, com dublês das partes do KOReader. Uso:
--   DRIVESYNC_LIB=<cópia> DRIVESYNC_INDEX=<arq> luajit harness.lua <main.lua> <remote.lua>

local function sh(cmd)
    local h = io.popen(cmd)
    local out = h:read("*a")
    h:close()
    return out
end
local function q(s) return "'" .. s:gsub("'", "'\\''") .. "'" end

local lfs = {}
function lfs.attributes(path, field)
    local out = sh("stat -c '%F|%s' " .. q(path) .. " 2>/dev/null")
    if out == "" then return nil end
    local kind, size = out:match("^(.-)|(%d+)")
    local a = { mode = kind:match("directory") and "directory" or "file", size = tonumber(size) }
    if field then return a[field] end
    return a
end
function lfs.dir(path)
    local names = {}
    for n in sh("ls -A " .. q(path)):gmatch("[^\n]+") do table.insert(names, n) end
    local i = 0
    return function() i = i + 1; return names[i] end
end

local moves, history = {}, { hist = {} }
local function sdrOf(path) return (path:gsub("%.[^./]+$", "")) .. ".sdr" end
local DocSettings = {}
function DocSettings:hasSidecarFile(path) return lfs.attributes(sdrOf(path), "mode") == "directory" end
function DocSettings:open(path)
    local data = {}
    local meta = sh("ls " .. q(sdrOf(path)) .. "/metadata.*.lua 2>/dev/null"):match("[^\n]+")
    if meta then data = dofile(meta) end
    return { readSetting = function(_, k) return data[k] end }
end
function DocSettings.updateLocation(from, to)
    if lfs.attributes(sdrOf(from)) then
        os.execute("mv " .. q(sdrOf(from)) .. " " .. q(sdrOf(to)))
        table.insert(moves, sdrOf(from) .. " -> " .. sdrOf(to))
    end
end
function history:updateItem(from, to) table.insert(moves, "historico: " .. to) end

G_reader_settings = { readSetting = function() end, saveSetting = function() end, flush = function() end }

local stub = function(t) return function() return t end end
package.preload["libs/libkoreader-lfs"] = stub(lfs)
package.preload["docsettings"] = stub(DocSettings)
package.preload["readhistory"] = stub(history)
package.preload["logger"] = stub({ warn = print, info = print, dbg = function() end })
package.preload["rapidjson"] = stub({ encode = function() return "{}" end, decode = function() return {} end })
package.preload["util"] = stub({
    makePath = function(p) os.execute("mkdir -p " .. q(p)) end,
    diskUsage = function() return {} end,
    shell_escape = function(args) local t = {} for _, a in ipairs(args) do table.insert(t, q(a)) end return table.concat(t, " ") end,
    getFriendlySize = function(n) return tostring(n) end,
})
package.preload["ffi/util"] = stub({ template = function(s) return s end })
for _, m in ipairs({ "ui/widget/infomessage", "ui/network/manager", "ui/widget/progressbardialog", "ui/uimanager" }) do
    package.preload[m] = stub({})
end
package.preload["ui/widget/container/widgetcontainer"] = stub({ extend = function(_, t) return t end })

local plugin = dofile(arg[1])
local remote = dofile(arg[2])
local stats = plugin._test.apply(remote)
print(string.format("novos(fantasmas) %d | removidos %d | reorganizados %d", stats.new, stats.removed, stats.moved))
for _, m in ipairs(moves) do print("  " .. m) end
