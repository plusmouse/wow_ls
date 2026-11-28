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

//! Showing types as text, for hover and tests

use super::*;

thread_local! {
    static MADE_WITH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While one is held, objects are described with the template and mixins
/// they're made with (`ButtonT & M`) rather than just their native type
/// (`Button`), as hover shows them: for looking into the analysis
pub struct MadeWith(bool);

impl MadeWith {
    pub fn shown() -> MadeWith {
        MadeWith(MADE_WITH.replace(true))
    }
}

impl Drop for MadeWith {
    fn drop(&mut self) {
        MADE_WITH.set(self.0);
    }
}

thread_local! {
    /// Whether strings are described as the literals they may be. Tests,
    /// most of which aren't about those, describe them as just `string`
    /// unless they ask (see `LiteralsShown`)
    static LITERALS_SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(!cfg!(test)) };
}

/// While one is held, strings are described as the literals they may be
/// (`"LEFT"|"RIGHT"`), as they always are outside tests
#[cfg(test)]
pub(super) struct LiteralsShown(bool);

#[cfg(test)]
impl LiteralsShown {
    pub(super) fn shown() -> LiteralsShown {
        LiteralsShown(LITERALS_SHOWN.replace(true))
    }
}

#[cfg(test)]
impl Drop for LiteralsShown {
    fn drop(&mut self) {
        LITERALS_SHOWN.set(self.0);
    }
}

impl Analysis {
    /// e.g. `number?`, `Frame|string` or `function(a, b) -> string`
    pub fn describe(&self, ty: Type) -> String {
        self.describe_with(ty, true)
    }

    /// Without function signatures, which could recurse forever on
    /// `function f() return f end`
    pub(super) fn describe_shallow(&self, ty: Type) -> String {
        self.describe_with(ty, false)
    }

