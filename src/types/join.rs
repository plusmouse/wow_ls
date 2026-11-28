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

//! Joining what one analysis (or layer) holds into another's, key by key

use std::hash::Hash;

use super::*;

/// Joins each type `from` has into the one `into` has for the same key
pub(super) fn join_types<K: Clone + Eq + Hash>(into: &mut HashMap<K, Type>, from: &HashMap<K, Type>) {
    for (key, &ty) in from {
        *into.entry(key.clone()).or_default() |= ty;
    }
}

/// `join_types` for the fields of each table
pub(super) fn join_fields(into: &mut HashMap<TableId, HashMap<String, Type>>, from: &HashMap<TableId, HashMap<String, Type>>) {
    for (at, fields) in from {
        join_types(into.entry(*at).or_default(), fields);
    }
}

/// Joins each set of values `from` has into the one `into` has for the same key
pub(super) fn join_values<K: Clone + Eq + Hash>(into: &mut HashMap<K, Values>, from: &HashMap<K, Values>) {
    for (key, values) in from {
        into.entry(key.clone())
            .and_modify(|all| *all = all.union(values))
            .or_insert_with(|| values.clone());
    }
}

/// Joins what each table's entries can be
pub(super) fn join_entries(into: &mut HashMap<TableId, Entries>, from: &HashMap<TableId, Entries>) {
    for (at, entries) in from {
        let mine = into.entry(*at).or_default();
        mine.keys |= entries.keys;
        mine.values |= entries.values;
    }
}

/// Adds the items of `from` that `into` doesn't have yet, in order
pub(super) fn add_new<T: PartialEq + Copy>(into: &mut Vec<T>, from: &[T]) {
    for &item in from {
        if !into.contains(&item) {
            into.push(item);
        }
    }
}
