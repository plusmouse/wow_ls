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

//! The [`Walker`], which walks the files once, and its scopes and snapshots

use super::*;

pub(super) struct Scope {
    pub(super) names: HashMap<String, BindingId>,
    /// Code in this scope might not run (or might run many times)
    pub(super) conditional: bool,
    /// A branch of an `if`, which joins what its branches leave (see
    /// `Join`), so assignments in it replace what a variable held
    pub(super) joined: bool,
    /// Where the scope's code ends, after which its locals can't be seen
    pub(super) end: TextSize,
}

#[derive(Default)]
pub(super) struct Walker<'a> {
    /// Results of the previous walk, used where this walk doesn't know yet
    pub(super) prior: Option<&'a Analysis>,
    /// Each file's tree
    pub(super) roots: Vec<SyntaxNode>,
    /// What the addons depended on make visible
    pub(super) layer: Arc<Layer>,
    /// What changed between the previous walk and the one before it, so
    /// bodies that read none of it can be replayed
    pub(super) changed: HashSet<Read>,
    /// Shared results that bodies walked (not replayed) on this walk changed
    pub(super) dirty: HashSet<Read>,
    /// Whether bodies can be replayed, rather than always walked
    pub(super) replay: bool,
    /// Whether walks have settled once, so what isn't known yet is taken to
    /// be unknown. Until then, what a table made in the files that nothing's
    /// been seen stored in holds is nothing yet, as are the parameters of a
    /// function with no calls seen yet: so values passed round a loop (a
    /// pool's frames, released into its list of free ones and acquired from
    /// it again) grow from what's given them, rather than from unknown,
    /// which they'd never be rid of. Once walks settle, such a table holds
    /// unknown, as it may be filled where that can't be seen, and a function
    /// that may be called where it isn't seen takes anything, and walks go
    /// on until they settle again
    pub(super) settled: bool,
    /// Whether this walk took something not known yet to be nothing (see
    /// `settled`), outside any body it replayed (whose record notes what it
    /// read), so walks go on once they settle
    pub(super) provisional: std::cell::Cell<bool>,
    /// The function bodies being walked, innermost last, and what each has read
    pub(super) frames: Vec<Frame>,
    pub(super) reads: std::cell::RefCell<Vec<HashSet<Read>>>,
    /// What each body's writes depend on, innermost body last
    pub(super) deps: std::cell::RefCell<Vec<BodyDeps>>,
    /// For each call being walked, the depth of its level in the innermost
    /// body's `levels` (see `Walker::call`), and what was read finding its
    /// callee, which is all which function it calls depends on
    pub(super) call_scopes: Vec<CallScope>,
    /// Where what finding the callee of the call whose returns are being
    /// read is in its statement's logs, for `returns_of`: the level, then
    /// the stretches of its reads and locals
    pub(super) callee_found: Option<(usize, std::ops::Range<usize>, std::ops::Range<usize>)>,
    pub(super) records: HashMap<FunctionId, Arc<Record>>,
    pub(super) replayed: usize,
    pub(super) walked: usize,
    /// The file being walked
    pub(super) file: FileId,
    /// The addon that file is part of, by its place in load order
    pub(super) addon: Option<u32>,

    pub(super) definitions: Arc<Definitions>,
    /// The preset types at `BUILTIN_PATHS`, as calls check for them by path
    pub(super) builtins: Vec<(&'static str, Option<Type>)>,
    pub(super) bindings: Vec<Binding>,
    /// Type of each binding at the current point of the walk
    pub(super) current: Vec<Type>,
    /// While walking a loop, each binding whose current type was set, with
    /// the type before, so a pass compares only what it changed
    pub(super) loop_log: Vec<(BindingId, Type)>,
    /// The `if` statements being walked, innermost last
    pub(super) joins: Vec<Join>,
    /// Where the constructor is that the table constructor about to be
    /// walked is written directly inside (see `ELEMENT`)
    pub(super) element_of: Option<TextSize>,
    /// Loops being walked, innermost last
    pub(super) loop_depth: usize,
    pub(super) references: Vec<Reference>,
    pub(super) field_references: Vec<FieldReference>,
    pub(super) diagnostics: Vec<Diagnostic>,
    /// References that read an upvalue, resolved once all assignments are known
    pub(super) deferred: Vec<usize>,
    pub(super) scopes: Vec<Scope>,
    pub(super) globals: HashMap<String, BindingId>,
    /// Names added to `globals`, in order, so a loop pass can be undone
    pub(super) globals_created: Vec<String>,
    pub(super) function: usize,
    pub(super) function_count: usize,
    pub(super) functions: HashMap<FunctionId, FunctionInfo>,
    /// Union of the `return`s seen so far, for each function being walked
    pub(super) returns: Vec<Option<Values>>,
    /// The functions called so far, for each function being walked
    pub(super) calls: Vec<Vec<FunctionId>>,
    /// Index in `scopes` of the current function's body
    pub(super) function_scope: usize,
    /// Inside the right of an `and`/`or`, which might not run
    pub(super) short_circuit_depth: usize,
    /// Fields narrowed here (see `Target::Field`), by their local and names.
    /// Any call or field write may change a field, through another name for
    /// its table too, so either forgets them all, as entering a loop does
    pub(super) narrowed_fields: Vec<(BindingId, Vec<String>, Type)>,
    /// Counts each time `narrowed_fields` is forgotten, so what's restored
    /// after a branch isn't what the branch made stale
    pub(super) field_epoch: usize,
    /// Union of the arguments at every direct call of each function, except
    /// those in `pending_calls`
    pub(super) call_arguments: HashMap<FunctionId, Values>,
    /// Calls passing a parameter straight on, settled at the end of the walk
    pub(super) pending_calls: Vec<PendingCall>,
    /// Each function's id, by its number in the walk
    pub(super) function_numbers: HashMap<usize, FunctionId>,
    /// Each parameter's type on entry to its function, to tell when it's
    /// been narrowed
    pub(super) parameter_entry: HashMap<BindingId, Type>,
    /// The single type each parameter has been required to have so far. None
    /// when two requirements conflicted
    pub(super) mandates: HashMap<BindingId, Option<Type>>,
    /// The local each function literal was directly assigned to
    pub(super) assigned_to: HashMap<FunctionId, BindingId>,
    /// Field writes seen on this walk. Reads use the previous walk's, as a
    /// field can be written after (or in another function from) a read
    pub(super) fields: HashMap<TableId, HashMap<String, Type>>,
    pub(super) any_table_fields: HashMap<String, Type>,
    /// Data types given fields that can't be named, seen on this walk
    pub(super) opened_types: HashSet<NamedId>,
    /// Entries seen on this walk. Reads use the previous walk's, like fields
    pub(super) entries: HashMap<TableId, Entries>,
    /// For each stand-in, the tables it stands for, so far on this walk.
    /// Reads add to it too (see `Walker::read_through`)
    pub(super) stand_ins: std::cell::RefCell<HashMap<TableId, Vec<TableId>>>,
    /// Metatables seen on this walk. Reads use the previous walk's, like fields
    pub(super) metatables: HashMap<TableId, Type>,
    /// Worked out with parameter types, at the end of the walk
    pub(super) vararg_values: HashMap<FunctionId, Values>,
    /// The functions being walked, innermost last
    pub(super) function_ids: Vec<FunctionId>,
    /// Calls to iterator makers like `pairs` and `gmatch`, by where they are,
    /// and what each variable of a loop over them gets, when that's known
    pub(super) iterated: HashMap<Location, Values>,
    /// Parents seen on this walk. Reads use the previous walk's, like fields
    pub(super) parents: HashMap<TableId, Type>,
    /// What callback registries' events passed on this walk (see
    /// `Analysis::triggered`). Reads use the previous walk's
    pub(super) triggered: HashMap<(TableId, u64), Values>,
    /// The field names each function was stored under, in any table
    pub(super) stored_in_fields: HashMap<FunctionId, HashSet<String>>,
    /// Mixins seen on this walk. Reads use the previous walk's, like fields
    pub(super) mixins: HashMap<TableId, Vec<Type>>,
    /// What each table made in the files was mixed into on this walk
    pub(super) mixed_into: HashMap<TableId, Vec<NamedRef>>,
    /// The table each copy `CopyTable` made is of
    pub(super) copies: HashMap<TableId, TableId>,
    /// Pools made on this walk, and what they hold
    pub(super) pool_objects: HashMap<TableId, Type>,
    /// Functions passed straight to a pool, which calls them only as it
    /// documents (with its objects)
    pub(super) pool_callbacks: HashSet<FunctionId>,
    /// Functions read where a call can't be seen to be all that's done
    /// with them (`t[k]` not called), so code that can't be seen may call
    /// them
    pub(super) escaped_functions: HashSet<FunctionId>,
    /// For each method, what's passed as its `self` by calls that pass one
    /// (`a:m()`, `t.m(a)`), and by the game to a script handler
    pub(super) self_passed: HashMap<FunctionId, (Type, Type)>,
    /// Calls to a pool's `EnumerateActive`, by where they are, and the objects
    /// the iterator gives
    pub(super) enumerated: HashMap<Location, Type>,
    /// The function being walked's code, to tell what it makes, for each
    /// function being walked
    pub(super) function_ranges: Vec<(TextRange, usize)>,
    /// Where each function walked (or replayed) ends, to tell what's
    /// declared in it
    pub(super) function_ends: HashMap<FunctionId, TextSize>,
    /// The stand-ins parameters have for a table they can't identify, which
    /// are already theirs
    pub(super) parameter_pseudos: HashSet<TableId>,
    /// For each function being walked, its `FunctionInfo::loop_edges`
    pub(super) loop_edges: Vec<Vec<LoopEdge>>,
    /// For each function being walked, its `FunctionInfo::returned_parameters`
    pub(super) returned_parameters: Vec<Vec<usize>>,
    /// The function each call to one of the files' functions calls, by where
    /// the call is
    pub(super) callees: HashMap<(FileId, TextRange), FunctionId>,
    /// How many loops were being walked where each function in
    /// `function_ranges` is
    pub(super) closure_loop_depths: Vec<usize>,
    /// Per function being walked: what each return position returns new, so
    /// far (None once returns disagree)
    pub(super) fresh: Vec<Option<Vec<Option<FreshObject>>>>,
    /// Per function being walked: the tables and objects it has stored in a
    /// variable outside it (an upvalue or a global), which it may return
    /// again from another call, as a cache is, so aren't new each call
    pub(super) kept_outside: Vec<HashSet<TableId>>,
    /// Per function being walked: the methods it calls that `ForwardedCall`s
    /// stand for
    pub(super) forwarded: Vec<Vec<ForwardedCall>>,
    /// Per function being walked: the fields it gives what it's passed, on
    /// `self`, a parameter, or a table it makes (which `FieldWrite`s name by
    /// the return that gives it, once that's known)
    pub(super) field_writes: Vec<Vec<(Result<On, TableId>, String, Vec<Forwarded>)>>,
    /// Per function being walked: for each return, the field of `self` or a
    /// parameter it returns, so far (None once returns disagree)
    pub(super) returned_fields: Vec<Option<Vec<Option<ReturnedField>>>>,
    /// Per function being walked: what a loop over what it returns gets, so
    /// far (Some(None) once a return gives none)
    pub(super) iterating: Vec<Option<Option<Values>>>,
    /// Per function being walked: its `self`, if it's a method
    pub(super) self_bindings: Vec<Option<BindingId>>,
    /// Which parameter of its function each parameter binding is
    pub(super) parameter_index: HashMap<BindingId, usize>,
    /// For each frame made with a kind its function is passed, which of the
    /// function's parameters that is (see `FreshObject::kind_parameter`)
    pub(super) kind_parameters: HashMap<TableId, usize>,
    /// For tables and objects made in a function: which of its parameters
    /// were mixed into them
    pub(super) parameter_mixins: HashMap<TableId, Vec<usize>>,
}