    pub(super) fn describe_with(&self, ty: Type, signatures: bool) -> String {
        // Any of several objects: each as it would be on its own, once
        // (Not when nothing's known of what it is, which says just that)
        if matches!(ty.named, NamedRef::Several(_)) && !ty.set.names()[0].starts_with("unknown") {
            let mut objects: Vec<String> = Vec::new();
            for object in ty.named.alternatives() {
                // Each as what it can be: a table made in the files a table,
                // an object what its type is based on
                let base = match object {
                    NamedRef::Table(_) => TypeSet::TABLE,
                    _ => object.named_id().and_then(|id| self.definitions.named_type(id)).map_or(OBJECTS, |named| named.base),
                };
                let set = match base.intersection(ty.set) {
                    TypeSet::NEVER => ty.set.intersection(OBJECTS),
                    set => set,
                };
                let one = self.describe_with(Type { named: object, ..Type::of(set) }, signatures);
                if !objects.contains(&one) {
                    objects.push(one);
                }
            }
            let several = objects.len() > 1;
            let objects = objects.iter()
                .map(|one| if several && one.contains(' ') { format!("({one})") } else { one.clone() })
                .collect::<Vec<_>>()
                .join("|");
            let rest = ty.without(OBJECTS);
            return match rest.set {
                TypeSet::NEVER => objects,
                TypeSet::NIL if several => format!("({objects})?"),
                TypeSet::NIL => format!("{objects}?"),
                _ => format!("{objects}|{}", self.describe_with(rest, signatures)),
            }
        }
        let names = ty.set.names();
        let signature = match ty.function {
            FunctionRef::One(id) if signatures => self.function_info(FunctionRef::One(id)).map(|f| self.signature(f)),
            FunctionRef::Builtin(id) if signatures => self.definitions.function(id).map(|f| self.signature(f)),
            // Each of several, once, unless one isn't known yet
            FunctionRef::Several(_) if signatures => {
                let signatures: Option<Vec<String>> = ty.function.alternatives().into_iter()
                    .map(|function| self.function_info(function).map(|f| self.signature(f)))
                    .collect();
                signatures.map(|mut each| {
                    each.dedup();
                    match each.as_slice() {
                        [one] => one.clone(),
                        _ => each.iter().map(|one| format!("({one})")).collect::<Vec<_>>().join("|"),
                    }
                })
            }
            _ => None,
        };
        let named = ty.named.named_id()
            .and_then(|id| self.definitions.named_type(id))
            .filter(|named| !named.namespace);
        let numeric = match ty.numeric {
            NamedRef::One(id) => self.definitions.named_type(id),
            _ => None,
        };
        // Nested tables are just `table`, which also stops a table holding itself recursing
        let contents = if signatures { self.describe_contents(ty) } else { None };
        // A single type that can also be nil is written `T?`
        let optional = names.len() == 2 && names[1] == "nil" && !names[0].starts_with("unknown");
        let wrap = names.len() > 1;
        let names: Vec<String> = names.iter()
            .filter(|&&name| !(optional && name == "nil"))
            .map(|&name| match (&signature, named, name) {
                // Several signatures are each in brackets already, so
                // they only need more to be optional together
                (Some(s), _, "function") if s.starts_with('(') => if optional { format!("({s})") } else { s.clone() },
                (Some(s), _, "function") if wrap && s.contains(" -> ") => format!("({s})"),
                (Some(s), _, "function") => s.clone(),
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] && signatures
                    && ty.named.identity().is_some_and(|at| self.pool_object(at).is_some()) => {
                    let object = ty.named.identity().and_then(|at| self.pool_object(at)).unwrap_or(Type::UNKNOWN);
                    format!("{}<{}>", self.display_name(ty), self.describe_shallow(object))
                }
                // By its native type alone, not the template or mixins it's
                // made with, unless those are being shown
                (_, Some(named), "table" | "userdata") if named.base.names() == [name] => {
                    let mixins = match MADE_WITH.get() {
                        true => ty.named.identity().map(|at| self.object_mixins(at)).unwrap_or_default(),
                        false => Vec::new(),
                    };
                    match mixins.is_empty() {
                        true => self.display_name(ty),
                        false if wrap || optional => format!("({} & {})", named.name, mixins.join(" & ")),
                        false => format!("{} & {}", named.name, mixins.join(" & ")),
                    }
                }
                (_, _, "number") if numeric.is_some() => numeric.map_or_else(String::new, |n| n.name.clone()),
                // The strings it may be, as they're written
                (_, _, "string") if ty.literal.strings().is_some() && LITERALS_SHOWN.get() => ty.literal.strings().unwrap_or_default().iter()
                    .map(|text| if text.contains('"') { format!("'{text}'") } else { format!("\"{text}\"") })
                    .collect::<Vec<_>>()
                    .join("|"),
                (_, _, "table") if contents.is_some() => {
                    let contents = contents.clone().unwrap_or_default();
                    // `(string[])|number`, but `string[]?`
                    if wrap && !optional && contents.ends_with("[]") { format!("({contents})") } else { contents }
                }
                _ => name.to_string(),
            })
            .collect();
        let described = names.join("|");
        if optional { described + "?" } else { described }
    }

    /// The names of the mixins a WoW object made in the files has, in the order
    /// they were mixed in. In a mixin's own methods, `self` is the mixin
    pub(super) fn object_mixins(&self, at: TableId) -> Vec<String> {
        let mut names = Vec::new();
        match self.table_name(at) {
            Some(name) => names.push(name.clone()),
            None => self.collect_mixins(at, &mut names, 0),
        }
        names
    }

    /// Named mixins are listed by name. Unnamed ones (an inline
    /// `CreateFromMixins(A, B)`, or the frame a function made and returns) are
    /// listed by their own mixins
    pub(super) fn collect_mixins(&self, at: TableId, names: &mut Vec<String>, depth: usize) {
        // Deep enough for any real chain of mixins, and stops a loop
        if depth > 16 {
            return
        }
        for mixin in self.mixins.get(&at).or_else(|| self.layer.mixins.get(&at)).into_iter().flatten() {
            // One the definitions give, by its name
            let defined = mixin.named.named_id().and_then(|id| self.definitions.named_type(id)).filter(|named| named.namespace);
            if let Some(named) = defined {
                if !names.contains(&named.name) {
                    names.push(named.name.clone());
                }
                continue
            }
            let Some(mixin) = mixin.named.identity() else { continue };
            match self.table_name(mixin) {
                Some(name) => if !names.contains(name) { names.push(name.clone()) },
                None => self.collect_mixins(mixin, names, depth + 1),
            }
        }
    }

    /// The name a table was first stored under, here or in a dependency
    pub(super) fn table_name(&self, at: TableId) -> Option<&String> {
        self.table_names.get(&at).or_else(|| self.layer.table_names.get(&at))
    }

    /// How an object's type is shown: by its native type, or while
    /// `MadeWith` is held, by the template it's made from
    pub(super) fn display_name(&self, ty: Type) -> String {
        let Some(id) = ty.named.named_id() else { return String::new() };
        match MADE_WITH.get() {
            true => self.definitions.named_type(id).map(|named| named.name.clone()),
            false => self.definitions.display_name(id).map(str::to_string),
        }.unwrap_or_default()
    }

    /// What a pool made here or in a dependency holds
    pub(super) fn pool_object(&self, at: TableId) -> Option<Type> {
        self.pool_objects.get(&at).or_else(|| self.layer.pool_objects.get(&at)).copied()
    }

    pub(super) fn signature(&self, function: &FunctionInfo) -> String {
        self.signature_returning(function, &function.returns)
    }

    /// A callee's type, with what the call returns when that's known better
    /// than the function's own returns
    pub fn describe_callee(&self, ty: Type, call_returns: Option<&Values>) -> String {
        let function = self.function_info(ty.function);
        match (function, call_returns) {
            // Only when it's sure to be the function, so the signature is all of it
            (Some(function), Some(returns)) if ty.set == TypeSet::FUNCTION => self.signature_returning(function, returns),
            _ => self.describe(ty),
        }
    }

    pub(super) fn signature_returning(&self, function: &FunctionInfo, returns: &Values) -> String {
        let parameter = |p: &Parameter| match p.types {
            Type::UNKNOWN => p.name.clone(),
            types => format!("{}: {}", p.name, self.describe_shallow(types)),
        };
        let mut parameters: Vec<String> = function.parameters.iter().map(parameter).collect();
        if let Some(varargs) = &function.varargs {
            parameters.push(parameter(varargs));
        }
        let mut signature = format!("function({})", parameters.join(", "));
        let mut parts: Vec<String> = returns.values.iter().map(|&t| self.describe_shallow(t)).collect();
        if returns.rest != Type::NIL {
            parts.push("...".to_string());
        }
        if !parts.is_empty() {
            signature += " -> ";
            signature += &parts.join(", ");
        }
        if let Some(only) = &function.only {
            signature += &format!(" ({only} only)");
        }
        signature
    }
}
