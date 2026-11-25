# Definitions

Preset types for globals that a file uses but never assigns, loaded by wow_ls at
runtime the first time it needs types. Every `.json` file here is loaded in
filename order; a later file's entry for a name replaces an earlier one.

```json
{
  "types": {
    "Frame": { "base": "table" }
  },
  "functions": {
    "tostring": { "parameters": ["value"], "returns": ["string"] },
    "select": { "parameters": ["index: number|string", "..."], "returns": ["unknown"], "rest": "unknown" }
  },
  "tables": {
    "string": {
      "functions": {
        "format": { "parameters": ["format: string", "..."], "returns": ["string"] }
      }
    }
  }
}
```

- `functions` are global functions. `tables` are global tables like `string` or
  `C_AddOns`, each with its own `functions` (and `tables` for nested tables).
  Members are fields of their table, so `local s = string; s.format()` works
  too. Names can't contain dots.
- A table can be spread over several files; each member's last definition wins.
- `"..."` as the last parameter means the function takes varargs.
- `only` lists the game clients a function is only in (`["Classic"]`), when
  the mainline client doesn't have it. Its signature ends `(Classic only)`.
- `returns` lists a type per return value. `rest` is the type of every value
  after those, for functions that return a variable number (defaults to `nil`).
- A parameter is `"name: type"`, or just `"name"` when it takes anything. One
  with a single type is a requirement: a function passing its own parameter
  straight to it gets that type for the parameter.
- A type is names joined with `|`: `nil`, `false`, `true`, `boolean`, `number`,
  `string`, `function`, `table`, `userdata`, `thread`, `unknown`, or a named type.
- `values` (at the top level or in a table) are members that aren't functions,
  each with a type: `"values": { "Immediate": "Enum.StatusBarInterpolation" }`.
- A table with `"open": true` may have functions beyond those listed (under
  computed keys), so what mixes it in isn't checked for missing methods.
- `mixins` holds Lua mixins, each with its `functions` like a table's, and
  `values`: fields it sets that may hold a function but aren't sure to (of a
  type that isn't known), with their types. A type
  can list `"mixins": ["ItemButtonMixin"]` to have their methods, after those
  of its `method_tables` and before inherited ones.
- `types` declares named types: tables or userdata shown by their own name
  (`Frame`) rather than `table`, or numbers with a name, for enums (`"base":
  "number"`). Any file can use a type declared in any other.

`sites/blizzard.json`, also written by `generate-templates`, is where in
wow-ui-source each Blizzard global, member, template and frame is, for go to
definition: `{"files": {path: {name: line}}}`, with members named by their
path (`MenuUtil.CreateContextMenu`) and XML templates as `template:Name`
(`template:Name.Key` for a `parentKey` child). It's read the first time it's
needed. `sites/wow-ui-source-path`, which isn't in the repository, holds where
the checkout it was generated from is.

wow_ls looks for this directory in order: the `definitionsPath` initialization
option from the editor, the `WOW_LS_DEFINITIONS` environment variable, then a
`definitions` directory beside the executable or up to three levels above it.

`blizzard_api.json` is generated from Blizzard's API documentation; don't edit
it by hand. Regenerate it with:

```sh
lua5.1 tools/generate_api_definitions.lua definitions/blizzard_api.json Blizzard_APIDocumentationGenerated/*.lua \
  --flavour Classic classic_era/Blizzard_APIDocumentationGenerated/*.lua classic/Blizzard_APIDocumentationGenerated/*.lua \
  --flavour Forever forever/Blizzard_APIDocumentationGenerated/*.lua
```