/// A call being walked: where its stretch of its statement's logs (and
/// what the statement did) starts, and where finding its callee ended
pub(super) struct CallScope {
    pub(super) level: usize,
    pub(super) reads: usize,
    pub(super) locals: usize,
    pub(super) writes: usize,
    pub(super) pending: usize,
    pub(super) callee: Option<(usize, usize)>,
    pub(super) ended: bool,
}

/// A function body being walked, for its `Record`
pub(super) struct Frame {
    pub(super) bindings_start: usize,
    pub(super) references_start: usize,
    pub(super) field_references_start: usize,
    pub(super) diagnostics_start: usize,
    pub(super) pending_calls_start: usize,
    pub(super) deferred_start: usize,
    pub(super) globals_created_start: usize,
    pub(super) function_count_start: usize,
    pub(super) writes: Vec<Arc<Write>>,
    /// The body's own, not those of the functions inside it
    pub(super) own_writes: Vec<Arc<Write>>,
    /// Upvalues read as they are where the body is (see `upvalue_here`)
    pub(super) upvalues: Vec<(BindingId, Type)>,
}

/// A call passing a parameter (or `...`) of the code calling it straight on.
/// Its arguments are settled with that parameter at the end of the walk,
/// rather than taken from the previous walk, so a chain of such calls
/// (`a(1)` calling `b(x)` calling `c(y)`) settles in one walk
#[derive(Debug, Clone)]
pub(super) struct PendingCall {
    pub(super) callee: FunctionId,
    /// The arguments as the walk saw them, by the callee's parameter positions
    pub(super) known: Values,
    /// Positions holding a parameter passed on: which function's, which
    /// parameter, and the types it was narrowed to at the call, if it was
    pub(super) parameters: Vec<(usize, FunctionId, usize, Option<TypeSet>)>,
    /// From this position on, the arguments are that function's own `...`
    pub(super) varargs: Option<(usize, FunctionId)>,
    /// Whether the call is recursive, so what it passes on of `...` is joined
    /// (see `resolve_pending`)
    pub(super) recursive: bool,
}

