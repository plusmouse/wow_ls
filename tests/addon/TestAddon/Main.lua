local _, ns = ...

local version = ns.Version
local size = ns.Settings.size
local name = ns:GetName()
local rounded = ns.Round(1.5)
local count = TestAddonGlobal.Count()
local private = Private
local shown = TestAddonFrames.shown
