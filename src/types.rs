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

//! Type tracking for un-annotated Lua, across the files of an addon.
//!
//! One walk over the tree resolves every variable name to a [`Binding`] (respecting
//! scopes and shadowing) and records a [`Reference`] with the type the variable has
//! at that point.
//!
//! Types are a [`TypeSet`] bitset, so unions (from branches, `and`/`or`, or
//! reassignment) are a single OR.
//!
//! Flow rules, chosen to over-approximate rather than be wrong:
//! * An assignment *replaces* a variable's current type if it is in the same block
//!   the variable was declared in (or an unconditional `do` block inside it).
//!   Otherwise (if branch, loop, function body) it is *unioned* with it.
//! * A read from inside a different function than the declaration (an upvalue)
//!   gets the union of every value ever assigned, since the closure could run at
//!   any time. Unless no assignment can come after the closure is made (every
//!   one is earlier in the declaring function, with no loop around the closure
//!   but not the declaration): then it's the type where the closure is made.
//! * Tables the game saves between sessions (`## SavedVariables`, and the
//!   tables stored in them) hold what the addon writes, not tables of unknown
//!   shape it stores there from outside, like an import.
//! * Globals start as unknown, because other files (or the WoW API) may set them.
//!
//! * Loop bodies are walked until the types flowing back to the top of the loop
//!   stop changing, so a read sees values assigned later in the body by an
//!   earlier iteration.
//!
//! * Values that can be functions remember which function (a [`FunctionRef`]),
//!   and each function records what it returns, so calls have result types.
//!
//! Files are walked in load order (their `.toc` order) as one program: globals
//! carry over from file to file, and functions and table fields are shared. In
//! an addon, `...` at the top of a file is the addon's name and its namespace
//! table, one table shared by all its files.
//!
//! The files are walked at least twice. On the first walk, reading an upvalue,
//! calling a function defined further down, or reading a parameter gives
//! unknown. Each later walk uses the previous walk's final types for those, which
//! stays sound because every walk can only over-approximate. Walking stops when
//! everything a walk reads from the one before is unchanged, or if a walk's
//! state repeats an earlier one's, as walking on would then cycle.
//!
//! * Parameters are typed by what the body requires of them, if it passes them
//!   unconditionally to a parameter declared with a single type (`floor(x)`
//!   makes `x` a number). Otherwise they get the union of the arguments at every
//!   call, if every call is visible: a local function only ever called directly.
//!   Otherwise they're unknown.
//! * Tables and userdata can be a named type from the definitions, like `Frame`
//!   or `ItemKey` (a [`NamedRef`]), so WoW objects aren't just `table`.
//! * Tables made in the file have an internal identity (never shown), so their
//!   fields are tracked: each `{ ... }` is one table, and a local or parameter
//!   holding a table of unknown origin (`local _, ns = ...`) gets a pseudo
//!   identity of its own. A field's type is the union of every value assigned to
//!   it anywhere in the file (`{ x = 1 }`, `t.x = 2`, `function t:x() end`), so
//!   a read before the first write isn't shown as nil. Writes through a table
//!   whose identity isn't known (or a WoW object, shared by every `Frame`) are
//!   kept by field name and used when nothing more specific is known.
//! * Globals the file never assigns can have preset types from [`Definitions`]
//!   (the Lua standard library, the WoW API), including dotted fields of those
//!   globals like `string.format`.
//!
//! * A call passing one of its function's parameters (or `...`) straight on
//!   takes it as settled at the end of the same walk, so a chain of them
//!   settles together rather than one link per walk.
//! * The first walk starts from nothing known rather than unknown, so types
//!   grow walk by walk: a cycle (a field written with what's read from it)
//!   settles on what really flows round it instead of staying unknown. If
//!   walks start to cycle between states, each from then on is combined with
//!   the one before, which only grows, so it settles covering both.
//! * Each walk records, for every function body, what it read and everything
//!   it did. On the next walk a body that read nothing which has changed since
//!   is replayed from that record instead of walked: the same result, without
//!   the work. Turning this off (`WOW_LS_NO_REPLAY`) is only for checking it
//! * A call is recursive when its caller and callee are in one cycle of
//!   calls, found from every function's calls once a walk is done, for the
//!   next walk. So a call depends only on whether the two are in one cycle,
//!   not on everything each function it reaches calls.
//! * Each record also notes, for each key the body influences (each write,
//!   each local it declares, its signature), what that depended on: what
//!   the statement doing it read, and what went into the locals it used,
//!   however indirectly. What a call passes depends on its callee and
//!   arguments, not on what the callee returns, and which functions a body
//!   calls on how each callee was reached. These say what an edit can
//!   change (see `reach`), and change nothing a walk finds
//!
//! Known gaps: globals the file doesn't assign (like saved variables) have no
//! table identity. Recursive calls return unknown.
//!
//! The modules: `value` is how types are represented, and `results` what an
//! analysis keeps. `analysis` runs walks until they settle. Each walk is a
//! [`Walker`] (`walker`), whose methods are split by what they walk
//! (`statements`, `expressions`, `calls`, `fields`, `tables`, `functions`,
//! `narrowing`, ...), keeping a `record` of each body. `queries` and
//! `describe` answer the editor, and `layer` carries an analysis over to the
//! addons that depend on it.

use std::fmt;
use std::sync::Arc;

use rowan::{NodeOrToken, TextRange, TextSize};

use crate::ast::*;
use crate::definitions::{BuiltinId, Definitions, NamedId};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

mod hash;
mod value;
mod results;
mod layer;
mod join;
mod describe;
mod analysis;
mod queries;
mod walker;
mod record;
mod bindings;
mod statements;
mod expressions;
mod fields;
mod tables;
mod functions;
mod calls;
mod checks;
mod objects;
mod summaries;
mod mixins;
mod library;
mod narrowing;

pub use hash::*;
pub use value::*;
pub use results::*;
pub use layer::*;
use join::*;
pub use describe::*;
pub use analysis::*;
pub use walker::*;
use record::*;
use statements::*;
use expressions::*;
use fields::*;
use tables::*;
use checks::*;
use summaries::*;
use mixins::*;
use narrowing::*;

mod reach;
mod shrink;
pub use reach::print_reach;
pub use shrink::{shrinkage, span};

#[cfg(test)]
mod tests;
