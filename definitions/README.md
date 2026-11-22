# Definitions

Preset types for globals that a file uses but never assigns, loaded by wow_ls at
runtime the first time it needs types. Every `.json` file here is loaded in
filename order; a later file's entry for a name replaces an earlier one.

```json
{
  "functions": {
    "tostring": { "parameters": ["value"], "returns": ["string"] },
    "string.format": { "parameters": ["format", "..."], "returns": ["string"] },
    "select": { "parameters": ["index", "..."], "returns": ["unknown"], "rest": "unknown" }
  }
}
```

- A dotted name is a field of a global table, like `string.format`.
- `"..."` as the last parameter means the function takes varargs.
- `returns` lists a type per return value. `rest` is the type of every value
  after those, for functions that return a variable number (defaults to `nil`).
- A type is names joined with `|`: `nil`, `false`, `true`, `boolean`, `number`,
  `string`, `function`, `table`, `userdata`, `thread`, `unknown`.

wow_ls looks for this directory in order: the `definitionsPath` initialization
option from the editor, the `WOW_LS_DEFINITIONS` environment variable, then a
`definitions` directory beside the executable or up to three levels above it.

`lua.json` is the Lua 5.1 standard library plus WoW's global aliases
(`strsplit`, `tinsert`, ...). `wow.json` is a small set of common WoW API
functions, meant to be replaced by generated API data.
