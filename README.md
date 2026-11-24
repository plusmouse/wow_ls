# Motivation
I am frustrated by the existing tooling

I want a language server that is fully type-free (with optional types in comments), and still gives out decent results.

Plus it should understand the connection between the WoW toc, XML and lua and be able to mix stuff into frames properly

# What it does

wow_ls is a language server for World of Warcraft addon Lua. It infers types
from the code itself, with no annotations needed.

- **Hover**: inferred types of variables, fields and functions, e.g.
  `function(itemLink, sameConnectedRealm, sameFaction) -> table?`
- **Completion**: locals in scope, globals once something is typed, keywords,
  and members after `.` or `:`. Members come from the definitions, from the
  fields the files assign, and through mixins and `__index`
- **Go to definition**, for variables and fields (methods included):
  - a local's or parameter's declaration, and for `self`, the method's name
  - every assignment to a global
  - every assignment to a field on the table it's reached through, or on
    the mixin or `__index` table it comes from. `t["x"]` and `{ ["x"] = v }`
    count as field `x`. A field of a `CopyTable` copy goes to the original's,
    and a method of a frame made from an XML template to its Lua mixin's
  - otherwise, where an XML file defines it: a named frame, or a template's
    `parentKey` child, in the addon's XML or Blizzard's
  - otherwise, where Blizzard's Lua defines it (`MenuUtil.CreateContextMenu`,
    a mixin's method), in wow-ui-source

  This also works across the addon's files and into its dependencies.
  Members of WoW's own API (`C_Container`, a frame's `SetPoint`) have no
  source to go to. Blizzard's code is looked for in the wow-ui-source checkout
  the `wowUiSourcePath` initialization option or `WOW_UI_SOURCE` environment
  variable names, else the one `generate-templates` was run on
- **Diagnostics**:
  - syntax errors and Lua's compile-time errors
  - calls to methods a WoW object doesn't have
    (`Frame has no method SetPiont`), including frames made from XML
    templates, Blizzard's or the addon's. Not reported when a template wow_ls
    doesn't know may have added the method, or a mixin it uses may have
    methods it can't see (functions stored under computed keys, or a Lua
    mixin with no table in the addon or its dependencies)
  - reads of fields a table the WoW API returns doesn't come with
    (``ContainerItemInfo has no field `stackCont` ``). Not reported when the
    addon or its dependencies give a field of that name to a table whose
    identity the analysis doesn't know, as the table is the addon's to change,
    or give a table of that type fields it can't name (under a computed key,
    by `rawset`, `Mixin` or `setmetatable`)
  - XML files that don't parse, at the error
  - calls to methods of a table made in the code when nothing in the addon or
    its dependencies defines a function by that name
    (``No function `ProcessQueue` is defined for this table``). Not reported
    when a WoW object has a method of that name, as the table may be a mixin
    for one, or when the table may get fields the analysis can't see: from
    an unknown mixin, metatable or `__index`, or functions stored under
    computed keys

## Usage

```sh
wow_ls                        # LSP server over stdio
wow_ls types <file.lua>       # every reference in a file, with its type
wow_ls types <Addon.toc>      # the same for a whole addon, with warnings and timing
wow_ls evaluate <file.lua>    # the parse tree and syntax errors
wow_ls generate-global-strings <GlobalStrings.csv> <out.json>   # see definitions/README.md
wow_ls generate-templates <wow-ui-source> <out.json>             # likewise
```

Preset types for WoW's API and the Lua standard library live in
`definitions/`; see [definitions/README.md](definitions/README.md) for the
format and how wow_ls finds them.

# How it works

## Addons
- An addon's files, from its `.toc`, are analysed together in load order, as
  one program. `...` at the top of each file is the addon's name and its own
  namespace table
- XML files are followed to the Lua and XML files they load, in place. Paths
  are taken for the current game version (`[Family]` is `Mainline`, `[Game]` is
  `Standard`, and `[AllowLoadGameType ...]` matches `mainline` and `standard`),
  ignoring case as the game does
- Dependencies (`## Dependencies`, `## RequiredDeps`, `## OptionalDeps`) are
  found in the workspace, or beside the addon's folder as `Interface/AddOns`
  would hold them
- Each dependency is analysed on its own first. Its addon then sees what the
  dependency makes visible but walks only its own files. So only an addon
  types its own API, whichever addon uses it

## Type inference
- A type is a set of basic kinds (nil, boolean, number, string, table,
  function, ...). It can also carry a named WoW type (`Frame`), the identity of
  a table made in the code, or a WoW object made in the code
- The analysis walks the code repeatedly, each walk reading what the one
  before settled, until nothing changes. Types start from nothing known and
  only grow. Function bodies whose inputs haven't changed are replayed rather
  than walked again. Loops that won't settle are widened after a few passes,
  so every walk ends
- A call is recursive when the function called can come back round to the
  caller through the functions each calls, directly or through others. Its
  results keep the first apart and join the rest together, so returns like
  `return x, f()` settle rather than gaining a value every walk
