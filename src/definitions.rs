//Copyright (C) 2025-  plusmouse and other contributors
//
//This program is free software: you can redistribute it and/or modify
//it under the terms of the GNU General Public License as published by
//the Free Software Foundation, either version 3 of the License, or
//(at your option) any later version.
//
//This program is distributed in the hope that it will be useful,
//but WITHOUT ANY WARRANTY; without even the implied warranty of
//MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//GNU General Public License for more details.
//
//You should have received a copy of the GNU General Public License
//along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Preset types for globals the file doesn't define, such as the Lua standard
//! library and the WoW API, loaded at runtime from `*.json` files:
//!
//! ```json
//! {
//!   "types": {
//!     "Frame": { "base": "table" },
//!     "LuaCurveObject": { "base": "userdata" }
//!   },
//!   "functions": {
//!     "tostring": { "parameters": ["value"], "returns": ["string"] },
//!     "select": { "parameters": ["index: number|string", "..."], "returns": ["unknown"], "rest": "unknown" }
//!   },
//!   "tables": {
//!     "string": {
//!       "functions": {
//!         "format": { "parameters": ["format: string", "..."], "returns": ["string"] }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! * `functions` are global functions. `tables` are global tables (namespaces like
//!   `string` or `C_AddOns`), each with its own `functions`, and `tables` for
//!   tables nested inside it. Names can't contain dots: `string.format` is the
//!   `format` member of the `string` table.
//! * A parameter is `"name: type"`, or just `"name"` when it takes anything. One
//!   with a single type (not `number|nil`) is required to be that type, so a
//!   function passing its own parameter straight through gets that type for it.
//! * `"..."` as the last parameter means the function takes varargs, and
//!   `"...: number"` gives them a type.
//! * `returns` has a type per return value. `rest` is the type of every value
//!   after those, for functions returning a variable number; it defaults to nil.
//! * A type is names joined with `|`: nil, false, true, boolean, number, string,
//!   function, table, userdata, thread, unknown, or a named type.
//! * `values` are members that aren't functions, each with a type, like
//!   `"values": { "Immediate": "Enum.StatusBarInterpolation" }`.
//! * Named types (`types`) are tables or userdata with their own name, like WoW's
//!   `Frame`, shown by that name instead of `table`, or numbers with a name (an
//!   enum). Any file can use a named type declared in any other, and several
//!   files can declare the same type (with the same `base`).
//! * A named type can list `values`, its fields and their types, like a data
//!   table a function returns:
//!   `"ContainerItemInfo": { "base": "table", "values": { "stackCount": "number" } }`.
//!   Those are all the fields it comes with, so reading another is reported,
//!   unless code gives it one: the table is the addon's to change.
//! * A named type's methods come from `method_tables` it lists (each a set of
//!   `functions`, declared in a `method_tables` section), and every method of
//!   the types it `inherits`:
//!   `"Button": { "base": "table", "inherits": ["Frame"], "method_tables": ["SimpleButtonAPI"] }`.
//!   A type that lists either has a known set of methods, so calls of other
//!   methods on it can be reported as missing.
//! * A type can also list `mixins`, declared in a `mixins` section of the same
//!   form as `method_tables`: Lua mixins, whose methods it has. They come
//!   after its method tables, so a method table's method wins. A type listing
//!   them has a known set of methods too. A mixin with `"open": true` may have
//!   methods beyond those listed, so a template using it doesn't.
//! * Method tables and mixins can also list `values`, fields that aren't
//!   methods, with their types, like a mixin's `self.count` of a type that
//!   isn't known.
//!
//! * `returned_templates` says, for functions of the files analysed (by path,
//!   `Mixin.Method` for `function Mixin:Method()`), the template each kind of
//!   object they return is made with, where the analysis can't tell, as when
//!   the templates are joined into a string at run time:
//!   `{"AuraContainerCustomFrameProviderMixin.AcquireFrame": {"AuraButton": "CustomAuraButtonTemplate"}}`.
//!   Each returned object of that kind is of the template's type, and may
//!   have members beyond it, as it may be made with more templates.
//!
//! Every `.json` file in the directory is loaded, in filename order, and a later
//! file's definition of a name replaces an earlier one. Tables are merged member
//! by member, so a later file can replace or add single members of `string`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::{HashMap, HashSet, FunctionInfo, FunctionRef, NamedRef, Parameter, Type, TypeSet, Values};

pub type BuiltinId = u32;
pub type NamedId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    /// For a namespace, its path (`string`, `C_AddOns`)
    pub name: String,
    /// TABLE, OTHER (userdata) or NUMBER (an enum)
    pub base: TypeSet,
    /// Known members, not counting inherited ones
    pub fields: HashMap<String, Type>,
    /// Types whose members this has too
    pub inherits: Vec<NamedId>,
    /// Whether its methods are known (it has `method_tables` or `inherits`), so
    /// anything else called on it is missing
    pub has_methods: bool,
    /// A namespace table like `string`, which only exists to hold its members.
    /// Shown as plain `table`, as it isn't a type code would name
    pub namespace: bool,
}

/// Templates' types while they're being made
#[derive(Default)]
struct Templates {
    /// Each template's type and description, by name
    nodes: HashMap<String, (NamedId, Value)>,
    /// Templates made so far, and whether every method each has is known
    complete: HashMap<NamedId, bool>,
    /// The type of element each template is for
    elements: HashMap<String, NamedId>,
    /// Types templates and frames inherit
    inherited: HashSet<NamedId>,
}

/// What a type is made of, by name, as its file lists them
#[derive(Debug, Default, Clone)]
struct Composition {
    inherits: Vec<String>,
    method_tables: Vec<String>,
    mixins: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Definitions {
    functions: Vec<FunctionInfo>,
    /// The type of each named frame the definitions' `frames` give, by the
    /// frame's name, which types elsewhere in them can name it by. Shown as
    /// the type of its element, as any frame is
    frame_types: HashMap<String, NamedId>,
    /// Global functions and namespace tables
    globals: HashMap<String, Type>,
    types: Vec<NamedType>,
    type_names: HashMap<String, NamedId>,
    /// Methods by table, from `method_tables` sections
    method_tables: HashMap<String, HashMap<String, Type>>,
    /// Methods of Lua mixins, from `mixins` sections. Kept apart from
    /// `method_tables`, as they aren't methods every object of a type has
    mixins: HashMap<String, HashMap<String, Type>>,
    /// Mixins with `"open": true`, which may have methods beyond those listed
    open_mixins: HashSet<String>,
    /// Tables with `"open": true`, which may have functions beyond those
    /// listed, under computed keys
    open_tables: HashSet<NamedId>,
    /// Types that list `values`: data tables whose fields are all known
    fixed_fields: HashSet<NamedId>,
    /// Templates' types, and the type of element each is for, by name
    templates: HashMap<String, (NamedId, NamedId)>,
    /// The type each XML element makes, and whether it may have methods
    /// beyond it, by tag, from `elements` sections
    elements: HashMap<String, (String, bool)>,
    /// Named fonts, which font strings inherit only settings from
    fonts: HashSet<String>,
    /// For a template's or frame's type, the mixins it has (its own and those
    /// of the templates it inherits) that aren't in `mixins`: Lua tables an
    /// addon defines, whose methods it has too, as far as wow_ls knows
    lua_mixins: HashMap<NamedId, Vec<String>>,
    /// The mixins each type lists or its XML gives it, by name, the last
    /// winning, for finding where a method it has from one is defined
    type_mixins: HashMap<NamedId, Vec<String>>,
    /// How sites name the members of types with no name of their own there:
    /// an XML object's children (`template:T.Key`) and named frames
    type_paths: HashMap<NamedId, String>,
    /// Where things in the addons' own XML are, by the names `sites` uses,
    /// with the file's full path
    xml_sites: HashMap<String, String>,
    /// Where things in Blizzard's code are, by name (`MenuUtil.CreateContextMenu`,
    /// `template:ButtonFrameTemplate.CloseButton`), as `path:line` within
    /// wow-ui-source. Read from `sites_file` the first time it's needed
    sites: std::sync::OnceLock<HashMap<String, String>>,
    sites_file: Option<PathBuf>,
    /// The wow-ui-source checkout `sites` are in
    pub source_root: Option<PathBuf>,
    /// Each object an XML file defines that has a mixin not in `mixins`: the
    /// mixin's name, and the object's type, which `self` is in its methods
    xml_mixins: Vec<(String, NamedId)>,
    /// The type of what each object with a Lua mixin is made in, when XML
    /// says: the mixin's name, and that type
    xml_parents: Vec<(String, NamedId)>,
    /// What each type listed for `inherits`, `method_tables` and `mixins`, resolved once
    /// every file is loaded
    compositions: HashMap<NamedId, Composition>,
    /// Types another type inherits from
    has_subtypes: HashSet<String>,
    /// For functions of the files analysed, by path
    /// (`AuraContainerCustomFrameProviderMixin.AcquireFrame`), the template
    /// each kind of object they return is taken to be made with, by the
    /// kind's name (`AuraButton`), from `returned_templates` sections
    returned_templates: HashMap<String, Vec<(String, String)>>,
}

impl Definitions {
    /// For a function of the files at `path`, the templates the objects it
    /// returns are taken to be made with, each by the kind of object it's
    /// for (see `returned_templates`)
    pub fn returned_templates(&self, path: &str) -> &[(String, String)] {
        self.returned_templates.get(path).map_or(&[], Vec::as_slice)
    }

    /// Reads `returned_templates`: by function path, an object of kind name
    /// to template name. They're names, not types, as the templates may be
    /// ones XML defines later (Blizzard's own, while they're generated)
    fn add_returned_templates(&mut self, root: &Value) -> Result<(), String> {
        let Some(functions) = object(root, "returned_templates")? else { return Ok(()) };
        for (path, kinds) in functions {
            let kinds = kinds.as_object().ok_or_else(|| format!("returned_templates: {path} should be an object of kinds to templates"))?;
            let mut templates = Vec::new();
            for (kind, template) in kinds {
                let template = template.as_str().ok_or_else(|| format!("returned_templates: {path}: {kind}'s template should be a name"))?;
                templates.push((kind.clone(), template.to_string()));
            }
            self.returned_templates.insert(path.clone(), templates);
        }
        Ok(())
    }