/// How much had been recorded before a loop pass, to discard that pass
pub(super) struct Snapshot {
    pub(super) bindings: usize,
    pub(super) references: usize,
    pub(super) field_references: usize,
    pub(super) diagnostics: usize,
    pub(super) deferred: usize,
    pub(super) globals_created: usize,
    pub(super) pending_calls: usize,
    /// So each pass numbers the loop's functions the same, and the functions
    /// after it are numbered the same whatever passes the loop took, which
    /// replaying them needs
    pub(super) function_count: usize,
}

thread_local! {
    /// Whether analyses keep the last walk's records, for `reach`
    pub(super) static KEEP_RECORDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with analyses keeping the last walk's records
pub fn keeping_records<R>(f: impl FnOnce() -> R) -> R {
    KEEP_RECORDS.with(|keep| keep.set(true));
    let result = f();
    KEEP_RECORDS.with(|keep| keep.set(false));
    result
}

/// Every file's top level, which run one after another
pub(super) const MAIN_CHUNK: usize = 0;

/// The table every file of an addon gets as the second value of `...`, by the
/// addon's place in load order. Not a real file position, so it can't clash
/// with a table made in a file
pub(super) fn addon_namespace(addon: u32) -> TableId {
    Location { file: FileId(u32::MAX - addon), offset: TextSize::new(0), slot: 0 }
}

/// The library functions calls are checked against, found once for each walk
/// rather than at every call
pub(super) const BUILTIN_PATHS: [&str; 23] = [
    "assert", "getmetatable", "gmatch", "ipairs", "next", "pairs", "pcall", "rawget", "setmetatable", "unpack", "xpcall",
    "string.gmatch", "table.create", "table.insert", "table.remove", "tinsert", "tremove",
    "CreateFontStringPool", "CreateFramePool", "CreateObjectPool", "CreateTexturePool",
    "CallbackRegistryMixin.RegisterCallback", "CallbackRegistryMixin.TriggerEvent",
];

/// Passes after which a loop that still hasn't settled is widened: any variable
/// whose type is still changing becomes unknown. Without this, a loop already
/// terminates (each pass adds at least one bit to some variable's TypeSet), but
/// nested loops multiply their passes. Realistic code settles in 2-3 passes
pub(super) const LOOP_PASSES_BEFORE_WIDENING: usize = 8;

impl<'a> Walker<'a> {
    pub(super) fn new(prior: Option<&'a Analysis>, definitions: &Arc<Definitions>, layer: &Arc<Layer>, changed: HashSet<Read>, replay: bool) -> Walker<'a> {
        let builtins = BUILTIN_PATHS.iter().map(|&path| (path, definitions.at_path(path))).collect();
        // Stand-ins only grow: this walk starts with all the walk before found
        let stand_ins = std::cell::RefCell::new(prior.map(|prior| prior.stand_ins.clone()).unwrap_or_default());
        Walker { prior, changed, replay, layer: layer.clone(), definitions: definitions.clone(), builtins, stand_ins, ..Walker::default() }
    }

    /// A global's preset type: what a dependency gives it, else the
    /// definitions'
    pub(super) fn preset_global(&self, name: &str) -> Option<Type> {
        self.layer.globals.get(name).copied().or_else(|| self.definitions.global(name))
    }

    /// Notes a read for each function body being walked
    pub(super) fn note(&self, read: Read) {
        // Only the innermost: each body's reads join its parent's when it ends
        if let Some(body) = self.deps.borrow_mut().last_mut() {
            let number = body.key(&read);
            body.noted.insert(number);
            // One of its own locals: what went into it, once that's all known
            let local = match read {
                Read::Binding(id) if id >= body.bindings_start && self.bindings.get(id).is_some_and(|b| b.kind != BindingKind::Global) => Some(body.local(id)),
                _ => None,
            };
            if let Some(level) = body.levels.last_mut() {
                match local {
                    Some(local) => level.locals.push(local),
                    None => level.reads.push(number),
                }
            }
        }
        // Outside any body: only a body's reads are recorded. A body's are
        // its numbered keys, joined with those of the functions inside it
        // once it's done (see `function_body`)
        if self.deps.borrow().is_empty()
            && let Some(innermost) = self.reads.borrow_mut().last_mut() {
            innermost.insert(read);
        }
    }

    /// Each file's top level runs in turn, so globals continue from one file to
    /// the next. Locals end with their file
    pub(super) fn walk(mut self, files: &[SourceFile]) -> Analysis {
        self.roots = files.iter().map(|file| file.root.clone()).collect();

        for (i, file) in files.iter().enumerate() {
            self.file = FileId(i as u32);
            self.addon = file.addon;
            if let Some(block) = Block::cast(file.root.clone()) {
                self.push_scope(false, file.root.text_range().end());
                if let Some(handler) = &file.handler {
                    self.declare_handler(handler, file.root.text_range());
                }
                self.statements(&block);
                self.pop_scope();
            }
        }
        let prior = self.prior;
        let mut analysis = self.finish();
        // The same calls as the walk before, so the same cycles
        let same_calls = prior.is_some_and(|prior| prior.functions.len() == analysis.functions.len()
            && analysis.functions.iter().all(|(id, f)| prior.functions.get(id).is_some_and(|p| p.calls == f.calls)));
        match prior {
            Some(prior) if same_calls => analysis.call_cycles = prior.call_cycles.clone(),
            _ => analysis.find_call_cycles(),
        }
        analysis.find_field_loops();
        analysis.find_entry_loops();
        analysis
    }

    /// `self` and the other arguments of an inline XML handler, which its
    /// body (the file's top level) sees as the game's function's parameters
    pub(super) fn declare_handler(&mut self, handler: &Handler, range: TextRange) {
        let depth = self.scopes.len() - 1;
        let parameters = std::iter::once(("self", handler.self_type)).chain(handler.parameters.iter().map(|(name, types)| (name.as_str(), *types)));
        for (name, types) in parameters {
            let id = self.new_binding(name, BindingKind::Parameter, None, types, MAIN_CHUNK, depth);
            self.bindings[id].assigned = true;
            self.bindings[id].visible = Some(range);
            self.scopes[depth].names.insert(name.to_string(), id);
        }
    }

    pub(super) fn location(&self, offset: TextSize) -> Location {
        Location { file: self.file, offset, slot: 0 }
    }

    pub(super) fn function_id(&self, def: &FunctionDefinition) -> FunctionId {
        self.location(def.syntax().text_range().start())
    }

    /// What `...` is here. At the top of an addon's file: its name and
    /// namespace table. In a function: what every call passed past its named
    /// parameters, as the previous walk settled it
    pub(super) fn vararg_values(&self) -> Option<Values> {
        if self.function == MAIN_CHUNK {
            return self.addon.map(|addon| Values {
                values: vec![Type::of(TypeSet::STRING), Type::table(addon_namespace(addon))],
                rest: Type::NIL,
            })
        }
        match self.prior {
            // Nothing known yet: types grow from here, walk by walk
            None => Some(Values::nothing()),
            Some(prior) => {
                let function = self.function_ids.last()?;
                self.note(Read::Varargs(*function));
                prior.vararg_values.get(function).cloned()
            }
        }
    }

    pub(super) fn finish(mut self) -> Analysis {
        self.settle_parameter_types();
        // A table in a loop of entries: with what it had the walk before too,
        // so it only grows (see `Analysis::find_entry_loops`)
        if let Some(prior) = self.prior {
            for at in &prior.entry_loops {
                if let Some(&before) = prior.entries.get(at) {
                    let entries = self.entries.entry(*at).or_default();
                    entries.keys |= before.keys;
                    entries.values |= before.values;
                }
            }
        }
        let method_selves = self.method_selves();
        for binding in self.bindings.iter_mut() {
            if binding.kind == BindingKind::Global && !binding.assigned {
                binding.types = self.layer.globals.get(&*binding.name).copied()
                    .or_else(|| self.definitions.global(&binding.name))
                    .unwrap_or(Type::UNKNOWN);
            }
        }
        let function_fields = self.fields.values().flat_map(|fields| fields.iter())
            .chain(self.any_table_fields.iter())
            .filter(|(_, ty)| ty.set.contains(TypeSet::FUNCTION))
            .map(|(name, _)| name.clone())
            .collect();
        // XML's mixins that are tables here: `self` in their methods is the
        // object XML mixes each into
        let mut lua_mixins: HashMap<String, Type> = HashMap::default();
        for (name, target) in self.definitions.xml_mixins() {
            if !lua_mixins.contains_key(name) {
                let own = self.globals.get(name.as_str()).map(|&id| &self.bindings[id]).filter(|b| b.assigned).map(|b| b.types);
                if let Some(ty) = own.or_else(|| self.layer.globals.get(name.as_str()).copied()) {
                    lua_mixins.insert(name.clone(), ty);
                }
            }
            if let Some(at) = lua_mixins.get(name).and_then(|ty| ty.named.identity()) {
                self.mixed_into.entry(at).or_default().push(NamedRef::One(*target));
            }
        }
        // And what XML makes those objects in, for `self:GetParent()`
        for (name, parent) in self.definitions.xml_parents() {
            let own = self.globals.get(name.as_str()).map(|&id| &self.bindings[id]).filter(|b| b.assigned).map(|b| b.types);
            let mixin = own.or_else(|| self.layer.globals.get(name.as_str()).copied());
            if let Some(at) = mixin.and_then(|ty| ty.named.identity()) {
                *self.parents.entry(at).or_default() |= Type::named(*parent, TypeSet::TABLE);
            }
        }
        let mixed_into = self.mixed_into.keys()
            .filter_map(|&mixin| match self.mixin_target(mixin, 0) {
                MixinTarget::Object(named, extended) => Some((mixin, (named, extended))),
                _ => None,
            })
            .collect();
        // The tables among what each mixin was mixed into, in a fixed order
        let mixin_copies = self.mixed_into.iter()
            .filter_map(|(&mixin, objects)| {
                let mut tables: Vec<TableId> = objects.iter().flat_map(|object| match object {
                    NamedRef::Table(_) | NamedRef::Tables(_) => object.identities(),
                    _ => Vec::new(),
                }).collect();
                tables.sort();
                tables.dedup();
                (!tables.is_empty()).then_some((mixin, tables))
            })
            .collect();
        Analysis {
            function_fields,
            mixed_into,
            mixin_copies,
            provisional: self.provisional.get(),
            lua_mixins,
            // Filled in by `complete`, once the last walk is known
            table_names: HashMap::default(),
            deferred: self.deferred,
            pending_calls: self.pending_calls,
            records: self.records,
            replayed: self.replayed,
            walked: self.walked,
            pool_objects: self.pool_objects,
            parents: self.parents,
            triggered: self.triggered,
            mixins: self.mixins,
            copies: self.copies,
            diagnostics: self.diagnostics,
            bindings: self.bindings,
            references: self.references,
            field_references: self.field_references,
            functions: self.functions,
            definitions: self.definitions,
            fields: self.fields,
            any_table_fields: self.any_table_fields,
            opened_types: self.opened_types,
            entries: self.entries,
            metatables: self.metatables,
            vararg_values: self.vararg_values,
            call_cycles: HashMap::default(),
            field_loops: HashSet::default(),
            entry_loops: HashSet::default(),
            walks: 1,
            cycled: false,
            roots: self.roots.iter().map(|root| root.green().into_owned()).collect(),
            layer: self.layer,
            // Found once the walk is done
            saved_tables: HashSet::default(),
            // With those of the walk before, which it started with
            stand_ins: self.stand_ins.into_inner(),
            method_selves,
        }
    }

    /// Each parameter's type for the next walk: what the body requires, else the
    /// arguments from every call when every call is visible, else unknown
    /// For each function, whether every call to it is one the walk saw: it
    /// was assigned straight to a variable (local or global) only ever called,
    /// which never holds another function, and any field it was stored in is
    /// only ever called, with no reads of a field by that name that could be
    /// it (`unresolved`)
    pub(super) fn all_calls_seen(&self, only_called: &[bool], escaped: &HashSet<FunctionId>, unresolved: &HashSet<&str>) -> HashMap<FunctionId, bool> {
        let mut all_seen = HashMap::default();
        for id in self.functions.keys() {
            let in_variable = self.assigned_to.get(id).map(|&b| {
                let binding = &self.bindings[b];
                binding.types.function == FunctionRef::One(*id) && only_called[b]
            });
            let in_fields = self.stored_in_fields.get(id).map(|names| {
                !escaped.contains(id) && names.iter().all(|name| !unresolved.contains(name.as_str()))
            });
            let all_calls_seen = !escaped.contains(id) && match (in_variable, in_fields) {
                (None, None) => self.pool_callbacks.contains(id),
                (variable, fields) => variable.unwrap_or(true) && fields.unwrap_or(true),
            };
            all_seen.insert(*id, all_calls_seen);
        }
        all_seen
    }

    /// Each method given as a script handler (as `frame:SetScript("OnShow",
    /// t.m)` gives it) that no call in the files is seen passing a `self`:
    /// the frames the game passes it, rather than the table it's defined
    /// on, which it's never seen called on. Otherwise, even read as a value
    /// for something else (`__call`, or kept to call later), the table it's
    /// defined on is still the best guess
    pub(super) fn method_selves(&self) -> HashMap<FunctionId, Type> {
        self.self_passed.iter()
            .filter(|(_, (by_call, by_game))| by_call.set == TypeSet::NEVER && by_game.set != TypeSet::NEVER)
            .map(|(&id, &(_, by_game))| (id, by_game))
            .collect()
    }

    pub(super) fn settle_parameter_types(&mut self) {
        let mut only_called = vec![true; self.bindings.len()];
        for reference in &self.references {
            if !reference.write && !reference.callee {
                only_called[reference.binding] = false;
            }
        }
        // Functions read from a field other than to call them, and field names
        // read where it isn't known which function they hold
        let mut escaped: HashSet<FunctionId> = self.escaped_functions.clone();
        // A variable read other than to call it: the functions it may hold
        // go wherever it does. (The variable a function was assigned to is
        // accounted for by `only_called`)
        for reference in self.references.iter().filter(|r| !r.write && !r.callee) {
            for function in reference.types.function.alternatives() {
                if let FunctionRef::One(id) = function
                    && self.assigned_to.get(&id) != Some(&reference.binding) {
                    escaped.insert(id);
                }
            }
        }
        let mut unresolved = HashSet::default();
        // Methods `ForwardedCall`s stand for: resolved by each call to the
        // function calling them, if every one is seen
        let mut forwarded: Vec<(&str, FunctionId)> = Vec::new();
        for field in self.field_references.iter().filter(|f| !f.write && !f.tested) {
            match (field.types.function, field.forwarded) {
                (FunctionRef::One(function), _) if !field.callee => { escaped.insert(function); }
                // Each of several it may be: called, each gets what's
                // passed, but read otherwise, any could be called anywhere
                (FunctionRef::Several(_), forwarded_by) => {
                    if !field.callee {
                        escaped.extend(field.types.function.alternatives().into_iter().filter_map(|f| match f {
                            FunctionRef::One(id) => Some(id),
                            _ => None,
                        }));
                    }
                    if let Some(caller) = forwarded_by {
                        forwarded.push((field.name(), caller));
                    }
                }
                (FunctionRef::Unknown, Some(caller)) => forwarded.push((field.name(), caller)),
                (FunctionRef::Unknown, None) => { unresolved.insert(field.name()); }
                _ => {}
            }
        }
        // Which functions have every call seen depends on which field names
        // are read unresolved, which in turn depends on that: until it
        // settles, which it does, as it only ever loses functions
        let mut all_seen;
        loop {
            all_seen = self.all_calls_seen(&only_called, &escaped, &unresolved);
            let before = unresolved.len();
            for &(name, caller) in &forwarded {
                if !all_seen.get(&caller).copied().unwrap_or(false) {
                    unresolved.insert(name);
                }
            }
            if unresolved.len() == before {
                break
            }
        }
        let definitions = &self.definitions;
        // A table or object made in the files that meets the requirement is
        // kept, so what it holds and its fields aren't lost to a plain `table`
        // Any of several objects meets it when each does
        let meets = |passed: Type, mandated: Type| {
            let objects = match passed.named {
                NamedRef::Several(_) => passed.named.alternatives(),
                // Any of several tables made in the files, each of which meets it
                NamedRef::Tables(_) => passed.named.identities().into_iter().map(NamedRef::Table).collect(),
                named => vec![named],
            };
            mandated.set.contains(passed.set) && objects.iter().all(|object| {
                object.identity().is_some()
                    && mandated.named.named_id().is_none_or(|m| object.named_id().is_some_and(|p| definitions.is_a(p, m)))
            })
        };
        let mut pending: HashMap<FunctionId, Vec<&PendingCall>> = HashMap::default();
        // The functions each function's parameters are passed on to
        let mut dependents: HashMap<FunctionId, Vec<FunctionId>> = HashMap::default();
        for call in &self.pending_calls {
            pending.entry(call.callee).or_default().push(call);
            let sources = call.parameters.iter().map(|&(_, function, _, _)| function).chain(call.varargs.map(|(_, function)| function));
            for source in sources {
                let callees = dependents.entry(source).or_default();
                if !callees.contains(&call.callee) {
                    callees.push(call.callee);
                }
            }
        }
        // Calls passing a parameter on take it as settled here, so parameters
        // and `...` start from nothing known and grow until they agree. They
        // only grow, and only so far, so this ends. A worklist keeps it to
        // the functions something new is passed to
        let mut ids: Vec<FunctionId> = self.functions.keys().copied().collect();
        ids.sort();
        let mut parameters: HashMap<FunctionId, Vec<Type>> = ids.iter()
            .map(|id| (*id, vec![Type::default(); self.functions[id].parameters.len()]))
            .collect();
        // Until walks settle, which calls are seen grows as callees are found
        // (finding a method reads a field, which only the previous walk
        // knows), so a function is taken to have the calls seen so far: a
        // parameter with none is nothing yet. Taking it to be unknown as soon
        // as a call might not be seen would put unknown in what's passed
        // round a loop of calls (a pool's frames, released and acquired
        // again), which it would never be rid of. Once walks settle, a
        // function that may be called where it isn't seen takes anything
        // (see `Walker::settled`)
        let trusted: HashMap<FunctionId, bool> = ids.iter().map(|id| (*id, all_seen[id] || !self.settled)).collect();
        // A function that may be called where it isn't seen, taking only the
        // calls seen, is provisional too, as are parameters taking nothing
        // below
        if !self.settled && ids.iter().any(|id| !all_seen[id] && (!self.functions[id].parameters.is_empty() || self.functions[id].varargs.is_some())) {
            self.provisional.set(true);
        }
        let open = if self.settled { Type::UNKNOWN } else { Type::default() };
        let mut varargs: HashMap<FunctionId, Values> = ids.iter()
            .map(|id| (*id, match self.functions[id].varargs.is_some() && trusted[id] {
                true => Values { values: Vec::new(), rest: Type::default() },
                false => Values { values: Vec::new(), rest: open },
            }))
            .collect();
        // Each function once, then again whenever what's passed on to it grows
        let mut queue: std::collections::VecDeque<FunctionId> = ids.iter().copied().collect();
        let mut queued: HashSet<FunctionId> = ids.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            queued.remove(&id);
            let id = &id;
            let mut changed = false;
            {
                let function = &self.functions[id];
                let mut passed = self.call_arguments.get(id).cloned();
                for call in pending.get(id).into_iter().flatten() {
                    let resolved = resolve_pending(call, &parameters, &varargs);
                    passed = Some(match passed {
                        Some(passed) => passed.union(&resolved),
                        None => resolved,
                    });
                }
                for (i, parameter) in function.parameters.iter().enumerate() {
                    let ty = match (parameter.mandated, &passed) {
                        // What the calls seen pass, as any parameter takes
                        // until walks settle, when it meets the requirement
                        (Some(mandated), Some(passed)) if trusted[id] && meets(passed.get(i), mandated) => passed.get(i),
                        // Some of what's passed meets it, the rest (a number,
                        // say) fails the body: what meets it. Not the
                        // requirement, a table of unknown origin, whose
                        // contents would be unknown, and read less than what's
                        // known of these tables, so a function passing what
                        // it reads of them back to itself would swap between
                        // the two each walk
                        (Some(mandated), Some(passed)) if trusted[id] && {
                            let narrowed = passed.get(i).without(TypeSet::UNKNOWN.without(mandated.set));
                            narrowed.set != TypeSet::NEVER && meets(narrowed, mandated)
                        } => passed.get(i).without(TypeSet::UNKNOWN.without(mandated.set)),
                        // Nothing passed yet: nothing yet, as the requirement
                        // (a plain `table`) would stand for a table of unknown
                        // origin, which may hold anything, round a loop that's
                        // never rid of it (see `Walker::settled`)
                        (Some(_), passed) if !self.settled && passed.as_ref().is_none_or(|p| p.get(i).set == TypeSet::NEVER) => {
                            self.provisional.set(true);
                            Type::default()
                        }
                        (Some(mandated), _) => mandated,
                        (None, Some(passed)) if trusted[id] => passed.get(i),
                        _ => {
                            if !self.settled {
                                self.provisional.set(true);
                            }
                            open
                        }
                    };
                    let slot = &mut parameters.get_mut(id).expect("every function")[i];
                    if *slot | ty != *slot {
                        *slot |= ty;
                        changed = true;
                    }
                }
                if let (true, true, Some(passed)) = (function.varargs.is_some(), trusted[id], &passed) {
                    let values = Values {
                        values: passed.values.iter().skip(function.parameters.len()).copied().collect(),
                        rest: passed.rest,
                    };
                    let slot = varargs.get_mut(id).expect("every function");
                    let grown = slot.union(&values);
                    if grown != *slot {
                        *slot = grown;
                        changed = true;
                    }
                }
            }
            if changed {
                for &dependent in dependents.get(id).into_iter().flatten() {
                    // A dependency's function: only that addon settles its parameters
                    if !self.functions.contains_key(&dependent) {
                        continue
                    }
                    if queued.insert(dependent) {
                        queue.push_back(dependent);
                    }
                }
            }
        }
        // Kept as their types: more tables than are kept apart are a stand-in
        let mut kept: HashMap<FunctionId, Vec<Type>> = HashMap::default();
        for (id, types) in &parameters {
            // Passed a variable's stand-in for a table it can't identify that's
            // declared in the function itself, passed back to it (as
            // `Degroup(entry)` in `Degroup`): which of its variables it was
            // would swap from walk to walk, as what each holds depends on the
            // parameter. The parameter's own stand-in, standing for them all,
            // is the same every walk and has what each of them has
            let end = self.function_ends.get(id).copied();
            let inner = |at: &TableId| at.slot == PSEUDO && at.file == id.file
                && end.is_some_and(|end| at.offset >= id.offset && at.offset < end)
                && !self.parameter_pseudos.contains(at);
            for (i, ty) in types.iter().enumerate() {
                if ty.named.identities().iter().any(inner) {
                    self.stand_ins.get_mut().entry(stand_in(Place::Parameter(*id, i))).or_default();
                }
            }
            // In a loop through a field: with what it had the walk before
            // too (see `Analysis::field_loops`)
            let prior = self.prior;
            let types = types.iter().enumerate().map(|(i, &ty)| {
                let before = prior.filter(|prior| prior.field_loops.contains(&(*id, i)))
                    .and_then(|prior| prior.functions.get(id)?.parameters.get(i))
                    .map(|p| p.types);
                let ty = before.map_or(ty, |before| ty | before);
                kept_at(self.stand_ins.get_mut(), Place::Parameter(*id, i), ty).0
            }).collect();
            kept.insert(*id, types);
        }
        let parameters = kept;
        for (id, function) in self.functions.iter_mut() {
            for (parameter, &ty) in function.parameters.iter_mut().zip(&parameters[id]) {
                parameter.types = ty;
            }
            let has_calls = self.call_arguments.contains_key(id) || pending.contains_key(id);
            if let (Some(shown), true, true) = (&mut function.varargs, trusted[id], has_calls) {
                let values = varargs[id].clone();
                // For the signature: anything one of them can be. Some calls
                // passing fewer isn't a nil worth showing
                let mut any = values.values.iter().fold(Type::default(), |all, &v| all | v.without(TypeSet::NIL));
                if values.rest != Type::NIL {
                    any |= values.rest.without(TypeSet::NIL);
                }
                // Not knowing them at all is just unknown, not `unknown (not nil)`
                let unknown = any == Type::default() || any.set == TypeSet::UNKNOWN.without(TypeSet::NIL);
                shown.types = if unknown { Type::UNKNOWN } else { any };
                self.vararg_values.insert(*id, values);
            }
        }
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot {
            bindings: self.bindings.len(),
            references: self.references.len(),
            field_references: self.field_references.len(),
            diagnostics: self.diagnostics.len(),
            deferred: self.deferred.len(),
            globals_created: self.globals_created.len(),
            pending_calls: self.pending_calls.len(),
            function_count: self.function_count,
        }
    }

    /// Forgets the bindings and references recorded since the snapshot. Changes
    /// to older bindings are kept: their current types are what's being carried
    /// round the loop, and their all-assignment unions only grow pass to pass
    pub(super) fn restore(&mut self, snapshot: &Snapshot) {
        self.bindings.truncate(snapshot.bindings);
        self.current.truncate(snapshot.bindings);
        self.references.truncate(snapshot.references);
        self.field_references.truncate(snapshot.field_references);
        self.diagnostics.truncate(snapshot.diagnostics);
        self.deferred.truncate(snapshot.deferred);
        self.pending_calls.truncate(snapshot.pending_calls);
        self.function_count = snapshot.function_count;
        for name in self.globals_created.drain(snapshot.globals_created..) {
            self.globals.remove(&name);
        }
    }

    /// Walks a loop body (including any condition checked each iteration)
    /// repeatedly, feeding the types at the end back in at the start, until they
    /// stop changing. Assignments inside a loop only ever widen the types of
    /// variables from outside it, so this converges. Only the final pass's
    /// bindings and references are kept.
    pub(super) fn loop_body(&mut self, mut body: impl FnMut(&mut Walker)) {
        // A pass may change fields the next one reads
        self.forget_fields();
        for pass in 1.. {
            let snapshot = self.snapshot();
            let log_start = self.loop_log.len();
            self.loop_depth += 1;
            body(self);
            self.loop_depth -= 1;
            // Each older binding the pass set, with its type on entry: the
            // first logged. Those it ends without changing don't count
            let mut seen = HashSet::default();
            let changed: Vec<BindingId> = self.loop_log[log_start..].iter()
                .filter(|&&(id, _)| id < snapshot.bindings && seen.insert(id))
                .filter(|&&(id, entry)| self.current[id] != entry)
                .map(|&(id, _)| id)
                .collect();
            // Kept for the loops around this one, which compare what they changed
            if self.loop_depth == 0 {
                self.loop_log.clear();
            }
            if changed.is_empty() {
                return
            }
            if pass >= LOOP_PASSES_BEFORE_WIDENING {
                // Each further pass makes at least one more variable unknown
                for id in changed {
                    self.set_current(id, Type::UNKNOWN);
                }
            }
            self.restore(&snapshot);
            self.forget_fields();
        }
    }

    /// Sets a binding's current type, noting the type before when in a loop
    pub(super) fn set_current(&mut self, id: BindingId, types: Type) {
        let types = self.stored(self.binding_place(id), types, true);
        if let Some(body) = self.deps.get_mut().last_mut()
            && id >= body.bindings_start && self.bindings[id].kind != BindingKind::Global
            && let Some(level) = body.levels.last_mut() {
            level.bindings.push(id);
        }
        if self.loop_depth > 0 && self.current[id] != types {
            self.loop_log.push((id, self.current[id]));
        }
        // What it held before the `if` being walked changed it
        if let Some(join) = self.joins.last_mut()
            && id < join.bindings && !join.before.contains_key(&id) {
            join.before.insert(id, (self.current[id], join.fell_through));
        }
        self.current[id] = types;
    }

    pub(super) fn push_scope(&mut self, conditional: bool, end: TextSize) {
        self.scopes.push(Scope { names: HashMap::default(), conditional, joined: false, end });
    }

    pub(super) fn pop_scope(&mut self) {
        self.scopes.pop();
    }
}