- What it models:
  - parameters typed by every call that passes them, when every call is seen
  - table fields and contents
  - metatables: `__index`, `__call`, `setmetatable`/`getmetatable`
  - `_G`, string methods, iterators and varargs
  - mixins, with a mixin's `self` typed by the frame it's mixed into
  - frame, texture and font string pools
  - every global Blizzard's Lua defines that the API documentation and
    hand-written definitions don't (`MenuUtil`, `CreateDataProvider`,
    `CallbackRegistryMixin`), from its own analysis of that Lua. One mixed in
    with `Mixin` or `CreateFromMixins` gives its functions, unless it holds
    functions under computed keys
  - XML templates, Blizzard's and the addon's: an object made from one
    (`CreateFrame`, pools, `CreateTexture`, ...) has its type, with its
    mixins' methods, its `parentKey` children and its `KeyValue`s, and those
    of what it inherits. Named XML frames (`GameTooltip`) are typed globals,
    and intrinsic elements (`ItemButton`) frame types of their own
  - an addon's Lua mixin named in XML (`mixin="MyMixin"`): `self` in its
    methods is the template or frame XML mixes it into, and a frame made from
    that template has the mixin's fields
  - script handlers written inline in XML (`<OnLoad>Mixin(self, M)</OnLoad>`),
    Blizzard's and the addon's, each as the function the game makes of it:
    `self` is the object it's on, and the other arguments are named as the
    game names them (`event`, `button`, `elapsed`, ...). A mixin mixed in
    there has `self` typed by that object too
  - an object from XML is shown by its native type or intrinsic (`Button`,
    `ItemButton`), not the template or mixins it's made with, though its
    members are all of theirs
  - a value that may be any of several WoW objects (a parameter given a
    frame made from a template and a plain one, `c and a or b`) keeps each
    apart, up to 8 (past that, it's the closest type
    they all are, which any may have more methods than). It has only the
    members all of them have: a method missing from any whose methods are
    known is reported (``This may be a Button, which has no method `Init` ``),
    and a field only some have isn't known. A field set or `SetParent`, or a
    `Mixin`, on it applies to each, and `GetParent` gives each one's parent
  - `CopyTable(t)` (and `CopyTableSafe`) makes a new table holding what `t`
    holds, with each table in it copied too, so writes to the copy don't
    reach `t`. `CopyTable(t, true)` shares the tables it holds
  - `CreateAnimation("Alpha")` makes the kind of animation it names, as
    `CreateFrame("Button")` does the kind of frame
  - the parents of WoW objects, and of `self` in a mixin: what its template
    is made in by `CreateFrame`, or where XML puts its objects (inside another,
    or `parent="..."`). As its frames may also be made where the analysis
    can't see, missing methods aren't reported on that parent

## In the editor
- Opening or editing a file shows diagnostics for every file of its addon,
  open or not, but not for the addons it depends on. A file closed without
  saving goes back to what's on disk
- Each addon's analysis is cached until a file it involves changes. An edit to
  a dependency also refreshes the open files of addons depending on it
- A burst of keystrokes is analysed once, for the last of them
- Templates and named frames are in one namespace for every addon, as in the
  game: the definitions hold those of every workspace addon's XML and its
  dependencies'. Editing an XML file makes them again and re-analyses every
  open file. A file that doesn't parse shows its error, and its types come
  from its last text that did. Which Lua and XML files an XML file loads is
  read from disk

On a release build, Syndicator (33 files) takes about 80ms and Baganator (122
files) about 400ms per edit, with Syndicator's analysis cached. Making the
definitions again after an XML edit adds about 30ms.

# Layout

| Path | What's there |
|---|---|
| `src/syntax/` | Lexer and parser, building a `rowan` tree, and compile-time checks |
| `src/ast.rs` | Typed wrappers over the syntax tree |
| `src/types.rs` | Type inference |
| `src/definitions.rs` | Loading the preset types |
| `src/project.rs` | Addons and their `.toc` files, and analysing an addon over its dependencies |
| `src/xml.rs` | XML files: what they load, the objects they define, and `UI.xsd` |
| `src/generate.rs` | Generating `definitions/blizzard_templates.json` from wow-ui-source |
| `src/completion.rs` | Completion |
| `src/lsp/` | The server loop, diagnostics, and positions |
| `definitions/` | Preset types; `blizzard_api.json` is generated by `tools/generate_api_definitions.lua`, `blizzard_templates.json` by `wow_ls generate-templates` |
| `tests/` | Lua fixtures, including a sample addon and addons depending on each other |

# Not done yet

- `GetParent()` gives the parent only of objects the analysis can follow to
  where they were made, or of `self` in a mixin (from `CreateFrame` with its
  template, or where XML puts its objects). Otherwise, as for a parent's
  parent, it's the documented `ScriptObject`, and its methods are reported
  missing (`ScriptObject has no method Show`)
- `self` in a mixin is the one type the objects it's mixed into all are, not
  each of them
- A `type="global"` `KeyValue` naming an addon's own global (a localised
  string) is `unknown`
- Making a frame from several templates at once, or from a template for
  another kind of object, gives the kind made, not checked for missing methods
- A `parentArray` is a plain `table`, not one of its objects
- Blizzard's Lua globals are typed by wow_ls's analysis of Blizzard's code,
  so a function whose returns come from undocumented C code
  (`MenuUtil.CreateContextMenu`) returns `unknown`, and a global its Lua only
  ever sets to one value (`X = true`) has that value's type
- A mixin passed to `Mixin` through a parameter (`Mixin(frame, mixin)` in a
  helper called with several mixins) isn't tied to the frame, so its `self`
  is a plain table rather than the frame
- A call to a dependency's function gets what it returns in general, not
  narrowed by that call's arguments
