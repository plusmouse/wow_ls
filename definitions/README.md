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
- `mixins` holds Lua mixins, each with its `functions` like a table's. A type
  can list `"mixins": ["ItemButtonMixin"]` to have their methods, after those
  of its `method_tables` and before inherited ones.
- `types` declares named types: tables or userdata shown by their own name
  (`Frame`) rather than `table`, or numbers with a name, for enums (`"base":
  "number"`). Any file can use a type declared in any other.

wow_ls looks for this directory in order: the `definitionsPath` initialization
option from the editor, the `WOW_LS_DEFINITIONS` environment variable, then a
`definitions` directory beside the executable or up to three levels above it.

`blizzard_api.json` is generated from Blizzard's API documentation; don't edit
it by hand. Regenerate it with:

```sh
lua5.1 tools/generate_api_definitions.lua definitions/blizzard_api.json Blizzard_APIDocumentationGenerated/*.lua
```

where `Blizzard_APIDocumentationGenerated` is
`Interface/AddOns/Blizzard_APIDocumentationGenerated` from wow-ui-source. How
documentation type names map to wow_ls types is set by the tables at the top of
the script, and it reports any names they don't cover.

`blizzard_templates.json` is generated from wow-ui-source's XML and Lua; don't
edit it by hand either. Regenerate it with:

```sh
wow_ls generate-templates ../wow-ui-source definitions/blizzard_templates.json
```

Generate `global_strings.json` first, as `KeyValue`s name global strings.

It reads `UI.xsd` for the type each XML element makes, every XML file a
Blizzard addon's `.toc` loads (for the current game version), and wow_ls's own
analysis of Blizzard's Lua for the methods of the mixins that XML uses. It gives
the intrinsic elements (`ItemButton`, `DropdownButton`, ...) a type each, and
records every template and named frame for addons' frames to use. It also
writes `elements`, the type each XML element makes (with `"extended": true`
when it may have methods beyond it), which is how an addon's own XML is typed
without `UI.xsd`. Its `functions`, `tables` and `values` are every other global
Blizzard's Lua assigns that the files here don't already define, with each
table's members, as the analysis types them. It reports mixins it found no
table for, and globals named by `KeyValue`s it couldn't type.

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
replaces the generated one.
