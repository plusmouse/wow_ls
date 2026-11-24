local _, ns = ...
ns.secret = 1
LibGlobal = {}
function LibGlobal.Get()
  return "s"
end
function LibGlobal.Take(value)
  return value
end
