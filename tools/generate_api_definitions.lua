--Copyright (C) 2025-  plusmouse and other contributors
--
--This program is free software: you can redistribute it and/or modify
--it under the terms of the GNU General Public License as published by
--the Free Software Foundation, either version 3 of the License, or
--(at your option) any later version.
--
--This program is distributed in the hope that it will be useful,
--but WITHOUT ANY WARRANTY; without even the implied warranty of
--MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
--GNU General Public License for more details.
--
--You should have received a copy of the GNU General Public License
--along with this program.  If not, see <https://www.gnu.org/licenses/>.

-- Generates wow_ls definitions from Blizzard's generated API documentation
-- (Interface/AddOns/Blizzard_APIDocumentationGenerated in wow-ui-source).
--
-- Usage:
--   lua5.1 tools/generate_api_definitions.lua definitions/blizzard_api.json Blizzard_APIDocumentationGenerated/*.lua
--
-- The documentation files are Lua, so they're run with stub globals and the
-- tables they register are converted. Type names are mapped by the tables
-- below; any name they don't cover becomes `unknown` and is reported on stderr,
-- so new documentation shows what needs mapping.
--
-- Generated: functions, the `Enum` table (each enumeration's members typed as
-- that enumeration, `Enum.X`), the `Constants` table, and each documented
-- method table (`method_tables`, from the ScriptObject documentation, like
-- SimpleFrameAPI). Which tables make up which object type isn't documented, so
-- that's in the hand-maintained definitions/wow_objects.json. Events are
-- skipped, as wow_ls can't use them yet.

local output_path = arg[1]
if not output_path or #arg < 2 then
	io.stderr:write("usage: lua5.1 generate_api_definitions.lua <output.json> <documentation files...>\n")
	os.exit(1)
end

------------------------------------------------------------------------------
-- Type names. Every entry here was reviewed by hand; add to them rather than
-- editing the generated JSON.
------------------------------------------------------------------------------

-- Names that stand for a plain Lua type, shown as that type (`cstring` is just
-- a string). Declared enumerations are their `Enum.X` number type and callback
-- types are functions; those are handled by rule rather than listed.
local LUA_TYPES = {
	number = {
		"number", "luaIndex", "fileID", "BigUInteger", "BigInteger", "time_t", "Seconds", "Milliseconds",
		"FrameTime", "size", "normalizedValue", "SingleColorValue", "WOWMONEY", "uiFontHeight", "SoundHandle",
		"UnitCastBarID", "EncounterTimelineEventID", "VoiceChatID", "ClubMemberOpaqueId",
	},
	boolean = { "bool" },
	string = {
		"string", "cstring", "stringView", "WOWGUID", "UnitToken", "UnitTokenPvPRestrictedForAddOns",
		"UnitTokenRestrictedForAddOns", "UnitTokenNamePlate", "uiUnit", "textureKit", "textureAtlas",
		"kstringLfgListApplicant", "kstringLfgListSearch", "kstringClubMessage", "kstringAuroraName",
		"KStringDiscordUserName", "ClubId", "ClubStreamId", "ClubInvitationId", "FramePoint", "DrawLayer",
		"JustifyHorizontal", "JustifyVertical", "FrameStrata", "BlendMode", "Orientation", "FilterMode",
		"LoopType", "SmoothingType", "TBFFlags", "HTMLTextType", "ScriptTypeName", "mouseButton", "MouseButton",
		"ClickButton", "SimpleButtonStateToken", "AuraFilters", "SendChatMessageType", "UISoundSubType",
	},
	-- An ID, or a name, link or file path
	["number|string"] = {
		"ItemInfo", "SpellIdentifier", "IDOrLink", "uiAddon", "FileAsset", "TextureAsset", "TextureAssetDisk",
		"ModelAsset", "FontAsset",
	},
	table = { "table" },
	["function"] = { "LuaFunctionReference" },
	unknown = { "any", "LuaValueReference", "LuaValueVariant", "LuaCurveEvaluatedResult" },
}

-- Names whose underlying type couldn't be confirmed, so they stay unknown
-- rather than risk being wrong. Listed so they aren't reported as unmapped.
local UNCONFIRMED = {
	-- Probably undeclared enumerations (numbers)
	"StoreError", "AccountStoreState", "AccountStoreItemStatus", "AccountStoreItemMode", "AccountStoreItemFlag",
	"AccountStoreCategoryType", "NeighborhoodType", "NeighborhoodOwnerType", "ResidentType",
	"PurchaseHouseDisabledReason", "InvalidPlotScreenshotReason", "HouseSettingFlags",
	"WeeklyRewardChestThresholdType", "CachedRewardType", "ItemCreationContext", "ItemSoundType",
	"Vocalerrorsounds", "GameRule", "GameMode", "InventorySlots", "LuaInventorySlot", "AnimationDataEnum",
	"ArtifactTiers", "PartyPlaylistEntry", "CurveType", "InsertMode", "FontAlphabet", "QuestObjectiveType",
	"ConnectionProtocol", "ConnectionIptype",
	-- 64-bit IDs that might be strings or numbers
	"CalendarEventID", "WeeklyRewardItemDBID", "RecruitAcceptanceID", "DiscordMemberOpaqueID", "NotificationDbId",
	-- A follower ID can be a number or a GUID string
	"GarrisonFollower",
}

-- WoW objects with their own type name. Widgets use the name GetObjectType()
-- returns rather than the documentation's `Simple...` name.
local WIDGETS = {
	SimpleFrame = "Frame", SimpleTexture = "Texture", SimpleMaskTexture = "MaskTexture", SimpleLine = "Line",
	SimpleFontString = "FontString", SimpleFont = "Font", SimpleRegion = "Region", SimpleWindow = "Window",
	SimpleCheckbox = "CheckButton", SimpleAnimGroup = "AnimationGroup", SimpleAnim = "Animation",
	SimplePathAnim = "Path", SimpleControlPoint = "ControlPoint", SimpleVectorGraphics = "VectorGraphics",
	ScriptRegion = "ScriptRegion", FrameScriptObject = "FrameScriptObject", CScriptObject = "ScriptObject",
	CooldownFrame = "Cooldown", Tooltip = "GameTooltip", ModelSceneFrame = "ModelScene",
	ModelSceneFrameActor = "ModelSceneActor", NamePlateFrame = "NamePlateFrame",
}

-- Objects implemented in C rather than as tables. Their documentation tables
-- are marked `ObjectType = "Userdata"`, which is checked below.
local NATIVE_OBJECTS = {
	"LuaCurveObject", "LuaCurveObjectBase", "LuaColorCurveObject", "LuaDurationObject", "LuaDurationClock",
	"LuaDurationManualClock", "LuaLocaleContext", "NumericFormatter", "NumericRuleFormatter", "SecondsFormatter",
	"AbbreviatedNumberFormatter", "AbbreviateConfig", "DurationTextBinding", "HousingCatalogSearcher",
	"TimedSignalMap", "UnitHealPredictionCalculator",
}

-- Tables made from one of Blizzard's mixins, like ItemLocationMixin
local MIXIN_TABLES = {
	"ItemLocation", "AzeriteItemLocation", "AzeriteEmpoweredItemLocation", "PlayerLocation", "TransmogLocation",
	"ItemTransmogInfo", "UiMapPoint", "TooltipComparisonItem", "ReportInfo", "vector2", "vector3", "colorRGB",
	"colorRGBA", "uiRect",
}

-- Data tables the documentation uses but never declares as structures. Named
-- like declared structures are.
local UNDECLARED_STRUCTURES = { "TooltipData", "AuraData" }

-- Documentation names that are treated as another type.
local MERGED = {
	-- An ItemLocation that's allowed to point at an empty slot (C_Item.DoesItemExist
	-- takes one). At runtime it's the same ItemLocationMixin table, so code passes
	-- one ItemLocation to both kinds of function.
	EmptiableItemLocation = "ItemLocation",
}

------------------------------------------------------------------------------
-- Loading the documentation
------------------------------------------------------------------------------

-- Stands in for any global the documentation refers to (Enum.X.Y, Constants...),
-- including in arithmetic
local stub
local function same() return stub end
stub = setmetatable({}, {
	__index = same, __call = same, __add = same, __sub = same, __mul = same, __div = same, __mod = same,
	__unm = same, __pow = same, __concat = same,
})

local documents = {}
local environment = setmetatable({
	APIDocumentation = { AddDocumentationTable = function(_, document) documents[#documents + 1] = document end },
}, { __index = same })

for i = 2, #arg do
	local chunk = assert(loadfile(arg[i]))
	setfenv(chunk, environment)
	chunk()
end

------------------------------------------------------------------------------
-- Type conversion
------------------------------------------------------------------------------

local lua_types = {}
for lua_type, names in pairs(LUA_TYPES) do
	for _, name in ipairs(names) do lua_types[name] = lua_type end
end
for _, name in ipairs(UNCONFIRMED) do lua_types[name] = "unknown" end

-- Named types written to the output: name -> "table" | "userdata"
local named_types = {}
-- Documentation name -> wow_ls name, for names that become named types
local named = {}
local function add_named(documentation_name, name, base)
	if named_types[name] and named_types[name] ~= base then
		error(("%s is declared as both %s and %s"):format(name, named_types[name], base))
	end
	named_types[name] = base
	named[documentation_name] = name
end

for documentation_name, name in pairs(WIDGETS) do add_named(documentation_name, name, "table") end
for _, name in ipairs(NATIVE_OBJECTS) do add_named(name, name, "userdata") end
for _, name in ipairs(MIXIN_TABLES) do add_named(name, name, "table") end
for _, name in ipairs(UNDECLARED_STRUCTURES) do add_named(name, name, "table") end

local declared = {}
-- Name -> the documentation table, for the `Enum` and `Constants` tables
local enumerations, constants = {}, {}
for _, document in ipairs(documents) do
	for _, declaration in ipairs(document.Tables or {}) do
		declared[declaration.Name] = declaration.Type
		if declaration.Type == "Structure" then
			add_named(declaration.Name, declaration.Name, "table")
		elseif declaration.Type == "Enumeration" then
			enumerations[declaration.Name] = declaration
			-- A number type, for its members and for arguments and returns
			local name = "Enum." .. declaration.Name
			if named_types[name] then error(name .. " is declared twice") end
			named_types[name] = "number"
		elseif declaration.Type == "Constants" then
			constants[declaration.Name] = declaration
		end
	end
end

-- Check the native objects against the documentation's own marking
local userdata_documents = {}
for _, document in ipairs(documents) do
	if document.ObjectType == "Userdata" then userdata_documents[document.Name] = true end
end
for _, name in ipairs(NATIVE_OBJECTS) do
	local documentation = name:gsub("Object$", "") .. "ObjectAPI"
	if not (userdata_documents[name .. "API"] or userdata_documents[documentation]) then
		io.stderr:write(("note: %s isn't documented as userdata\n"):format(name))
	end
end

local unmapped = {}
local function convert(documentation_name)
	documentation_name = MERGED[documentation_name] or documentation_name
	if named[documentation_name] then return named[documentation_name] end
	if lua_types[documentation_name] then return lua_types[documentation_name] end
	local kind = declared[documentation_name]
	if kind == "Enumeration" then return "Enum." .. documentation_name end
	if kind == "CallbackType" then return "function" end
	unmapped[documentation_name] = (unmapped[documentation_name] or 0) + 1
	return "unknown"
end

-- The wow_ls type of an argument or return value
local function field_type(field, optional)
	local converted = convert(field.Type)
	if (optional or field.Nilable or field.Default ~= nil) and converted ~= "unknown" then
		converted = converted .. "|nil"
	end
	return converted
end

------------------------------------------------------------------------------
-- Functions
------------------------------------------------------------------------------

-- Global functions, and the functions of each namespace table (C_AddOns...)
local functions = {}
local namespaces = {}
local skipped = { secure = 0, duplicate = 0 }

-- A function's parameters and returns, in the definitions' form
local function convert_function(documentation)
	local parameters = {}
	for _, argument in ipairs(documentation.Arguments or {}) do
		local types = field_type(argument)
		-- Arguments with a StrideIndex repeat, so they're the varargs
		local parameter_name = argument.StrideIndex and "..." or argument.Name
		parameters[#parameters + 1] = types == "unknown" and parameter_name or (parameter_name .. ": " .. types)
		if argument.StrideIndex then break end
	end
	local returns, rest = {}, nil
	for _, value in ipairs(documentation.Returns or {}) do
		local types = field_type(value, documentation.MayReturnNothing)
		if value.StrideIndex then
			-- Repeating results, as many as there are
			rest = types == "unknown" and "unknown" or (types:gsub("|nil$", "") .. "|nil")
			break
		end
		returns[#returns + 1] = types
	end
	return { parameters = parameters, returns = returns, rest = rest }
end

local function add_function(namespace, name, documentation)
	local scope = functions
	if namespace then
		namespaces[namespace] = namespaces[namespace] or {}
		scope = namespaces[namespace]
	end
	if scope[name] then
		skipped.duplicate = skipped.duplicate + 1
		io.stderr:write(("note: %s%s is documented twice; keeping the first\n"):format(namespace and (namespace .. ".") or "", name))
		return
	end
	scope[name] = convert_function(documentation)
end

for _, document in ipairs(documents) do
	if document.Type == "System" then
		for _, documentation in ipairs(document.Functions or {}) do
			if document.Environment == "SecureOnly" then
				-- Blizzard's own secure code only; addons can't call these
				skipped.secure = skipped.secure + 1
			else
				add_function(document.Namespace, documentation.Name, documentation)
			end
		end
	end
end

-- Each documented method table, as is
local method_tables = {}
for _, document in ipairs(documents) do
	if document.Type == "ScriptObject" then
		if method_tables[document.Name] then error(document.Name .. " is documented twice") end
		method_tables[document.Name] = {}
		for _, documentation in ipairs(document.Functions or {}) do
			if document.Environment ~= "SecureOnly" then
				method_tables[document.Name][documentation.Name] = convert_function(documentation)
			end
		end
	end
end

------------------------------------------------------------------------------
-- Output
------------------------------------------------------------------------------

local function sorted_keys(t)
	local keys = {}
	for key in pairs(t) do keys[#keys + 1] = key end
	table.sort(keys)
	return keys
end

local function json_string(s)
	return '"' .. s:gsub('[%c"\\]', function(c)
		return ({ ['"'] = '\\"', ["\\"] = "\\\\", ["\n"] = "\\n", ["\t"] = "\\t" })[c] or ("\\u%04x"):format(c:byte())
	end) .. '"'
end

local function json_array(items)
	local parts = {}
	for i, item in ipairs(items) do parts[i] = json_string(item) end
	return "[" .. table.concat(parts, ", ") .. "]"
end


-- One function per line, indented under its scope
local function function_lines(scope, indent)
	local lines = {}
	for _, name in ipairs(sorted_keys(scope)) do
		local f = scope[name]
		local entry = ("{\"parameters\": %s, \"returns\": %s"):format(json_array(f.parameters), json_array(f.returns))
		if f.rest then entry = entry .. ", \"rest\": " .. json_string(f.rest) end
		lines[#lines + 1] = ("%s%s: %s}"):format(indent, json_string(name), entry)
	end
	return table.concat(lines, ",\n")
end

local type_entries = {}
for _, name in ipairs(sorted_keys(named_types)) do
	type_entries[#type_entries + 1] = ("    %s: {\"base\": %s}"):format(json_string(name), json_string(named_types[name]))
end
local type_lines = table.concat(type_entries, ",\n")

-- Method tables, one method per line
local method_table_entries = {}
for _, name in ipairs(sorted_keys(method_tables)) do
	local functions = next(method_tables[name]) and ("\n" .. function_lines(method_tables[name], "        ") .. "\n      ") or ""
	method_table_entries[#method_table_entries + 1] = ('    %s: {"functions": {%s}}'):format(json_string(name), functions)
end

-- A table of tables of values, one inner table per line:
-- "Enum": {"tables": {"X": {"values": {"A": "Enum.X", ...}}, ...}}
local function value_tables(entries, member_type)
	local lines = {}
	for _, name in ipairs(sorted_keys(entries)) do
		local values = {}
		for _, member in ipairs(entries[name].Fields or entries[name].Values or {}) do
			values[#values + 1] = ("%s: %s"):format(json_string(member.Name), json_string(member_type(name, member)))
		end
		lines[#lines + 1] = ('        %s: {"values": {%s}}'):format(json_string(name), table.concat(values, ", "))
	end
	return '{\n      "tables": {\n' .. table.concat(lines, ",\n") .. "\n      }\n    }"
end

for _, global in ipairs({ "Enum", "Constants" }) do
	if namespaces[global] or functions[global] then error(global .. " is also documented as functions") end
end

local table_blocks = {}
table_blocks[#table_blocks + 1] = '    "Constants": ' .. value_tables(constants, function(_, value) return convert(value.Type) end)
table_blocks[#table_blocks + 1] = '    "Enum": ' .. value_tables(enumerations, function(name) return "Enum." .. name end)
for _, namespace in ipairs(sorted_keys(namespaces)) do
	table_blocks[#table_blocks + 1] = ('    %s: {\n      "functions": {\n%s\n      }\n    }')
		:format(json_string(namespace), function_lines(namespaces[namespace], "        "))
end

local file = assert(io.open(output_path, "w"))
file:write("{\n")
file:write('  "generated": "by tools/generate_api_definitions.lua from Blizzard_APIDocumentationGenerated; regenerate rather than editing",\n')
file:write('  "types": {\n', type_lines, "\n  },\n")
file:write('  "method_tables": {\n', table.concat(method_table_entries, ",\n"), "\n  },\n")
file:write('  "functions": {\n', function_lines(functions, "    "), "\n  },\n")
file:write('  "tables": {\n', table.concat(table_blocks, ",\n"), "\n  }\n")
file:write("}\n")
file:close()

local count = #sorted_keys(functions)
for _, scope in pairs(namespaces) do count = count + #sorted_keys(scope) end
io.stderr:write(("wrote %d functions, %d method tables, %d enumerations, %d constants tables and %d named types to %s (skipped %d secure-only, %d duplicates)\n")
	:format(count, #sorted_keys(method_tables), #sorted_keys(enumerations), #sorted_keys(constants), #sorted_keys(named_types), output_path, skipped.secure, skipped.duplicate))
local names = sorted_keys(unmapped)
if #names > 0 then
	table.sort(names, function(a, b) return unmapped[a] > unmapped[b] end)
	local parts = {}
	for i, name in ipairs(names) do parts[i] = ("%s (%d)"):format(name, unmapped[name]) end
	io.stderr:write("unmapped type names, treated as unknown: " .. table.concat(parts, ", ") .. "\n")
end
