local _, ns = ...
local fromLib = LibGlobal.Get()
local mine = ns.secret
local taken = LibGlobal.Take(5)
UserGlobal = 1
local function passOn(value)
  return LibGlobal.Take(value)
end
passOn(1)