    /// The preset type of a global, if it has one
    pub fn global(&self, name: &str) -> Option<Type> {
        self.globals.get(name).copied()
    }

    /// The preset type at a path like `table.insert`
    pub fn at_path(&self, path: &str) -> Option<Type> {
        let mut parts = path.split('.');
        let mut ty = self.global(parts.next()?)?;
        for part in parts {
            ty = self.field(ty, part)?;
        }
        Some(ty)
    }

    /// The type of member `name` of a value of type `ty`, if it's known,
    /// including inherited members
    pub fn field(&self, ty: Type, name: &str) -> Option<Type> {
        self.member(ty.named.named_id()?, name, 0)
    }

    /// Whether type `id` lists `values`, so every field it comes with is known
    pub fn has_fixed_fields(&self, id: NamedId) -> bool {
        self.fixed_fields.contains(&id)
    }

    /// The `string` library's type. Every string has it as its metatable's
    /// `__index`, so `s:upper()` is `string.upper(s)`
    pub fn string_library(&self) -> Option<Type> {
        self.global("string").filter(|ty| ty.named.named_id().is_some())
    }

    /// The type whose members a value of type `ty` has: the `string` library
    /// for a string (that may be nil), otherwise `ty` itself
    pub fn member_source(&self, ty: Type) -> Type {
        let only_string = ty.set.contains(TypeSet::STRING) && ty.set.without(TypeSet::STRING | TypeSet::NIL) == TypeSet::NEVER;
        match only_string {
            true => self.string_library().unwrap_or(ty),
            false => ty,
        }
    }

    /// Every preset global, in no particular order
    pub fn globals(&self) -> impl Iterator<Item = (&str, Type)> {
        self.globals.iter().map(|(name, &ty)| (name.as_str(), ty))
    }

    /// Every known member of type `id`, including inherited ones. A member
    /// redefined lower down the hierarchy is given once, with its own type
    pub fn members(&self, id: NamedId) -> HashMap<&str, Type> {
        let mut members = HashMap::default();
        self.collect_members(id, &mut members, 0);
        members
    }

    fn collect_members<'d>(&'d self, id: NamedId, members: &mut HashMap<&'d str, Type>, depth: usize) {
        // Deep enough for any real hierarchy, and stops a loop in a bad file
        if depth > 32 {
            return
        }
        let Some(named) = self.named_type(id) else { return };
        for (name, &ty) in &named.fields {
            members.entry(name.as_str()).or_insert(ty);
        }
        for &parent in &named.inherits {
            self.collect_members(parent, members, depth + 1);
        }
    }

    /// Whether another type inherits from `id`
    pub fn has_subtypes(&self, id: NamedId) -> bool {
        self.named_type(id).is_some_and(|named| self.has_subtypes.contains(&named.name))
    }

    /// Whether `id` is `ancestor` or inherits from it
    pub fn is_a(&self, id: NamedId, ancestor: NamedId) -> bool {
        self.is_a_within(id, ancestor, 0)
    }

    fn is_a_within(&self, id: NamedId, ancestor: NamedId, depth: usize) -> bool {
        id == ancestor || (depth <= 32 && self.named_type(id).is_some_and(|named| {
            named.inherits.iter().any(|&parent| self.is_a_within(parent, ancestor, depth + 1))
        }))
    }