where `Blizzard_APIDocumentationGenerated` is
`Interface/AddOns/Blizzard_APIDocumentationGenerated` from wow-ui-source's
mainline branch (`ptr`), and the others are the same directory from its
`classic_era`, `classic` and `forever` branches, extracted with, for example,
`mkdir classic_era && git -C wow-ui-source archive classic_era Interface/AddOns/Blizzard_APIDocumentationGenerated | tar -x -C classic_era --strip-components=2`.
What the mainline documents is kept as it is; each flavour adds the functions
and methods it doesn't, marked `only`, and the structures and enumerations
they use. How
documentation type names map to wow_ls types is set by the tables at the top of
the script, and it reports any names they don't cover.

`blizzard_templates.json` is generated from wow-ui-source's XML and Lua; don't
edit it by hand either. Regenerate it with:

```sh
wow_ls generate-templates ../wow-ui-source definitions/blizzard_templates.json GlobalColor.<version>.csv
```

Generate `global_strings.json` first, as `KeyValue`s name global strings.

The last argument is the client's GlobalColor table exported as CSV
(`ID,LuaConstantName,Color`, as wago.tools gives it), for the global colors
the client makes (`NORMAL_FONT_COLOR`, ...). Each is analysed as if
`Blizzard_SharedXML` made it, straight after `ColorUtil.lua`, with
`NAME = CreateColorFromHexString("aarrggbb")`, the hex digits from `Color` (a
signed 32-bit ARGB number), so it's a color as that makes one, never nil, and
`NAME_CODE = "|caarrggbb"`, its markup, as the client makes. Without it
they're left out, and unknown.

It reads `UI.xsd` for the type each XML element makes, every XML file a
Blizzard addon's `.toc` loads (for the current game version), and wow_ls's own
analysis of Blizzard's Lua for the methods of the mixins that XML uses. It gives
the intrinsic elements (`ItemButton`, `DropdownButton`, ...) a type each, and
records every template and named frame for addons' frames to use. It also
writes `elements`, the type each XML element makes (with `"extended": true`
when it may have methods beyond it), which is how an addon's own XML is typed
without `UI.xsd`. Its `functions`, `tables` and `values` are every other global
Blizzard's Lua assigns that the files here don't already define, with each
table's members, as the analysis types them. The analysis knows Blizzard's
templates and frames, as an addon's does its own, so it walks the script
handlers Blizzard's XML writes inline. Its types name a template by the
template and a named frame by the frame (`"NamePlateDriverFrame"`), which keep
their mixins when loaded, and a `parentKey` child's by what it inherits when
that's all it is, else `unknown`. It reports mixins it found no table for, and
globals named by `KeyValue`s it couldn't type.

`global_strings.json` is the client's global strings (`ACCEPT`, `CANCEL`, ...),
each a `string`, generated from the GlobalStrings table exported as CSV
(`ID,BaseTag,TagText_lang,Flags`, as wago.tools gives it):

```sh
wow_ls generate-global-strings GlobalStrings.<version>.csv definitions/global_strings.json
```

Tags that aren't Lua names (`8.0_WARFRONTS_...`) are left out, as only
`_G[...]` reaches them.

`lua.json` is the Lua 5.1 standard library plus WoW's global aliases
(`strsplit`, `tinsert`, ...).

`wow.json` is for WoW API functions the documentation doesn't cover, like the
global `CreateFrame`. It loads after `blizzard_api.json`, so an entry here also
replaces the generated one. That fixes what the documentation has wrong: a frame's
attributes hold any value, though it gives `GetAttribute` and `SetAttribute`
a `cstring`.

It also holds `returned_templates`: for functions of the Lua analysed (Blizzard's,
while `generate-templates` runs, or an addon's), by path (`Mixin.Method` for
`function Mixin:Method()`), the template each kind of object they return is
made with, where the analysis can't tell, as when the templates are joined
into a string at run time:

```json
"returned_templates": {
  "AuraContainerCustomFrameProviderMixin.AcquireFrame": {"AuraButton": "CustomAuraButtonTemplate"}
}
```

Each object of that kind the function returns is of the template's type, and
may have members beyond it, as it may be made with more templates. Other
objects it returns are left as they are. Templates are named, not typed, as
they may be ones XML defines later.
