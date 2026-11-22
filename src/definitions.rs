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
//! * Named types (`types`) are tables or userdata with their own name, like WoW's
//!   `Frame`, shown by that name instead of `table`. Any file can use a named type
//!   declared in any other.
//!
//! Every `.json` file in the directory is loaded, in filename order, and a later
//! file's definition of a name replaces an earlier one. Tables are merged member
//! by member, so a later file can replace or add single members of `string`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::{FunctionInfo, FunctionRef, NamedRef, Parameter, Type, TypeSet, Values};

pub type BuiltinId = u32;
pub type NamedId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    /// For a namespace, its path (`string`, `C_AddOns`)
    pub name: String,
    /// TABLE or OTHER (userdata)
    pub base: TypeSet,
    /// Known members
    pub fields: HashMap<String, Type>,
    /// A namespace table like `string`, which only exists to hold its members.
    /// Shown as plain `table`, as it isn't a type code would name
    pub namespace: bool,
}

#[derive(Debug, Default)]
pub struct Definitions {
    functions: Vec<FunctionInfo>,
    /// Global functions and namespace tables
    globals: HashMap<String, Type>,
    types: Vec<NamedType>,
    type_names: HashMap<String, NamedId>,
}

impl Definitions {
    /// The preset type of a global, if it has one
    pub fn global(&self, name: &str) -> Option<Type> {
        self.globals.get(name).copied()
    }

    /// The type of member `name` of a value of type `ty`, if it's known
    pub fn field(&self, ty: Type, name: &str) -> Option<Type> {
        match ty.named {
            NamedRef::One(id) => self.named_type(id)?.fields.get(name).copied(),
            _ => None,
        }
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

    pub fn named_type_id(&self, name: &str) -> Option<NamedId> {
        self.type_names.get(name).copied()
    }

    /// Loads every `.json` file in `directory`
    pub fn load_directory(directory: &Path) -> Result<Definitions, String> {
        let entries = std::fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
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
        for (file, root) in files {
            definitions.add_members(root, None, "").map_err(|e| format!("{file}: {e}"))?;
        }
        Ok(definitions)
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
                _ => return Err(format!("{name}: `base` should be \"table\" or \"userdata\"")),
            };
            if parse_builtin_type(name).is_some() {
                return Err(format!("{name}: can't redefine a built-in type"));
            }
            let named = NamedType { name: name.clone(), base, fields: HashMap::new(), namespace: false };
            match self.type_names.get(name) {
                Some(&id) => self.types[id as usize] = named,
                None => {
                    self.type_names.insert(name.clone(), self.types.len() as NamedId);
                    self.types.push(named);
                }
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
                        self.types.push(NamedType { name: path.clone(), base: TypeSet::TABLE, fields: HashMap::new(), namespace: true });
                        self.set_member(namespace, name, Type::named(id, TypeSet::TABLE));
                        id
                    }
                };
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
        Ok(FunctionInfo { parameters, varargs, returns: Values { values, rest } })
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

    /// The shipped definitions must load, or the LSP silently loses them
    #[test]
    fn repository_definitions_load() {
        let definitions = Definitions::load_directory(Path::new("definitions")).unwrap();
        for name in ["tostring", "string.format", "strsplit", "GetTime", "C_Timer.After"] {
            assert!(definitions.function_at(name).is_some(), "{name} missing");
        }
        for name in ["Frame", "Texture", "ItemLocation", "ItemKey"] {
            assert!(definitions.named_type_id(name).is_some(), "{name} missing");
        }
    }
}