    /// The closest type both `a` and `b` are, if they have one in common
    pub fn common_ancestor(&self, a: NamedId, b: NamedId) -> Option<NamedId> {
        // Breadth first through `a`'s ancestors, so the closest is found first
        let mut queue = std::collections::VecDeque::from([a]);
        let mut seen = HashSet::default();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue
            }
            if self.is_a(b, id) {
                return Some(id)
            }
            queue.extend(self.named_type(id).iter().flat_map(|named| named.inherits.iter().copied()));
        }
        None
    }

    /// Named types, by name ignoring case
    pub fn named_type_id_ignoring_case(&self, name: &str) -> Option<NamedId> {
        self.named_type_id(name).or_else(|| {
            self.types.iter().position(|t| !t.namespace && t.name.eq_ignore_ascii_case(name)).map(|i| i as NamedId)
        })
    }

    fn member(&self, id: NamedId, name: &str, depth: usize) -> Option<Type> {
        // Deep enough for any real hierarchy, and stops a loop in a bad file
        if depth > 32 {
            return None
        }
        let named = self.named_type(id)?;
        named.fields.get(name).copied()
            .or_else(|| named.inherits.iter().find_map(|&parent| self.member(parent, name, depth + 1)))
    }

    /// The function at a path like `string.format`
    pub fn function_at(&self, path: &str) -> Option<&FunctionInfo> {
        let mut parts = path.split('.');
        let mut ty = self.global(parts.next()?)?;
        for part in parts {
            ty = self.field(ty, part)?;
        }
        match ty.function {
            FunctionRef::Builtin(id) => self.function(id),
            _ => None,
        }
    }

    pub fn function(&self, id: BuiltinId) -> Option<&FunctionInfo> {
        self.functions.get(id as usize)
    }

    pub fn len(&self) -> usize {
        self.functions.len()
    }

    pub fn named_type(&self, id: NamedId) -> Option<&NamedType> {
        self.types.get(id as usize)
    }

    /// Whether some WoW object type has a method called `name`, in one of
    /// its `method_tables`
    pub fn is_object_method(&self, name: &str) -> bool {
        self.method_tables.values().any(|methods| methods.contains_key(name))
    }

    pub fn named_type_id(&self, name: &str) -> Option<NamedId> {
        self.type_names.get(name).copied()
    }

    /// Every `.json` file in `directory`, with where Blizzard's code defines
    /// things from its `sites/blizzard.json`, in the wow-ui-source checkout
    /// `WOW_UI_SOURCE` names, else the one it was generated from
    pub fn load_directory(directory: &Path) -> Result<Definitions, String> {
        let mut definitions = Definitions::load_directory_except(directory, None)?;
        let sites = directory.join("sites/blizzard.json");
        definitions.sites_file = sites.is_file().then_some(sites);
        definitions.source_root = std::env::var_os("WOW_UI_SOURCE").map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::fs::read_to_string(directory.join(SOURCE_PATH_FILE)).ok()?.trim())));
        Ok(definitions)
    }

    /// Loads every `.json` file in `directory` but the one called `except`
    pub fn load_directory_except(directory: &Path, except: Option<&std::ffi::OsStr>) -> Result<Definitions, String> {
        let entries = std::fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter(|p| except.is_none() || p.file_name() != except)
            .collect();
        paths.sort();
        let mut files = Vec::new();
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let root: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            files.push((path.display().to_string(), root));
        }
        Definitions::from_values(&files)
    }

    pub fn from_json(text: &str) -> Result<Definitions, String> {
        let root: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Definitions::from_values(&[("definitions".to_string(), root)])
    }

    /// Named types first, from every file, so functions in any file can use them
    fn from_values(files: &[(String, Value)]) -> Result<Definitions, String> {
        let mut definitions = Definitions::default();
        for (file, root) in files {
            definitions.add_types(root).map_err(|e| format!("{file}: {e}"))?;
        }
        definitions.has_subtypes = definitions.compositions.values()
            .flat_map(|composition| composition.inherits.iter().cloned())
            .collect();
        // Templates and frames can be named as types before they're built
        definitions.reserve_objects(files);
        for (file, root) in files {
            definitions.add_method_tables(root).map_err(|e| format!("{file}: {e}"))?;
            definitions.add_type_values(root).map_err(|e| format!("{file}: {e}"))?;
            definitions.add_members(root, None, "").map_err(|e| format!("{file}: {e}"))?;
            definitions.add_returned_templates(root).map_err(|e| format!("{file}: {e}"))?;
        }
        definitions.compose()?;
        definitions.add_templates(files);
        definitions.add_mixin_types();
        Ok(definitions)
    }

    /// For each function returning a new table with only globals mixed in
    /// (see `FreshObject`), the table's type: one for each list of globals,
    /// with their members, the last listed winning as `Mixin` does. Named by
    /// the globals (`ColorMixin`, `A & B`), each table one of that type, so
    /// any number of them together keep what they have, as frames do
    fn add_mixin_types(&mut self) {
        let mut made: HashMap<Vec<String>, NamedId> = HashMap::default();
        for index in 0..self.functions.len() {
            for slot in 0..self.functions[index].fresh_returns.len() {
                let Some(fresh) = &self.functions[index].fresh_returns[slot] else { continue };
                if fresh.mixin_globals.is_empty() || !fresh.mixed_parameters.is_empty() {
                    continue
                }
                let Some(&returned) = self.functions[index].returns.values.get(slot) else { continue };
                // Only a plain table: not one already of a type
                if returned.named.named_id().is_some() || !returned.set.contains(TypeSet::TABLE) {
                    continue
                }
                let globals = fresh.mixin_globals.clone();
                let id = match made.get(&globals) {
                    Some(&id) => id,
                    None => {
                        let Some(id) = self.mixin_type(&globals) else { continue };
                        made.insert(globals, id);
                        id
                    }
                };
                let set = returned.set;
                self.functions[index].returns.values[slot] = Type { named: NamedRef::One(id), ..Type::of(set) };
            }
        }
    }

    /// A type with the members of the global tables `globals`, the last
    /// winning. Its methods are known unless one of them may have more. None
    /// if one isn't a table the definitions give
    fn mixin_type(&mut self, globals: &[String]) -> Option<NamedId> {
        let mut fields: HashMap<String, Type> = HashMap::default();
        let mut open = false;
        for global in globals {
            let table = self.global(global)?.named.named_id()?;
            let named = &self.types[table as usize];
            if !named.namespace {
                return None
            }
            open |= self.open_tables.contains(&table);
            fields.extend(named.fields.iter().map(|(name, &ty)| (name.clone(), ty)));
        }
        let id = self.types.len() as NamedId;
        self.types.push(NamedType {
            name: globals.join(" & "), base: TypeSet::TABLE, fields, inherits: Vec::new(), has_methods: !open, namespace: false,
        });
        Some(id)
    }

    /// A type for each template of `templates` (of its own name) and named
    /// frame of `frames`, to be filled in by `add_templates`, so the types
    /// of functions and values can name them
    fn reserve_objects(&mut self, files: &[(String, Value)]) {
        for (_, root) in files {
            for (name, _) in root.get("templates").and_then(Value::as_object).into_iter().flatten() {
                if !self.type_names.contains_key(name) {
                    self.type_names.insert(name.clone(), self.types.len() as NamedId);
                    self.types.push(NamedType {
                        name: name.clone(), base: TypeSet::TABLE, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: false,
                    });
                }
            }
            for (name, node) in root.get("frames").and_then(Value::as_object).into_iter().flatten() {
                if self.frame_types.contains_key(name) {
                    continue
                }
                let element = node.get("type").and_then(Value::as_str).unwrap_or("Frame").to_string();
                self.frame_types.insert(name.clone(), self.types.len() as NamedId);
                self.types.push(NamedType {
                    name: element, base: TypeSet::TABLE, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: false,
                });
            }
        }
    }

    /// Reads `elements`, then gives each template (`templates`) a type of its
    /// own name, and each named frame (`frames`) a global of an unnamed type,
    /// once every other type is known. A later file's template or frame
    /// replaces an earlier one's
    fn add_templates(&mut self, files: &[(String, Value)]) {
        let mut templates: Vec<(String, Value)> = Vec::new();
        let mut frames: Vec<(String, Value)> = Vec::new();
        for (_, root) in files {
            for (tag, kind) in root.get("elements").and_then(Value::as_object).into_iter().flatten() {
                let kind = match kind {
                    Value::String(ty) => Some((ty.clone(), false)),
                    Value::Object(kind) => kind.get("type").and_then(Value::as_str)
                        .map(|ty| (ty.to_string(), kind.get("extended").and_then(Value::as_bool).unwrap_or(false))),
                    _ => None,
                };
                if let Some(kind) = kind {
                    self.elements.insert(tag.clone(), kind);
                }
            }
            for (key, found) in [("templates", &mut templates), ("frames", &mut frames)] {
                let Some(Value::Object(entries)) = root.get(key) else { continue };
                for (name, node) in entries {
                    found.retain(|(n, _)| n != name);
                    found.push((name.clone(), node.clone()));
                }
            }
        }
        self.add_objects(&templates, &frames);
    }

    /// These definitions with the templates and named frames of addons' XML
    /// files (`objects`, the top ones of each file, in the order they load),
    /// all in one namespace as they are in the game. A template replaces one
    /// of the same name loaded before it; a frame doesn't replace a global
    /// defined otherwise
    /// `sites` are where in their files the objects and their parts are (see
    /// `xml::sites`)
    pub fn with_xml(&self, objects: &[crate::xml::Object], sites: Vec<(String, String)>) -> Definitions {
        let mut definitions = self.clone();
        definitions.xml_sites.extend(sites);
        // An intrinsic is an element of its own, making its own type
        for object in objects.iter().filter(|o| o.intrinsic) {
            if let Some(name) = &object.name {
                definitions.elements.insert(name.clone(), (name.clone(), false));
            }
        }
        let (mut templates, mut frames) = (Vec::new(), Vec::new());
        {
            let kind = |tag: &str| definitions.elements.get(tag).cloned();
            let mut global = |path: &str| definitions.at_path(path).map(|ty| definitions.written_type_text(ty));
            for object in objects {
                if object.is_virtual || object.intrinsic {
                    if let Some(name) = &object.name {
                        templates.retain(|(n, _): &(String, Value)| n != name);
                        templates.push((name.clone(), crate::xml::node(object, &kind, &mut global)));
                    }
                    continue
                }
                let mut found = Vec::new();
                crate::xml::global_frames(object, None, &mut found);
                for (name, object) in found {
                    frames.retain(|(n, _): &(String, Value)| *n != name);
                    frames.push((name, crate::xml::node(&object, &kind, &mut global)));
                }
            }
        }
        definitions.add_objects(&templates, &frames);
        definitions
    }

    /// These definitions with what each element makes, by tag: its type, and
    /// whether it may have methods beyond it, as `elements` gives them
    pub fn with_elements(&self, elements: impl IntoIterator<Item = (String, (String, bool))>) -> Definitions {
        let mut definitions = self.clone();
        definitions.elements.extend(elements);
        definitions
    }

    /// The type of an object the XML defines, by how sites name it
    /// (`template:T.Key`, a frame's name, `Frame.Key`), or failing that the
    /// type its element `tag` makes
    pub fn xml_object(&self, object: Option<&str>, tag: &str) -> Option<Type> {
        let by_name = object.and_then(|object| {
            let mut parts = object.split('.');
            let first = parts.next()?;
            let mut ty = match first.strip_prefix("template:") {
                Some(template) => Type::named(self.template(template)?.0, TypeSet::TABLE),
                None => self.global(first)?,
            };
            for key in parts {
                ty = *self.types.get(ty.named.named_id()? as usize)?.fields.get(key)?;
            }
            Some(ty)
        });
        by_name.or_else(|| {
            let (element, _) = self.element(tag)?;
            Some(Type::named(self.named_type_id(element)?, TypeSet::TABLE))
        })
    }

    /// How sites name type `id`'s members: a namespace table's path, a
    /// template's `template:` name, or the path given an XML object or frame
    fn type_path(&self, id: NamedId) -> Option<String> {
        let named = self.types.get(id as usize)?;
        if named.namespace {
            return Some(named.name.clone())
        }
        if self.templates.get(&named.name).is_some_and(|&(template, _)| template == id) {
            return Some(format!("template:{}", named.name))
        }
        self.type_paths.get(&id).cloned()
    }

    /// Where a thing sites name `key` is: a file (in wow-ui-source when
    /// relative) and line
    fn site(&self, key: &str) -> Option<(PathBuf, u32)> {
        let text = match self.xml_sites.get(key) {
            Some(text) => text,
            None => self.sites.get_or_init(|| read_sites(self.sites_file.as_deref())).get(key)?,
        };
        let (path, line) = text.rsplit_once(':')?;
        let path = PathBuf::from(path);
        let path = match path.is_absolute() {
            true => path,
            false => self.source_root.as_ref()?.join(path),
        };
        Some((path, line.parse().ok()?))
    }

    /// Where global `name` is defined, if Blizzard's code or an addon's XML
    /// defines it
    pub fn global_site(&self, name: &str) -> Option<(PathBuf, u32)> {
        self.site(name)
    }

    /// Where member `name` of a value of type `id` is defined, if Blizzard's
    /// code or an addon's XML defines it: on the type, one of its mixins
    /// (the last first), or what it inherits
    pub fn member_site(&self, id: NamedId, name: &str) -> Option<(PathBuf, u32)> {
        self.member_site_within(id, name, 0)
    }

    fn member_site_within(&self, id: NamedId, name: &str, depth: usize) -> Option<(PathBuf, u32)> {
        if depth > 32 {
            return None
        }
        if let Some(site) = self.type_path(id).and_then(|path| self.site(&format!("{path}.{name}"))) {
            return Some(site)
        }
        for mixin in self.type_mixins.get(&id).into_iter().flatten().rev() {
            if self.mixins.get(mixin).is_some_and(|methods| methods.contains_key(name)) {
                return self.site(&format!("{mixin}.{name}"))
            }
        }
        let named = self.types.get(id as usize)?;
        named.inherits.iter().find_map(|&parent| self.member_site_within(parent, name, depth + 1))
    }

    /// The type XML element `tag` makes, and whether it may have methods beyond it
    pub fn element(&self, tag: &str) -> Option<(&str, bool)> {
        self.elements.get(tag).map(|(ty, extended)| (ty.as_str(), *extended))
    }

    /// Whether table `ty` is one the definitions give that may hold functions
    /// beyond those listed (`"open": true`), or isn't one they give at all
    pub fn may_have_more(&self, ty: Type) -> bool {
        match ty.named.named_id() {
            Some(id) => !self.types[id as usize].namespace || self.open_tables.contains(&id),
            None => true,
        }
    }

    /// The mixins a template's or frame's type has that are Lua tables wow_ls
    /// doesn't know, which its methods may come from
    pub fn lua_mixins(&self, id: NamedId) -> &[String] {
        self.lua_mixins.get(&id).map_or(&[], Vec::as_slice)
    }

    /// For each Lua mixin of an object XML makes inside another (or names a
    /// `parent` for), the mixin's name and the type of that parent
    pub fn xml_parents(&self) -> &[(String, NamedId)] {
        &self.xml_parents
    }

    /// Each mixin XML gives an object that isn't in `mixins`, with the object's type
    pub fn xml_mixins(&self) -> &[(String, NamedId)] {
        &self.xml_mixins
    }

    /// Templates and named frames, by name: a type for each template, built
    /// over those already known, and a global for each frame
    fn add_objects(&mut self, templates: &[(String, Value)], frames: &[(String, Value)]) {
        // Each template's type first, so they can inherit one another in any order
        let mut building = Templates::default();
        for (name, node) in templates {
            let id = match self.type_names.get(name) {
                Some(&id) => id,
                None => {
                    let id = self.types.len() as NamedId;
                    self.type_names.insert(name.clone(), id);
                    self.types.push(NamedType {
                        name: name.clone(), base: TypeSet::TABLE, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: false,
                    });
                    id
                }
            };
            building.nodes.insert(name.clone(), (id, node.clone()));
        }
        self.fonts.extend(frames.iter()
            .filter(|(_, node)| node.get("type").and_then(Value::as_str) == Some("Font"))
            .map(|(name, _)| name.clone()));
        for (name, _) in templates {
            self.build_template(name, &mut building);
        }
        for (name, node) in frames {
            if self.globals.contains_key(name) {
                continue
            }
            // Into the type reserved for it, when types elsewhere name it
            let into = self.frame_types.get(name).copied();
            if let Some(id) = into {
                self.type_paths.insert(id, name.clone());
            }
            if let Some((id, _)) = self.object_node_at(node, into, None, Some(name), &mut building) {
                self.globals.insert(name.clone(), Type::named(id, TypeSet::TABLE));
            }
        }
        for (name, &(id, _)) in &building.nodes {
            match building.elements.get(name) {
                Some(&element) => { self.templates.insert(name.clone(), (id, element)); }
                None => { self.templates.remove(name); }
            }
        }
        for &id in &building.inherited {
            self.has_subtypes.insert(self.types[id as usize].name.clone());
        }
    }

    /// The type of element template `name` is for
    fn template_element(&self, name: &str, building: &Templates) -> Option<NamedId> {
        building.elements.get(name).copied().or_else(|| Some(self.templates.get(name)?.1))
    }

    /// Template `name`'s type, filled in if it isn't yet, and whether every
    /// method it has is known
    fn build_template(&mut self, name: &str, building: &mut Templates) -> Option<(NamedId, bool)> {
        let Some((id, node)) = building.nodes.get(name).cloned() else {
            // One built before, as Blizzard's are for an addon's
            let &(id, _) = self.templates.get(name)?;
            return Some((id, self.types[id as usize].has_methods))
        };
        if let Some(&complete) = building.complete.get(&id) {
            return Some((id, complete))
        }
        // One that inherits itself, through others, isn't known
        building.complete.insert(id, false);
        let (_, complete) = self.object_node_at(&node, Some(id), None, Some(&format!("template:{name}")), building)?;
        building.complete.insert(id, complete);
        if let Some(element) = node.get("type").and_then(Value::as_str).and_then(|t| self.named_type_id(t)) {
            building.elements.insert(name.to_string(), element);
        }
        Some((id, complete))
    }

    /// The type of an object a template or frame describes: `into`, the
    /// template's own type, or else a new unnamed one, shown as the type of
    /// its element. An object with nothing of its own is just of that type.
    /// Also whether every method it has is known: it isn't when it's a kind
    /// of object that isn't a known type, or something it inherits or mixes in
    /// isn't known
    fn object_node(&mut self, node: &Value, into: Option<NamedId>, parent: Option<NamedId>, building: &mut Templates) -> Option<(NamedId, bool)> {
        self.object_node_at(node, into, parent, None, building)
    }

    /// `object_node` for an object sites name `path` (`template:T.Key`, or a
    /// frame's name), for finding where its members are
    fn object_node_at(&mut self, node: &Value, into: Option<NamedId>, parent: Option<NamedId>, path: Option<&str>, building: &mut Templates) -> Option<(NamedId, bool)> {
        let strings = |key: &str| -> Vec<String> {
            node.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
        };
        let element_name = node.get("type").and_then(Value::as_str);
        // An intrinsic element, built first so whether its methods are known is
        if let Some(intrinsic) = element_name.filter(|name| building.nodes.contains_key(*name)) {
            self.build_template(intrinsic, building);
        }
        let element =element_name.and_then(|t| self.named_type_id(t)).filter(|&id| self.types[id as usize].has_methods);
        let extended = node.get("extended").and_then(Value::as_bool).unwrap_or(false);
        let (inherits, mixins) = (strings("inherits"), strings("mixins"));
        let own = ["values", "children", "arrays"].iter().any(|key| node.get(key).is_some());
        if into.is_none() && inherits.is_empty() && mixins.is_empty() && !own {
            return element.map(|id| (id, !extended))
        }
        let mut complete = element.is_some() && !extended;
        let is_font = element_name == Some("Font");
        // The last inherited wins, then the element's own methods
        let mut parents = Vec::new();
        for inherited in inherits.iter().rev() {
            // A font string inheriting a font takes its settings, not methods
            if !is_font && self.fonts.contains(inherited) {
                continue
            }
            match self.build_template(inherited, building) {
                Some((_, _)) if !is_font && self.template_element(inherited, building).is_some_and(|e| self.types[e as usize].name == "Font") => {}
                Some((parent, parent_complete)) => {
                    parents.push(parent);
                    complete &= parent_complete;
                }
                None => complete = false,
            }
        }
        parents.extend(element);
        building.inherited.extend(parents.iter().copied());
        let id = match into {
            Some(id) => id,
            None => {
                let id = self.types.len() as NamedId;
                let name = element.map_or_else(|| "Frame".to_string(), |e| self.types[e as usize].name.clone());
                self.types.push(NamedType {
                    name, base: TypeSet::TABLE, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: false,
                });
                id
            }
        };
        let mut fields: HashMap<String, Type> = HashMap::default();
        for (key, value) in node.get("values").and_then(Value::as_object).into_iter().flatten() {
            let ty = value.as_str().and_then(|t| self.parse_type(t).ok()).unwrap_or(Type::UNKNOWN);
            fields.insert(key.clone(), ty);
        }
        for (key, child) in node.get("children").and_then(Value::as_object).into_iter().flatten() {
            let child_path = path.map(|path| format!("{path}.{key}"));
            let ty = match self.object_node_at(child, None, Some(id), child_path.as_deref(), building) {
                Some((id, _)) => Type::named(id, TypeSet::TABLE),
                None => Type::UNKNOWN,
            };
            fields.insert(key.clone(), ty);
        }
        for key in node.get("arrays").and_then(Value::as_object).into_iter().flatten().map(|(key, _)| key) {
            fields.insert(key.clone(), Type::of(TypeSet::TABLE));
        }
        // Lua mixins it has through what it inherits
        let mut lua: Vec<String> = Vec::new();
        for parent in &parents {
            for mixin in self.lua_mixins.get(parent).into_iter().flatten() {
                if !lua.contains(mixin) {
                    lua.push(mixin.clone());
                }
            }
        }
        // The last mixed in wins
        let mut own_lua = Vec::new();
        for mixin in mixins.iter().rev() {
            match self.mixins.get(mixin) {
                Some(methods) => for (method, &ty) in methods {
                    fields.entry(method.clone()).or_insert(ty);
                },
                // One wow_ls doesn't know, like an addon's own: whether its
                // methods are known is up to the analysis
                None => {
                    own_lua.push(mixin.clone());
                    if !lua.contains(mixin) {
                        lua.push(mixin.clone());
                    }
                }
            }
            complete &= !self.open_mixins.contains(mixin);
        }
        if into.is_none() {
            if let Some(path) = path {
                self.type_paths.insert(id, path.to_string());
            }
        }
        if mixins.is_empty() {
            self.type_mixins.remove(&id);
        } else {
            self.type_mixins.insert(id, mixins.clone());
        }
        let named = &mut self.types[id as usize];
        named.fields = fields;
        named.inherits = parents;
        named.has_methods = complete;
        if lua.is_empty() {
            self.lua_mixins.remove(&id);
        } else {
            self.lua_mixins.insert(id, lua.clone());
        }
        self.xml_mixins.extend(own_lua.into_iter().map(|mixin| (mixin, id)));
        // Where it's made, for `self:GetParent()` in its Lua mixins: inside
        // another object, or the frame `parent` names
        let parent = parent.or_else(|| {
            let name = node.get("parent").and_then(Value::as_str)?;
            self.globals.get(name)?.named.named_id()
        });
        if let Some(parent) = parent {
            self.xml_parents.extend(lua.iter().map(|mixin| (mixin.clone(), parent)));
        }
        Some((id, complete))
    }

    /// The type a template gives what it's made into, and the type of element
    /// it is for (`Button` for a button template)
    pub fn template(&self, name: &str) -> Option<(NamedId, NamedId)> {
        self.templates.get(name).copied()
    }

    /// A type as the definitions write it: `number|nil`, `Frame`, `Enum.ItemQuality`
    pub fn type_text(&self, ty: Type) -> String {
        self.type_text_naming(ty, false)
    }

    /// A type as definitions are written: like `type_text`, but naming a
    /// template's type by the template and a named frame's by the frame, so
    /// what it's made with (its mixins, templates and children) is kept, as
    /// definitions' types can name both
    pub fn written_type_text(&self, ty: Type) -> String {
        self.type_text_naming(ty, true)
    }

    /// How type `id` is shown: by its name, but a template's type by the
    /// element it's for (`Button`), as an object from XML is shown by its
    /// native type, not the template or mixins it's made with. An intrinsic
    /// is an element of its own, so it's shown by its name (`ItemButton`)
    pub fn display_name(&self, id: NamedId) -> Option<&str> {
        let mut id = id;
        // Deep enough for any real template, and stops a loop
        for _ in 0..16 {
            let named = self.named_type(id)?;
            match self.templates.get(&named.name) {
                Some(&(template, element)) if template == id && !self.elements.contains_key(&named.name) => id = element,
                _ => return Some(&named.name),
            }
        }
        self.named_type(id).map(|named| named.name.as_str())
    }

    /// The named frame whose type `id` is, if it's one's own and its name
    /// isn't a type's too
    fn frame_name(&self, id: NamedId) -> Option<&str> {
        let path = self.type_paths.get(&id)?;
        let is_frame = !path.contains([':', '.']) && self.globals.get(path).and_then(|ty| ty.named.named_id()) == Some(id);
        (is_frame && self.named_type_id(path).is_none()).then_some(path.as_str())
    }

    /// How type `id` is written in definitions, so that it loads with the
    /// same methods: by its name, a template's or named frame's by theirs.
    /// An object's type of its own that has no name (a `parentKey` child's)
    /// is written as what it inherits when that's all it is, the methods
    /// of mixins it has itself aside. Otherwise it can't be written
    fn written_name(&self, id: NamedId, depth: usize) -> Option<String> {
        let named = self.named_type(id)?;
        if let Some(frame) = self.frame_name(id) {
            return Some(frame.to_string())
        }
        if self.type_names.get(&named.name) == Some(&id) || depth > 16 {
            return (depth <= 16).then(|| named.name.clone())
        }
        // Each it inherits that isn't already one of the others: its element,
        // when its template is for that element, is just the template
        let parents: Vec<NamedId> = named.inherits.iter().copied()
            .filter(|&parent| !named.inherits.iter().any(|&other| other != parent && self.is_a(other, parent)))
            .collect();
        match parents.as_slice() {
            [parent] if !self.type_mixins.contains_key(&id) => self.written_name(*parent, depth + 1),
            _ => None,
        }
    }

    fn type_text_naming(&self, ty: Type, written: bool) -> String {
        let definitions = self;
        // An object whose type can't be written makes the whole unknown,
        // rather than one with fewer methods
        let unwritable = std::cell::Cell::new(false);
        let named = |id: Option<u32>| id.and_then(|id| {
            definitions.named_type(id).filter(|n| !n.namespace)?;
            match written {
                true => {
                    let name = definitions.written_name(id, 0);
                    unwritable.set(unwritable.get() || name.is_none());
                    name
                }
                false => definitions.display_name(id).map(str::to_string),
            }
        });
        let mut parts: Vec<String> = Vec::new();
        for name in ty.set.names() {
            let part = match name {
                _ if name.starts_with("unknown") => return "unknown".to_string(),
                "never" => continue,
                // Any of several objects: each's type
                "table" | "userdata" if matches!(ty.named, NamedRef::Several(_)) => {
                    let mut names: Vec<String> = Vec::new();
                    for object in ty.named.alternatives() {
                        let one = named(object.named_id()).unwrap_or_else(|| name.to_string());
                        if !names.contains(&one) {
                            names.push(one);
                        }
                    }
                    names.join("|")
                }
                "table" | "userdata" => named(ty.named.named_id()).unwrap_or_else(|| name.to_string()),
                "number" => named(ty.numeric.named_id()).unwrap_or_else(|| name.to_string()),
                _ => name.to_string(),
            };
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
        if unwritable.get() {
            return "unknown".to_string()
        }
        if parts.is_empty() {
            return "nil".to_string()
        }
        parts.join("|")
    }

    fn add_types(&mut self, root: &Value) -> Result<(), String> {
        let types = match root.get("types") {
            Some(Value::Object(types)) => types,
            Some(_) => return Err("`types` should be an object".to_string()),
            None => return Ok(()),
        };
        for (name, definition) in types {
            let base = match definition.get("base").and_then(Value::as_str) {
                Some("table") => TypeSet::TABLE,
                Some("userdata") => TypeSet::OTHER,
                Some("number") => TypeSet::NUMBER,
                _ => return Err(format!("{name}: `base` should be \"table\", \"userdata\" or \"number\"")),
            };
            if parse_builtin_type(name).is_some() {
                return Err(format!("{name}: can't redefine a built-in type"));
            }
            let id = match self.type_names.get(name) {
                Some(&id) if self.types[id as usize].base == base => id,
                Some(_) => return Err(format!("{name} is declared with two different bases")),
                None => {
                    let id = self.types.len() as NamedId;
                    self.type_names.insert(name.clone(), id);
                    self.types.push(NamedType {
                        name: name.clone(), base, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: false,
                    });
                    id
                }
            };
            let names = |key: &str| -> Result<Vec<String>, String> {
                match definition.get(key) {
                    None => Ok(Vec::new()),
                    Some(Value::Array(items)) => items.iter()
                        .map(|i| i.as_str().map(str::to_string).ok_or_else(|| format!("{name}: `{key}` should only hold strings")))
                        .collect(),
                    Some(_) => Err(format!("{name}: `{key}` should be an array")),
                }
            };
            let (inherits, method_tables, mixins) = (names("inherits")?, names("method_tables")?, names("mixins")?);
            if ["inherits", "method_tables", "mixins"].iter().any(|key| definition.get(key).is_some()) {
                let composition = self.compositions.entry(id).or_default();
                composition.inherits.extend(inherits);
                composition.method_tables.extend(method_tables);
                composition.mixins.extend(mixins);
            }
        }
        Ok(())
    }

    /// The fields named types list as `values`, once every type is declared,
    /// so a field can be of a type declared after it
    fn add_type_values(&mut self, root: &Value) -> Result<(), String> {
        let Some(types) = object(root, "types")? else { return Ok(()) };
        for (name, definition) in types {
            let Some(values) = object(definition, "values")? else { continue };
            let id = self.type_names[name];
            self.fixed_fields.insert(id);
            for (field, ty) in values {
                let ty = ty.as_str().ok_or_else(|| format!("{name}.{field}: a value's type should be a string"))?;
                let ty = self.parse_type(ty).map_err(|e| format!("{name}.{field}: {e}"))?;
                self.types[id as usize].fields.insert(field.clone(), ty);
            }
        }
        Ok(())
    }

    /// The `method_tables` and `mixins` sections, which have the same form
    fn add_method_tables(&mut self, root: &Value) -> Result<(), String> {
        for key in ["method_tables", "mixins"] {
            let Some(tables) = object(root, key)? else { continue };
            for (table, definition) in tables {
                // Registered even if empty, so types can list it
                let mut methods = HashMap::default();
                if let Some(functions) = object(definition, "functions")? {
                    for (name, function) in functions {
                        let function = self.parse_function(function).map_err(|e| format!("{table}:{name}: {e}"))?;
                        let id = self.functions.len() as BuiltinId;
                        self.functions.push(function);
                        methods.insert(name.clone(), Type::builtin(id));
                    }
                }
                // Fields that aren't methods, with their types
                if let Some(values) = object(definition, "values")? {
                    for (name, ty) in values {
                        let ty = ty.as_str().ok_or_else(|| format!("{table}.{name}: a value's type should be a string"))?;
                        let ty = self.parse_type(ty).map_err(|e| format!("{table}.{name}: {e}"))?;
                        methods.insert(name.clone(), ty);
                    }
                }
                if key == "mixins" && definition.get("open").and_then(Value::as_bool) == Some(true) {
                    self.open_mixins.insert(table.clone());
                }
                let section = if key == "mixins" { &mut self.mixins } else { &mut self.method_tables };
                section.entry(table.clone()).or_default().extend(methods);
            }
        }
        Ok(())
    }

    /// Gives each type the methods of the tables it lists, and links the types
    /// it inherits from
    fn compose(&mut self) -> Result<(), String> {
        let mut compositions: Vec<_> = std::mem::take(&mut self.compositions).into_iter().collect();
        compositions.sort_by_key(|(id, _)| *id);
        for (id, Composition { inherits, method_tables, mixins }) in compositions {
            let name = self.types[id as usize].name.clone();
            let mut parents = Vec::new();
            for parent in &inherits {
                let parent = self.named_type_id(parent).ok_or_else(|| format!("{name} inherits unknown type {parent}"))?;
                parents.push(parent);
            }
            let mut fields = HashMap::default();
            let tables = method_tables.iter().map(|table| self.method_tables.get(table).ok_or_else(|| format!("{name}: no method table {table}")))
                .chain(mixins.iter().map(|mixin| self.mixins.get(mixin).ok_or_else(|| format!("{name}: no mixin {mixin}"))));
            for methods in tables {
                let methods = methods?;
                for (method, ty) in methods {
                    // The first table listed wins
                    fields.entry(method.clone()).or_insert(*ty);
                }
            }
            let open = mixins.iter().any(|mixin| self.open_mixins.contains(mixin));
            if !mixins.is_empty() {
                // The first listed wins here, so it's looked in last
                self.type_mixins.insert(id, mixins.iter().rev().cloned().collect());
            }
            let named = &mut self.types[id as usize];
            named.inherits = parents;
            named.has_methods = !open;
            for (method, ty) in fields {
                named.fields.entry(method).or_insert(ty);
            }
        }
        Ok(())
    }

    /// Adds the `functions` and `tables` of `scope`: the file's root (globals) when
    /// `namespace` is None, otherwise the table `path`
    fn add_members(&mut self, scope: &Value, namespace: Option<NamedId>, path: &str) -> Result<(), String> {
        let qualified = |name: &str| if path.is_empty() { name.to_string() } else { format!("{path}.{name}") };
        let check_name = |name: &str| if name.contains('.') {
            Err(format!("{name}: names can't contain dots; put table members under `tables`"))
        } else {
            Ok(())
        };
        if let Some(functions) = object(scope, "functions")? {
            for (name, definition) in functions {
                check_name(name)?;
                let function = self.parse_function(definition).map_err(|e| format!("{}: {e}", qualified(name)))?;
                let id = self.functions.len() as BuiltinId;
                self.functions.push(function);
                self.set_member(namespace, name, Type::builtin(id));
            }
        }
        if let Some(values) = object(scope, "values")? {
            for (name, ty) in values {
                check_name(name)?;
                let ty = ty.as_str().ok_or_else(|| format!("{}: a value's type should be a string", qualified(name)))?;
                let ty = self.parse_type(ty).map_err(|e| format!("{}: {e}", qualified(name)))?;
                self.set_member(namespace, name, ty);
            }
        }
        if let Some(tables) = object(scope, "tables")? {
            for (name, definition) in tables {
                check_name(name)?;
                let path = qualified(name);
                let existing = match namespace {
                    None => self.globals.get(name),
                    Some(id) => self.types[id as usize].fields.get(name),
                };
                let table = match existing.map(|ty| ty.named) {
                    Some(NamedRef::One(id)) if self.types[id as usize].namespace => id,
                    Some(_) => return Err(format!("{path} is both a function and a table")),
                    None => {
                        let id = self.types.len() as NamedId;
                        self.types.push(NamedType {
                            name: path.clone(), base: TypeSet::TABLE, fields: HashMap::default(), inherits: Vec::new(), has_methods: false, namespace: true,
                        });
                        self.set_member(namespace, name, Type::named(id, TypeSet::TABLE));
                        id
                    }
                };
                if definition.get("open").and_then(Value::as_bool) == Some(true) {
                    self.open_tables.insert(table);
                }
                self.add_members(definition, Some(table), &path)?;
            }
        }
        Ok(())
    }

    fn set_member(&mut self, namespace: Option<NamedId>, name: &str, ty: Type) {
        match namespace {
            None => self.globals.insert(name.to_string(), ty),
            Some(id) => self.types[id as usize].fields.insert(name.to_string(), ty),
        };
    }
}

