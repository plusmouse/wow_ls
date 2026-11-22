local addonName, ns = ...

ns.Version = "1.0"
ns.Settings = { size = 10 }

function ns:GetName()
  return addonName
end

TestAddonGlobal = {}

function TestAddonGlobal.Count()
  return 1
end
