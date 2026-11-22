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
//!   "functions": {
//!     "tostring": { "parameters": ["value"], "returns": ["string"] },
//!     "string.format": { "parameters": ["format", "..."], "returns": ["string"] },
//!     "select": { "parameters": ["index", "..."], "returns": ["unknown"], "rest": "unknown" }
//!   }
//! }
//! ```
//!
//! * A dotted name is a field of a global table (`string.format`).
//! * `"..."` as the last parameter means the function takes varargs.
//! * `returns` has a type per return value. `rest` is the type of every value
//!   after those, for functions returning a variable number; it defaults to nil.
//! * A type is names joined with `|`: nil, false, true, boolean, number, string,
//!   function, table, userdata, thread, unknown.
//!
//! Every `.json` file in the directory is loaded, in filename order, and a later
//! file's definition of a name replaces an earlier one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::{FunctionInfo, Type, TypeSet, Values};

pub type BuiltinId = u32;

#[derive(Debug, Default)]
pub struct Definitions {
    functions: Vec<FunctionInfo>,
    names: HashMap<String, BuiltinId>,
}

impl Definitions {
    pub fn function_id(&self, name: &str) -> Option<BuiltinId> {
        self.names.get(name).copied()
    }

    pub fn function(&self, id: BuiltinId) -> Option<&FunctionInfo> {
        self.functions.get(id as usize)
    }

    pub fn len(&self) -> usize {
        self.functions.len()
    }

    /// Loads every `.json` file in `directory`
    pub fn load_directory(directory: &Path) -> Result<Definitions, String> {
        let entries = std::fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        let mut definitions = Definitions::default();
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            definitions.add_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(definitions)
    }

    pub fn from_json(text: &str) -> Result<Definitions, String> {
        let mut definitions = Definitions::default();
        definitions.add_json(text)?;
        Ok(definitions)
    }

    fn add_json(&mut self, text: &str) -> Result<(), String> {
        let root: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let functions = match root.get("functions") {
            Some(Value::Object(functions)) => functions,
            Some(_) => return Err("`functions` should be an object".to_string()),
            None => return Ok(()),
        };
        for (name, definition) in functions {
            let function = parse_function(definition).map_err(|e| format!("{name}: {e}"))?;
            match self.names.get(name) {
                Some(&id) => self.functions[id as usize] = function,
                None => {
                    self.names.insert(name.clone(), self.functions.len() as BuiltinId);
                    self.functions.push(function);
                }
            }
        }
        Ok(())
    }
}

fn parse_function(definition: &Value) -> Result<FunctionInfo, String> {
    let strings = |key: &str| -> Result<Vec<String>, String> {
        match definition.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter()
                .map(|item| item.as_str().map(str::to_string).ok_or_else(|| format!("`{key}` should only hold strings")))
                .collect(),
            Some(_) => Err(format!("`{key}` should be an array")),
        }
    };
    let mut parameters = strings("parameters")?;
    let varargs = parameters.last().is_some_and(|p| p == "...");
    if varargs {
        parameters.pop();
    }
    if parameters.iter().any(|p| p == "...") {
        return Err("`...` can only be the last parameter".to_string());
    }
    let values = strings("returns")?.iter().map(|t| parse_type(t)).collect::<Result<Vec<_>, _>>()?;
    let rest = match definition.get("rest") {
        None => Type::NIL,
        Some(Value::String(t)) => parse_type(t)?,
        Some(_) => return Err("`rest` should be a string".to_string()),
    };
    Ok(FunctionInfo { parameters, varargs, returns: Values { values, rest } })
}

pub fn parse_type(text: &str) -> Result<Type, String> {
    let mut set = TypeSet::NEVER;
    for name in text.split('|') {
        set |= match name.trim() {
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
            other => return Err(format!("unknown type `{other}` in `{text}`")),
        };
    }
    Ok(Type::of(set))
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
            "f": {"parameters": ["a", "..."], "returns": ["number|nil", "string"], "rest": "unknown"},
            "g": {}
        }}"#).unwrap();
        let f = definitions.function(definitions.function_id("f").unwrap()).unwrap();
        assert_eq!(f.parameters, ["a"]);
        assert!(f.varargs);
        assert_eq!(f.returns.values, [Type::of(TypeSet::NUMBER | TypeSet::NIL), Type::of(TypeSet::STRING)]);
        assert_eq!(f.returns.rest, Type::UNKNOWN);
        let g = definitions.function(definitions.function_id("g").unwrap()).unwrap();
        assert_eq!(g.returns, Values::none());
    }

    #[test]
    fn later_definitions_replace_earlier() {
        let mut definitions = Definitions::from_json(r#"{"functions": {"f": {"returns": ["number"]}}}"#).unwrap();
        definitions.add_json(r#"{"functions": {"f": {"returns": ["string"]}}}"#).unwrap();
        assert_eq!(definitions.len(), 1);
        let f = definitions.function(definitions.function_id("f").unwrap()).unwrap();
        assert_eq!(f.returns.values, [Type::of(TypeSet::STRING)]);
    }

    #[test]
    fn reports_errors() {
        let error = Definitions::from_json(r#"{"functions": {"f": {"returns": ["nubmer"]}}}"#).unwrap_err();
        assert!(error.contains("f:") && error.contains("nubmer"), "{error}");
        assert!(Definitions::from_json(r#"{"functions": {"f": {"parameters": ["...", "a"]}}}"#).is_err());
        assert!(Definitions::from_json(r#"{"functions": []}"#).is_err());
        assert!(Definitions::from_json("{").is_err());
    }

    /// The shipped definitions must load, or the LSP silently loses them
    #[test]
    fn repository_definitions_load() {
        let definitions = Definitions::load_directory(Path::new("definitions")).unwrap();
        for name in ["tostring", "string.format", "strsplit", "GetTime"] {
            assert!(definitions.function_id(name).is_some(), "{name} missing");
        }
    }
}