impl Definitions {
    fn parse_function(&self, definition: &Value) -> Result<FunctionInfo, String> {
        let strings = |key: &str| -> Result<Vec<String>, String> {
            match definition.get(key) {
                None => Ok(Vec::new()),
                Some(Value::Array(items)) => items.iter()
                    .map(|item| item.as_str().map(str::to_string).ok_or_else(|| format!("`{key}` should only hold strings")))
                    .collect(),
                Some(_) => Err(format!("`{key}` should be an array")),
            }
        };
        let mut parameters = strings("parameters")?.iter().map(|p| self.parse_parameter(p)).collect::<Result<Vec<_>, _>>()?;
        let varargs = parameters.pop_if(|p| p.name == "...");
        if parameters.iter().any(|p| p.name == "...") {
            return Err("`...` can only be the last parameter".to_string());
        }
        let values = strings("returns")?.iter().map(|t| self.parse_type(t)).collect::<Result<Vec<_>, _>>()?;
        let rest = match definition.get("rest") {
            None => Type::NIL,
            Some(Value::String(t)) => self.parse_type(t)?,
            Some(_) => return Err("`rest` should be a string".to_string()),
        };
        // For each return, `null`, or that each call returns a new table with
        // the arguments at `parameters` (by position) and the globals named
        // in `mixins` mixed in, and the fields in `values`, which it's given
        // after it's made
        let fresh_returns = match definition.get("fresh_returns") {
            None => Vec::new(),
            Some(Value::Array(items)) => items.iter()
                .map(|item| -> Result<_, String> {
                    if item.is_null() {
                        return Ok(None)
                    }
                    let list = |key: &str| match item.get(key) {
                        None => Ok(Vec::new()),
                        Some(Value::Array(list)) => Ok(list.clone()),
                        Some(_) => Err(format!("`fresh_returns` `{key}` should be an array")),
                    };
                    let mixed_parameters = list("parameters")?.iter()
                        .map(|p| p.as_u64().map(|p| p as usize).ok_or_else(|| "`fresh_returns` parameters should be numbers".to_string()))
                        .collect::<Result<Vec<_>, _>>()?;
                    let mixin_globals = list("mixins")?.iter()
                        .map(|m| m.as_str().map(str::to_string).ok_or_else(|| "`fresh_returns` mixins should be names".to_string()))
                        .collect::<Result<Vec<_>, _>>()?;
                    // Fields it's given after it's made, with their types
                    let fields = match item.get("values") {
                        None => Vec::new(),
                        Some(Value::Object(values)) => values.iter()
                            .map(|(name, ty)| {
                                let ty = ty.as_str().ok_or_else(|| format!("`fresh_returns` values: {name}'s type should be a string"))?;
                                Ok((name.clone(), self.parse_type(ty)?))
                            })
                            .collect::<Result<Vec<_>, String>>()?,
                        Some(_) => return Err("`fresh_returns` values should be an object".to_string()),
                    };
                    Ok(Some(crate::types::FreshObject { made: None, mixed_parameters, pool_mixed_parameters: None, mixin_globals, kind_parameter: None, fields }))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("`fresh_returns` should be an array".to_string()),
        };
        // Methods it calls on what it's passed: `on` the arguments at those
        // positions (the last holding it), passing it the arguments at the
        // positions given, `...` for those beyond its parameters, or types
        let forwarded_calls = match definition.get("method_calls") {
            None => Vec::new(),
            Some(Value::Array(items)) => items.iter().map(|item| -> Result<_, String> {
                // `"self"`, or the positions of parameters
                let on = item.get("on").and_then(Value::as_array).ok_or("`method_calls` needs `on`")?.iter()
                    .map(|on| match on {
                        Value::String(s) if s == "self" => Ok(crate::types::On::SelfValue),
                        Value::Number(n) => n.as_u64().map(|p| crate::types::On::Parameter(p as usize)).ok_or_else(|| "`on` should hold whole numbers".to_string()),
                        _ => Err("`on` should hold \"self\" or positions".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let method = item.get("method").and_then(Value::as_str).ok_or("`method_calls` needs `method`")?.to_string();
                let arguments = match item.get("arguments") {
                    None => Vec::new(),
                    Some(Value::Array(arguments)) => arguments.iter().map(|argument| match argument {
                        Value::Number(n) => n.as_u64().map(|p| crate::types::Forwarded::Parameter(p as usize)).ok_or_else(|| "a position should be a whole number".to_string()),
                        Value::String(s) if s == "..." => Ok(crate::types::Forwarded::Rest),
                        Value::String(s) => self.parse_type(s).map(crate::types::Forwarded::Value),
                        _ => Err("an argument should be a position, `...` or a type".to_string()),
                    }).collect::<Result<Vec<_>, _>>()?,
                    Some(_) => return Err("`arguments` should be an array".to_string()),
                };
                Ok(crate::types::ForwardedCall { on, method, arguments })
            }).collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("`method_calls` should be an array".to_string()),
        };
        // What an argument stands for, in `method_calls` and `field_writes`: a
        // position, `{"truthy": position}`, `...` or a type
        let forwarded = |argument: &Value| -> Result<crate::types::Forwarded, String> {
            match argument {
                Value::Number(n) => n.as_u64().map(|p| crate::types::Forwarded::Parameter(p as usize)).ok_or_else(|| "a position should be a whole number".to_string()),
                Value::String(s) if s == "..." => Ok(crate::types::Forwarded::Rest),
                Value::String(s) => self.parse_type(s).map(crate::types::Forwarded::Value),
                Value::Object(o) => o.get("truthy").and_then(Value::as_u64).map(|p| crate::types::Forwarded::Truthy(p as usize)).ok_or_else(|| "an object argument should be `{\"truthy\": position}`".to_string()),
                _ => Err("an argument should be a position, `...`, a type or `{\"truthy\": position}`".to_string()),
            }
        };
        // What one of them is on: `"self"`, a position, or `{"returned": position}`
        let on = |on: Option<&Value>| -> Result<crate::types::On, String> {
            match on {
                Some(Value::String(s)) if s == "self" => Ok(crate::types::On::SelfValue),
                Some(Value::Number(n)) => n.as_u64().map(|p| crate::types::On::Parameter(p as usize)).ok_or_else(|| "`on` should be a whole number".to_string()),
                Some(Value::Object(o)) => o.get("returned").and_then(Value::as_u64).map(|p| crate::types::On::Returned(p as usize)).ok_or_else(|| "`on` should be `{\"returned\": position}`".to_string()),
                _ => Err("`on` should be \"self\", a position or `{\"returned\": position}`".to_string()),
            }
        };
        // Fields it gives what it's passed
        let field_writes = match definition.get("field_writes") {
            None => Vec::new(),
            Some(Value::Array(items)) => items.iter().map(|item| -> Result<_, String> {
                let field = item.get("field").and_then(Value::as_str).ok_or("`field_writes` needs `field`")?.to_string();
                let value = item.get("value").and_then(Value::as_array).ok_or("`field_writes` needs `value`")?.iter()
                    .map(&forwarded).collect::<Result<Vec<_>, _>>()?;
                Ok(crate::types::FieldWrite { on: on(item.get("on"))?, field, value })
            }).collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("`field_writes` should be an array".to_string()),
        };
        // For each return, `null` or the field of `self` or a parameter it is
        let returned_fields = match definition.get("returned_fields") {
            None => Vec::new(),
            Some(Value::Array(items)) => items.iter().map(|item| -> Result<_, String> {
                if item.is_null() {
                    return Ok(None)
                }
                let field = item.get("field").and_then(Value::as_str).ok_or("`returned_fields` needs `field`")?.to_string();
                Ok(Some(crate::types::ReturnedField { on: on(item.get("on"))?, field }))
            }).collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("`returned_fields` should be an array".to_string()),
        };
        Ok(FunctionInfo { parameters, method: false, varargs, returns: Values { values, rest }, fresh_returns, calls: Vec::new(), forwarded_calls, field_writes, returned_fields, iterated: None })
    }

    /// `"name"` or `"name: type"`
    fn parse_parameter(&self, text: &str) -> Result<Parameter, String> {
        let (name, types) = match text.split_once(':') {
            Some((name, types)) => (name.trim(), self.parse_type(types)?),
            None => (text.trim(), Type::UNKNOWN),
        };
        if name.is_empty() {
            return Err(format!("parameter `{text}` has no name"));
        }
        Ok(Parameter::declared(name, types))
    }

    pub fn parse_type(&self, text: &str) -> Result<Type, String> {
        let mut types = Type::default();
        for name in text.split('|') {
            let name = name.trim();
            types |= match (parse_builtin_type(name), self.named_type_id(name)) {
                (Some(set), _) => Type::of(set),
                (None, Some(id)) => Type::named(id, self.types[id as usize].base),
                (None, None) if self.frame_types.contains_key(name) => Type::named(self.frame_types[name], TypeSet::TABLE),
                (None, None) => return Err(format!("unknown type `{name}` in `{text}`")),
            };
        }
        Ok(types)
    }
}

fn object<'v>(scope: &'v Value, key: &str) -> Result<Option<&'v serde_json::Map<String, Value>>, String> {
    match scope.get(key) {
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(format!("`{key}` should be an object")),
        None => Ok(None),
    }
}

fn parse_builtin_type(name: &str) -> Option<TypeSet> {
    Some(match name {
        "nil" => TypeSet::NIL,
        "false" => TypeSet::FALSE,
        "true" => TypeSet::TRUE,
        "boolean" => TypeSet::BOOLEAN,
        "number" => TypeSet::NUMBER,
        "string" => TypeSet::STRING,
        "function" => TypeSet::FUNCTION,
        "table" => TypeSet::TABLE,
        "userdata" | "thread" => TypeSet::OTHER,
        "unknown" => TypeSet::UNKNOWN,
        _ => return None,
    })
}

/// Where to load definitions from: an explicit path (from the editor's
/// settings), then `$WOW_LS_DEFINITIONS`, then a `definitions` directory beside
/// the executable or up to three directories above it (which finds the repo's
/// copy from `target/debug`)
/// The file in the definitions directory naming the wow-ui-source checkout
/// `sites` are in, as `generate-templates` was given it. Not in the
/// repository, as it's where it is on one machine
pub const SOURCE_PATH_FILE: &str = "sites/wow-ui-source-path";

/// What a sites file holds, `{"files": {path: {name: line}}}`, as `path:line`
/// by name
fn read_sites(file: Option<&Path>) -> HashMap<String, String> {
    let Some(file) = file else { return HashMap::default() };
    let parsed: Option<Value> = std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok());
    let files = parsed.as_ref().and_then(|root| root.get("files")).and_then(Value::as_object);
    let mut sites = HashMap::default();
    for (path, names) in files.into_iter().flatten() {
        for (name, line) in names.as_object().into_iter().flatten() {
            if let Some(line) = line.as_u64() {
                sites.insert(name.clone(), format!("{path}:{line}"));
            }
        }
    }
    sites
}

pub fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("WOW_LS_DEFINITIONS") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    exe.parent()?.ancestors().take(4)
        .map(|dir| dir.join("definitions"))
        .find(|dir| dir.is_dir())
}

/// Finds and loads the definitions, reporting problems on stderr (stdout is the
/// LSP channel) and falling back to none
pub fn load(explicit: Option<&Path>) -> Definitions {
    let Some(directory) = locate(explicit) else {
        eprintln!("wow_ls: no definitions directory found; standard library types are unknown");
        return Definitions::default();
    };
    match Definitions::load_directory(&directory) {
        Ok(definitions) => definitions,
        Err(e) => {
            eprintln!("wow_ls: couldn't load definitions: {e}");
            Definitions::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_functions() {
        let definitions = Definitions::from_json(r#"{"functions": {
            "f": {"parameters": ["a", "b: string", "c : number|nil", "...: number"], "returns": ["number|nil", "string"], "rest": "unknown"},
            "g": {}
        }}"#).unwrap();
        let f = definitions.function_at("f").unwrap();
        let names: Vec<&str> = f.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(f.parameters[0].types, Type::UNKNOWN);
        assert_eq!(f.parameters[0].mandated, None);
        assert_eq!(f.parameters[1].mandated, Some(Type::of(TypeSet::STRING)));
        // Optional, so callers needn't pass a number
        assert_eq!(f.parameters[2].types, Type::of(TypeSet::NUMBER | TypeSet::NIL));
        assert_eq!(f.parameters[2].mandated, None);
        assert_eq!(f.varargs.as_ref().unwrap().mandated, Some(Type::of(TypeSet::NUMBER)));
        assert_eq!(f.returns.values, [Type::of(TypeSet::NUMBER | TypeSet::NIL), Type::of(TypeSet::STRING)]);
        assert_eq!(f.returns.rest, Type::UNKNOWN);
        let g = definitions.function_at("g").unwrap();
        assert_eq!(g.returns, Values::none());
    }

    #[test]
    fn later_definitions_replace_earlier() {
        let first: Value = serde_json::from_str(r#"{"functions": {"f": {"returns": ["number"]}}}"#).unwrap();
        let second: Value = serde_json::from_str(r#"{"functions": {"f": {"returns": ["string"]}}}"#).unwrap();
        let definitions = Definitions::from_values(&[("a".to_string(), first), ("b".to_string(), second)]).unwrap();
        let f = definitions.function_at("f").unwrap();
        assert_eq!(f.returns.values, [Type::of(TypeSet::STRING)]);
    }

    #[test]
    fn reports_errors() {
        let error = Definitions::from_json(r#"{"functions": {"f": {"returns": ["nubmer"]}}}"#).unwrap_err();
        assert!(error.contains("f:") && error.contains("nubmer"), "{error}");
        assert!(Definitions::from_json(r#"{"functions": {"f": {"parameters": ["...", "a"]}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"functions": {"f": {"parameters": [": number"]}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"functions": {"f": {"parameters": ["a: nubmer"]}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"functions": []}"#).is_err());
        assert!(Definitions::from_json("{").is_err());
    }

    #[test]
    fn named_types() {
        // Declared in one file, used in another loaded earlier
        let functions: Value = serde_json::from_str(r#"{"functions": {"CreateThing": {"returns": ["Frame|nil"]}}}"#).unwrap();
        let types: Value = serde_json::from_str(r#"{"types": {"Frame": {"base": "table"}, "Curve": {"base": "userdata"}}}"#).unwrap();
        let definitions = Definitions::from_values(&[("a".to_string(), functions), ("b".to_string(), types)]).unwrap();
        let frame = definitions.named_type_id("Frame").unwrap();
        assert_eq!(definitions.named_type(frame).unwrap().base, TypeSet::TABLE);
        let curve = definitions.named_type_id("Curve").unwrap();
        assert_eq!(definitions.named_type(curve).unwrap().base, TypeSet::OTHER);
        let f = definitions.function_at("CreateThing").unwrap();
        assert_eq!(f.returns.values, [Type::named(frame, TypeSet::TABLE) | Type::NIL]);

        assert!(Definitions::from_json(r#"{"types": {"Frame": {"base": "string"}}}"#).is_err());
        // Fields can name types declared after them
        let d = Definitions::from_json(r#"{"types": {"Info": {"base": "table", "values": {"n": "number", "frame": "Frame|nil"}}, "Frame": {"base": "table"}}}"#).unwrap();
        let info = Type::named(d.named_type_id("Info").unwrap(), TypeSet::TABLE);
        assert_eq!(d.field(info, "n"), Some(Type::of(TypeSet::NUMBER)));
        assert_eq!(d.type_text(d.field(info, "frame").unwrap()), "Frame|nil");
        assert!(Definitions::from_json(r#"{"types": {"Info": {"base": "table", "values": {"n": "nubmer"}}}}"#).is_err());
        // A mixin's values are fields of what lists it, beside its methods
        let d = Definitions::from_json(r#"{"types": {"F": {"base": "table", "mixins": ["M"]}},
            "mixins": {"M": {"functions": {"Go": {}}, "values": {"count": "unknown"}}}}"#).unwrap();
        let f = Type::named(d.named_type_id("F").unwrap(), TypeSet::TABLE);
        assert_eq!(d.field(f, "count"), Some(Type::UNKNOWN));
        assert!(d.field(f, "Go").is_some_and(|go| go.set == TypeSet::FUNCTION));
        assert!(Definitions::from_json(r#"{"mixins": {"M": {"values": {"n": 1}}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"types": {"E": {"base": "number"}}}"#).is_ok());
        assert!(Definitions::from_json(r#"{"types": {"table": {"base": "table"}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"functions": {"f": {"returns": ["Missing"]}}}"#).is_err());
    }

    #[test]
    fn tables() {
        let definitions = Definitions::from_json(r#"{
            "functions": {"print": {}},
            "tables": {
                "string": {"functions": {"format": {"returns": ["string"]}}},
                "a": {"tables": {"b": {"functions": {"c": {"returns": ["number"]}}}}}
            }
        }"#).unwrap();
        assert!(definitions.function_at("print").is_some());
        assert_eq!(definitions.function_at("string.format").unwrap().returns.values, [Type::of(TypeSet::STRING)]);
        assert_eq!(definitions.function_at("a.b.c").unwrap().returns.values, [Type::of(TypeSet::NUMBER)]);
        assert!(definitions.function_at("string.nope").is_none());
        assert!(definitions.function_at("string").is_none());
        // A table is a table, with its members as fields
        let string = definitions.global("string").unwrap();
        assert_eq!(string.set, TypeSet::TABLE);
        assert!(definitions.field(string, "format").is_some());
    }

    #[test]
    fn values() {
        let definitions = Definitions::from_json(r#"{
            "types": {"Enum.Quality": {"base": "number"}},
            "values": {"VERSION": "string"},
            "tables": {"Enum": {"tables": {"Quality": {"values": {"Poor": "Enum.Quality"}}}}}
        }"#).unwrap();
        assert_eq!(definitions.global("VERSION"), Some(Type::of(TypeSet::STRING)));
        let quality = definitions.named_type_id("Enum.Quality").unwrap();
        let table = definitions.field(definitions.global("Enum").unwrap(), "Quality").unwrap();
        assert_eq!(definitions.field(table, "Poor"), Some(Type::named(quality, TypeSet::NUMBER)));
        assert!(Definitions::from_json(r#"{"values": {"x": 1}}"#).is_err());
        assert!(Definitions::from_json(r#"{"values": {"x": "nubmer"}}"#).is_err());
    }

    #[test]
    fn tables_merge_across_files() {
        let first: Value = serde_json::from_str(r#"{"tables": {"string": {"functions": {
            "format": {"returns": ["string"]}, "trim": {"returns": ["number"]}
        }}}}"#).unwrap();
        let second: Value = serde_json::from_str(r#"{"tables": {"string": {"functions": {"trim": {"returns": ["string"]}}}}}"#).unwrap();
        let definitions = Definitions::from_values(&[("a".to_string(), first), ("b".to_string(), second)]).unwrap();
        assert!(definitions.function_at("string.format").is_some());
        assert_eq!(definitions.function_at("string.trim").unwrap().returns.values, [Type::of(TypeSet::STRING)]);
    }

    #[test]
    fn table_errors() {
        let error = Definitions::from_json(r#"{"functions": {"string.format": {}}}"#).unwrap_err();
        assert!(error.contains("tables"), "{error}");
        assert!(Definitions::from_json(r#"{"functions": {"x": {}}, "tables": {"x": {}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"tables": {"x": {"functions": []}}}"#).is_err());
    }

    #[test]
    fn object_methods() {
        let generated: Value = serde_json::from_str(r#"{
            "types": {"Frame": {"base": "table"}, "Texture": {"base": "table"}},
            "method_tables": {
                "RegionAPI": {"functions": {"SetPoint": {}, "Show": {"returns": ["number"]}}},
                "FrameAPI": {"functions": {"CreateTexture": {"returns": ["Texture"]}, "Show": {"returns": ["string"]}}},
                "ButtonAPI": {"functions": {"Click": {}}}
            }
        }"#).unwrap();
        let objects: Value = serde_json::from_str(r#"{"types": {
            "Region": {"base": "table", "method_tables": ["RegionAPI"]},
            "Frame": {"base": "table", "inherits": ["Region"], "method_tables": ["FrameAPI"]},
            "Button": {"base": "table", "inherits": ["Frame"], "method_tables": ["ButtonAPI"]}
        }}"#).unwrap();
        let definitions = Definitions::from_values(&[("a".to_string(), generated), ("b".to_string(), objects)]).unwrap();
        let button = Type::named(definitions.named_type_id("Button").unwrap(), TypeSet::TABLE);
        assert!(definitions.field(button, "Click").is_some());
        assert!(definitions.field(button, "SetPoint").is_some());
        assert!(definitions.field(button, "Missing").is_none());
        // The nearest definition wins
        let show = definitions.field(button, "Show").unwrap();
        let FunctionRef::Builtin(show) = show.function else { panic!() };
        assert_eq!(definitions.function(show).unwrap().returns.values, [Type::of(TypeSet::STRING)]);
        let create = definitions.field(button, "CreateTexture").unwrap();
        let FunctionRef::Builtin(create) = create.function else { panic!() };
        let texture = definitions.named_type_id("Texture").unwrap();
        assert_eq!(definitions.function(create).unwrap().returns.values, [Type::named(texture, TypeSet::TABLE)]);
        assert!(definitions.named_type(definitions.named_type_id("Button").unwrap()).unwrap().has_methods);
        assert!(!definitions.named_type(texture).unwrap().has_methods);
    }

    #[test]
    fn mixins() {
        let definitions = Definitions::from_json(r#"{
            "method_tables": {"ButtonAPI": {"functions": {"Click": {}}}},
            "mixins": {"ItemMixin": {"functions": {"SetItem": {"parameters": ["item"]}, "Click": {"returns": ["number"]}}}},
            "types": {
                "Button": {"base": "table", "method_tables": ["ButtonAPI"]},
                "ItemButton": {"base": "table", "inherits": ["Button"], "mixins": ["ItemMixin"]}
            }
        }"#).unwrap();
        let id = definitions.named_type_id("ItemButton").unwrap();
        assert!(definitions.named_type(id).unwrap().has_methods);
        let members = definitions.members(id);
        assert!(members.contains_key("SetItem"));
        // Its own mixin's method replaces the inherited one
        let click = definitions.field(Type::named(id, TypeSet::TABLE), "Click").unwrap();
        let FunctionRef::Builtin(click) = click.function else { panic!() };
        assert_eq!(definitions.function(click).unwrap().returns.first().set, TypeSet::NUMBER);
        // Mixin methods aren't methods every object of some type has
        assert!(!definitions.is_object_method("SetItem"));
        // One that may have more methods than it lists
        let open = Definitions::from_json(r#"{
            "mixins": {"M": {"open": true, "functions": {"F": {}}}},
            "types": {"T": {"base": "table", "mixins": ["M"]}}
        }"#).unwrap();
        let id = open.named_type_id("T").unwrap();
        assert!(open.members(id).contains_key("F"));
        assert!(!open.named_type(id).unwrap().has_methods);
        assert!(Definitions::from_json(r#"{"types": {"A": {"base": "table", "mixins": ["Missing"]}}}"#).is_err());
    }

    #[test]
    fn returned_templates() {
        let definitions = Definitions::from_json(r#"{"returned_templates": {"Pool.Acquire": {"AuraButton": "CustomAuraButtonTemplate"}}}"#).unwrap();
        assert_eq!(definitions.returned_templates("Pool.Acquire"), [("AuraButton".to_string(), "CustomAuraButtonTemplate".to_string())]);
        assert!(definitions.returned_templates("Pool.Release").is_empty());
        // By name, so a template that isn't known yet loads
        assert!(Definitions::from_json(r#"{"returned_templates": {"Pool.Acquire": ["AuraButton"]}}"#).is_err());
        assert!(Definitions::from_json(r#"{"returned_templates": {"Pool.Acquire": {"AuraButton": 1}}}"#).is_err());
    }

    #[test]
    fn object_method_errors() {
        assert!(Definitions::from_json(r#"{"types": {"A": {"base": "table", "inherits": ["Missing"]}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"types": {"A": {"base": "table", "method_tables": ["Missing"]}}}"#).is_err());
        let a: Value = serde_json::from_str(r#"{"types": {"A": {"base": "table"}}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"types": {"A": {"base": "userdata"}}}"#).unwrap();
        assert!(Definitions::from_values(&[("a".to_string(), a), ("b".to_string(), b)]).is_err());
        // A loop doesn't hang lookups
        let looped = Definitions::from_json(r#"{"types": {"A": {"base": "table", "inherits": ["B"]}, "B": {"base": "table", "inherits": ["A"]}}}"#).unwrap();
        assert!(looped.field(Type::named(looped.named_type_id("A").unwrap(), TypeSet::TABLE), "x").is_none());
    }

    /// The shipped definitions must load, or the LSP silently loses them
    #[test]
    fn sites_of_blizzard_code() {
        let directory = std::env::temp_dir().join(format!("wow_ls_sites_{}", std::process::id()));
        std::fs::create_dir_all(directory.join("sites")).unwrap();
        std::fs::write(directory.join("lua.json"), r#"{
            "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}},
            "method_tables": {"FrameAPI": {"functions": {"Show": {}}}},
            "mixins": {"TitleMixin": {"functions": {"SetTitle": {}}}},
            "elements": {"Frame": "Frame"},
            "templates": {"PanelT": {"type": "Frame", "mixins": ["TitleMixin"]}, "SubT": {"type": "Frame", "inherits": ["PanelT"]}},
            "tables": {"MenuUtil": {"functions": {"CreateContextMenu": {}}}}
        }"#).unwrap();
        std::fs::write(directory.join("sites/blizzard.json"), r#"{"files": {
            "A/Menu.lua": {"MenuUtil": 3, "MenuUtil.CreateContextMenu": 9},
            "A/Title.lua": {"TitleMixin.SetTitle": 5}
        }}"#).unwrap();
        std::fs::write(directory.join(SOURCE_PATH_FILE), "/ui\n").unwrap();
        let definitions = Definitions::load_directory(&directory).unwrap();
        // Read the first time one's needed
        assert_eq!(definitions.global_site("MenuUtil"), Some(("/ui/A/Menu.lua".into(), 3)));
        let menu = definitions.global("MenuUtil").unwrap().named.named_id().unwrap();
        assert_eq!(definitions.member_site(menu, "CreateContextMenu"), Some(("/ui/A/Menu.lua".into(), 9)));
        // A template's method from its mixin, however far up it's inherited
        let (sub, _) = definitions.template("SubT").unwrap();
        assert_eq!(definitions.member_site(sub, "SetTitle"), Some(("/ui/A/Title.lua".into(), 5)));
        // Not in Blizzard's code
        assert_eq!(definitions.member_site(sub, "Show"), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn types_naming_templates_and_frames() {
        let definitions = Definitions::from_json(r#"{
            "types": {"Button": {"base": "table", "method_tables": ["ButtonAPI"]}},
            "method_tables": {"ButtonAPI": {"functions": {"Click": {}}}},
            "mixins": {"DriverMixin": {"functions": {"Drive": {}}}},
            "elements": {"Button": "Button"},
            "functions": {"GetRack": {"returns": ["RackT"]}, "GetDriver": {"returns": ["Driver|nil"]}},
            "templates": {"RackT": {"type": "Button"}},
            "frames": {"Driver": {"type": "Button", "mixins": ["DriverMixin"]}}
        }"#).unwrap();
        let returned = |name: &str| match definitions.global(name).unwrap().function {
            crate::types::FunctionRef::Builtin(id) => definitions.function(id).unwrap().returns.values[0],
            _ => panic!("{name} isn't a function"),
        };
        let (rack, driver) = (returned("GetRack"), returned("GetDriver"));
        // Named before they're built, and the frame's type is its own, with its mixin
        assert_eq!(driver.named.named_id(), definitions.global("Driver").unwrap().named.named_id());
        assert!(definitions.field(driver, "Drive").is_some() && definitions.field(rack, "Click").is_some());
        // Shown by the native type, but written by name, so loading keeps them
        assert_eq!(definitions.type_text(rack), "Button");
        assert_eq!(definitions.type_text(driver), "Button|nil");
        assert_eq!(definitions.written_type_text(rack), "RackT");
        assert_eq!(definitions.written_type_text(driver), "Driver|nil");
        // A child's type, written as what it inherits when that's all it is,
        // else not at all rather than with fewer methods
        let definitions = Definitions::from_json(r#"{
            "types": {"Button": {"base": "table", "method_tables": ["ButtonAPI"]}},
            "method_tables": {"ButtonAPI": {"functions": {"Click": {}}}},
            "mixins": {"M": {"functions": {"Drive": {}}}},
            "elements": {"Button": "Button"},
            "templates": {
                "BoxT": {"type": "Button", "mixins": ["M"]},
                "OuterT": {"type": "Button", "children": {"Box": {"type": "Button", "inherits": ["BoxT"], "values": {"n": "number"}}, "Own": {"type": "Button", "inherits": ["BoxT"], "mixins": ["M"]}}}
            }
        }"#).unwrap();
        let child = |key: &str| definitions.xml_object(Some(&format!("template:OuterT.{key}")), "Button").unwrap();
        assert_eq!(definitions.written_type_text(child("Box")), "BoxT");
        assert_eq!(definitions.written_type_text(child("Own")), "unknown");
        assert_eq!(definitions.type_text(child("Box")), "Button");
    }

    #[test]
    fn repository_definitions_load() {
        let definitions = Definitions::load_directory(Path::new("definitions")).unwrap();
        for name in ["tostring", "string.format", "strsplit", "GetTime", "C_Timer.After"] {
            assert!(definitions.function_at(name).is_some(), "{name} missing");
        }
        for name in ["Frame", "Texture", "ItemLocation", "ItemKey"] {
            assert!(definitions.named_type_id(name).is_some(), "{name} missing");
        }
        let frame = Type::named(definitions.named_type_id("Button").unwrap(), TypeSet::TABLE);
        for method in ["Click", "SetScript", "SetPoint", "CreateTexture", "GetName", "Show"] {
            assert!(definitions.field(frame, method).is_some(), "Button:{method} missing");
        }
    }
}
