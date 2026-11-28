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

//! Running an analysis: walking the files until what each walk reads from
//! the one before stops changing

use super::*;

/// A file to analyse
pub struct SourceFile {
    pub root: SyntaxNode,
    /// The addon the file is part of, by its place in load order, so `...` at
    /// its top is that addon's name and namespace. Addons analysed together
    /// (one and those it depends on) each have their own namespace
    pub addon: Option<u32>,
    /// For a script handler written inline in XML, which the game makes a
    /// function of: what it's called with
    pub handler: Option<Handler>,
}

/// What an inline XML script handler is called with: `self`, the object the
/// XML makes, and the arguments the game passes that kind of handler, by the
/// names it gives them
#[derive(Debug, Clone)]
pub struct Handler {
    pub self_type: Type,
    /// Each with the type the game passes
    pub parameters: Vec<(String, Type)>,
}

#[derive(Debug, Default)]
pub struct Analysis {
    /// For a method (`function t:m()`) only seen given as a script
    /// handler: what its `self` is, rather than `t` (see
    /// `Walker::method_selves`)
    pub(super) method_selves: HashMap<FunctionId, Type>,
    pub bindings: Vec<Binding>,
    /// Sorted by file, then position
    pub references: Vec<Reference>,
    /// Sorted by file, then position
    pub field_references: Vec<FieldReference>,
    pub functions: HashMap<FunctionId, FunctionInfo>,
    pub definitions: Arc<Definitions>,
    /// Every value assigned to each field of each table made in the file
    pub fields: HashMap<TableId, HashMap<String, Type>>,
    /// Values assigned to fields of tables whose identity wasn't known, by name
    pub any_table_fields: HashMap<String, Type>,
    /// Data types (see `Definitions::has_fixed_fields`) a value of which was
    /// given fields the analysis can't name: under a computed key, by
    /// `rawset`, from a mixin or through a metatable
    pub opened_types: HashSet<NamedId>,
    /// What each table made in the files holds beyond its named fields
    pub entries: HashMap<TableId, Entries>,
    /// Each table's metatable, from `setmetatable`
    pub metatables: HashMap<TableId, Type>,
    /// What `...` is in each function taking it, when every call is seen: the
    /// arguments past its named parameters
    pub vararg_values: HashMap<FunctionId, Values>,
    /// For each function in a cycle of calls (one calling itself, or calling
    /// one that comes back round to it), the cycle's first function: a call
    /// between two with the same is recursive. Found from what each function
    /// calls once a walk is done, for the next walk
    pub call_cycles: HashMap<FunctionId, FunctionId>,
    /// Parameters what they're passed comes back to through a field, as
    /// `ShowGuild(rowWidth)` writes `self.oldRowWidth = rowWidth` and
    /// `OnEvent` calls `ShowGuild(self.oldRowWidth)`. A field read sees the
    /// walk before's writes, and a parameter settles once a walk is done, so
    /// the value takes two walks to come round: what odd walks have and what
    /// even walks have never meet, and something only one of them got stays
    /// in it, swapping each walk. Each keeps what it had the walk before as
    /// well, so they meet. Found from each function's `loop_edges`
    pub field_loops: HashSet<(FunctionId, usize)>,
    /// The tables in a group whose entries each come from the others'
    /// (see `Analysis::find_entry_loops`)
    pub(super) entry_loops: HashSet<TableId>,
    /// How many walks it took to settle
    pub walks: usize,
    /// Whether walks came round to a state seen before, so they only
    /// settled by keeping what each had (see `combining`)
    pub cycled: bool,
    /// Each file's tree, for what field references point to. The green tree,
    /// which can go to another thread, unlike a `SyntaxNode`
    pub(super) roots: Vec<rowan::GreenNode>,
    /// What the addons this one depends on make visible to it
    pub layer: Arc<Layer>,
    /// References that read an upvalue, given the binding's final type by
    /// `complete`
    pub(super) deferred: Vec<usize>,
    /// Calls passing a parameter on, for `Record`s to point into
    pub(super) pending_calls: Vec<PendingCall>,
    /// What walking each function body did, for the next walk to replay
    pub(super) records: HashMap<FunctionId, Arc<Record>>,
    /// Function bodies replayed rather than walked, over every walk, and
    /// walked
    pub replayed: usize,
    pub walked: usize,
    /// Every field name any table in the files was given a function under
    pub function_fields: HashSet<String>,
    /// The parent of each WoW object made in the files: what it was made with
    /// (`CreateFrame`'s parent, or the receiver of `CreateTexture` and the like)
    /// and anything passed to its `SetParent`
    pub parents: HashMap<TableId, Type>,
    /// For each callback registry made in the files (a table with
    /// `CallbackRegistryMixin`) and event, by its name's hash, what its
    /// `TriggerEvent` calls pass after the event's name
    pub triggered: HashMap<(TableId, u64), Values>,
    /// The tables each table or object made in the files had mixed into it (by
    /// `Mixin` or `CreateFromMixins`), whose fields it has too
    pub mixins: HashMap<TableId, Vec<Type>>,
    /// For each table mixed into WoW objects (directly, or through tables made
    /// from it with `CreateFromMixins`), the one type those objects all are,
    /// and whether they may have members beyond that type's. What `self` is in
    /// the table's methods
    pub mixed_into: HashMap<TableId, (NamedId, bool)>,
    /// The tables each mixin was mixed into, as `CreateFromMixins(M)` makes
    /// one: an object `M` reaches through one of them has its fields too,
    /// so `self` in `M`'s methods may have them
    pub mixin_copies: HashMap<TableId, Vec<TableId>>,
    /// Whether the walk took something not known yet to be nothing (see
    /// `Walker::provisional`)
    pub provisional: bool,
    /// The table each table `CopyTable` made is a copy of
    pub copies: HashMap<TableId, TableId>,
    /// The name each table made in the files was first stored under (a
    /// variable or a field path like `ns.ButtonMixin`), to show its mixins by
    pub table_names: HashMap<TableId, String>,
    /// What `Acquire` gives for each pool made in the files (by
    /// `CreateFramePool` and the like): one object standing for all of them,
    /// whose fields and mixins come from the pool's init and reset functions
    pub pool_objects: HashMap<TableId, Type>,
    /// The value of each global that XML names as a mixin wow_ls doesn't know
    /// (see `Definitions::xml_mixins`), as the files or those they depend on
    /// assign it
    pub lua_mixins: HashMap<String, Type>,
    /// The tables the game saves between sessions (see `tables_saved_in`)
    pub(super) saved_tables: HashSet<TableId>,
    /// For each stand-in, the tables it stands for: those ever kept at its
    /// place, only growing (see `Walker::stored`)
    pub stand_ins: HashMap<TableId, Vec<TableId>>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// One file on its own
    pub fn analyse_file(root: &SyntaxNode, definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse(&[SourceFile { root: root.clone(), addon: None, handler: None }], definitions)
    }

    /// Files in the order they load, as one program. The `FileId`s in the
    /// result are positions in `files`
    pub fn analyse(files: &[SourceFile], definitions: &Arc<Definitions>) -> Analysis {
        Analysis::analyse_over(files, definitions, Arc::default(), &[])
    }

    /// `analyse`, for files loaded after the addons `layer` comes from
    /// `saved` names the globals the game saves between sessions
    pub fn analyse_over(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>, saved: &[String]) -> Analysis {
        // Replaying is only ever faster; turning it off is for checking it
        let replay = std::env::var_os("WOW_LS_NO_REPLAY").is_none();
        Analysis::analyse_replaying(files, definitions, layer, saved, replay)
    }

    /// `analyse`, replaying function bodies whose inputs haven't changed
    /// since the walk before, or walking every one every time
    pub(super) fn analyse_replaying(files: &[SourceFile], definitions: &Arc<Definitions>, layer: Arc<Layer>, saved: &[String], replay: bool) -> Analysis {
        let _analysing = Analysing::with(definitions);
        let mut result = Walker::new(None, definitions, &layer, Default::default(), replay).walk(files);
        result.saved_tables = result.tables_saved_in(saved);
        // Nothing to compare the first walk with, but no body it recorded can
        // be replayed anyway, as it walked without a previous walk
        let mut changed = HashSet::default();
        let mut seen = HashSet::from_iter([result.fingerprint()]);
        // Whether walks have started to cycle, so each is combined with the
        // one before
        let mut combining = false;
        let mut cycled = false;
        // Whether walks have settled once, and go on with what tables nothing's
        // been seen stored in hold unknown (see `Walker::settled`)
        let mut unstored_unknown = false;
        for walks in 2.. {
            let mut walker = Walker::new(Some(&result), definitions, &layer, changed, replay);
            walker.settled = unstored_unknown;
            let mut next = walker.walk(files);
            next.walks = walks;
            next.replayed += result.replayed;
            next.walked += result.walked;
            if combining {
                next.combine_inputs(&result);
                next.find_call_cycles();
                next.find_field_loops();
                next.find_entry_loops();
            }
            // Once found, still saved: leaving out a write can take away the
            // field a table was found through, and keeping it again would put
            // it back, forever
            next.saved_tables = next.tables_saved_in(saved);
            next.saved_tables.extend(result.saved_tables.iter().copied());
            changed = next.changed_reads(&result);
            // Nothing the next walk would read has changed
            let settled = changed.is_empty();
            // Walks are deterministic, so a state coming round again would
            // cycle forever (a field and parameter feeding each other can
            // swap between two types). From then on each walk keeps what
            // the one before had too: both sides of the cycle, which only
            // grows, so it settles
            let repeat = !seen.insert(next.fingerprint());
            if ((repeat && !combining) || std::env::var_os("SHOW_EVERY_WALK").is_some()) && !settled && std::env::var_os("SHOW_COMBINING").is_some() {
                eprintln!("COMBINING{} starts at walk {walks}", if repeat && !combining { "" } else { " (every walk)" }); eprintln!("  at walk {walks} (phase {}), {} keys changed:", if unstored_unknown { 2 } else { 1 }, changed.len());
                let at = |l: &Location| -> String {
                    let text = files.get(l.file.0 as usize).map(|f| f.root.text().to_string()).unwrap_or_default();
                    let o = usize::from(l.offset).min(text.len());
                    let line = text[..o].matches('\n').count() + 1;
                    let start = text[..o].rfind('\n').map_or(0, |i| i + 1);
                    let end = text[o..].find('\n').map_or(text.len(), |i| o + i);
                    format!("f{}:{} slot {:x} `{}`", l.file.0, line, l.slot, text[start..end].trim())
                };
                let name_of = |h: u64| -> String {
                    result.fields.values().flat_map(|m| m.keys()).chain(result.any_table_fields.keys()).chain(result.function_fields.iter())
                        .find(|n| name_key(n) == h).cloned()
                        .or_else(|| next.fields.values().flat_map(|m| m.keys()).find(|n| name_key(n) == h).cloned())
                        .or_else(|| next.field_references.iter().map(|f| f.name()).find(|n| name_key(n) == h).map(str::to_string))
                        .unwrap_or_else(|| format!("#{h:x}"))
                };
                let describe = |r: &Read| -> String { match r {
                    Read::Binding(id) => { let b = &next.bindings[*id]; format!("Binding {} {:?} f{} decl {:?}: {} -> {}  [{:?} -> {:?}]", b.name, b.kind, b.file.0, b.declaration.map(|d| at(&Location { file: b.file, offset: d.start(), slot: 0 })), result.bindings.get(*id).map(|o| next.describe(o.types)).unwrap_or_default(), next.describe(b.types), result.bindings.get(*id).map(|o| o.types), b.types) }
                    Read::Field(l, h) => { let n = name_of(*h); let o = result.fields.get(l).and_then(|m| m.get(&n)).copied(); let v = next.fields.get(l).and_then(|m| m.get(&n)).copied(); format!("Field {} of {}: {} -> {}  [{:?} -> {:?}]", n, at(l), o.map(|t| next.describe(t)).unwrap_or_default(), v.map(|t| next.describe(t)).unwrap_or_default(), o, v) }
                    Read::AnyTableField(h) => format!("AnyTableField {}", name_of(*h)),
                    Read::FunctionField(h) => format!("FunctionField {}", name_of(*h)),
                    Read::Fields(l) => format!("Fields of {}", at(l)),
                    Read::Entries(l) => format!("Entries of {}", at(l)),
                    Read::Parent(l) => format!("Parent of {}", at(l)),
                    Read::Function(l) => format!("Function {}", at(l)),
                    Read::ParameterTypes(l) => format!("ParameterTypes {}", at(l)),
                    other => format!("{other:?}"),
                } };
                let mut keys: Vec<String> = changed.iter().map(describe).collect();
                keys.sort();
                for k in keys.iter().take(40) { eprintln!("    {k}"); }
            }
            combining |= repeat;
            cycled |= repeat && !settled;
            result = next;
            if !settled {
                continue
            }
            if unstored_unknown {
                break
            }
            // What isn't known yet is unknown from here on, until walks
            // settle again (see `Walker::settled`). Starting from where they
            // settled, a table filled from itself already holds what it's
            // given, and a function called round a loop is passed it. Bodies
            // reading a table nothing's been seen stored in, or a field
            // written nowhere seen, are walked again;
            // parameters change at the end of this walk, which the next sees
            let written: HashSet<u64> = result.any_table_fields.keys().chain(layer.any_table_fields.keys()).map(|name| name_key(name)).collect();
            changed = result.records.values()
                .flat_map(|record| record.reads.iter())
                .filter(|read| match read {
                    Read::Entries(at) => !result.entries.contains_key(at) && !layer.entries.contains_key(at),
                    Read::AnyTableField(name) => !written.contains(name),
                    _ => false,
                })
                .cloned()
                .collect();
            if changed.is_empty() && !result.provisional {
                break
            }
            unstored_unknown = true;
            seen.clear();
            seen.insert(result.fingerprint());
            combining = false;
        }
        result.cycled = cycled;
        result.complete();
        result
    }

    /// What only the result shows, not needed between walks: done once, for
    /// the last walk
    pub(super) fn complete(&mut self) {
        // Only the next walk needed them, unless they're asked for
        if !KEEP_RECORDS.with(|keep| keep.get()) {
            self.records = HashMap::default();
            self.pending_calls = Vec::new();
        }
        for &index in &self.deferred {
            let binding = self.references[index].binding;
            self.references[index].types = self.bindings[binding].types;
        }
        self.references.sort_by_key(|r| (r.file, r.range.start()));
        self.field_references.sort_by_key(|r| (r.file, r.range.start()));
        self.diagnostics.sort_by_key(|d| (d.file, d.range.start()));
        // References are sorted, so the first write of each table wins.
        // Not parameters, whose types come from what calls pass them
        // Only writes of a table can name one, and paths are slow to find
        let is_table = |types: Type| matches!(types.named, NamedRef::Table(_));
        let variables = self.references.iter()
            .filter(|r| r.write && is_table(r.types) && self.bindings[r.binding].kind != BindingKind::Parameter)
            .map(|r| (r.file, r.range.start(), r.types, self.bindings[r.binding].name.to_string()));
        let roots = self.syntax_roots();
        let fields = self.field_references.iter().filter(|f| f.write && is_table(f.types))
            .map(|f| (f.file, f.range.start(), f.types, f.path(roots.get(f.file.0 as usize))));
        let mut named: Vec<_> = variables.chain(fields).collect();
        named.sort_by_key(|&(file, start, ..)| (file, start));
        for (_, _, types, name) in named {
            if let NamedRef::Table(at) = types.named {
                self.table_names.entry(at).or_insert(name);
            }
        }
    }

    /// The tables the globals named `saved` hold, and those stored in them,
    /// however deep: what the game saves between sessions
    pub(super) fn tables_saved_in(&self, saved: &[String]) -> HashSet<TableId> {
        let mut found = HashSet::default();
        let mut pending: Vec<TableId> = self.bindings.iter()
            .filter(|b| b.kind == BindingKind::Global && saved.iter().any(|name| **name == *b.name))
            .flat_map(|b| b.types.named.identities())
            .collect();
        while let Some(at) = pending.pop() {
            if !found.insert(at) {
                continue
            }
            let fields = self.fields.get(&at).into_iter().flat_map(|fields| fields.values());
            let entries = self.entries.get(&at).map(|entries| entries.values);
            for value in fields.copied().chain(entries) {
                pending.extend(value.named.identities());
            }
        }
        found
    }

    /// Finds `field_loops` from each function's `loop_edges`: the parameters
    /// in a group of parameters and fields that each reach the others, with
    /// a field among them
    pub(super) fn find_field_loops(&mut self) {
        #[derive(Clone, PartialEq, Eq, Hash)]
        enum Node { Parameter(FunctionId, usize), Field(Option<TableId>, String) }
        let mut nodes: Vec<Node> = Vec::new();
        let mut index: HashMap<Node, usize> = HashMap::default();
        let mut edges: Vec<Vec<usize>> = Vec::new();
        let mut node = |n: Node, nodes: &mut Vec<Node>, edges: &mut Vec<Vec<usize>>| -> usize {
            *index.entry(n.clone()).or_insert_with(|| { nodes.push(n); edges.push(Vec::new()); nodes.len() - 1 })
        };
        // The fields a field on `on` is: on each table, each a stand-in
        // stands for, and each mixin, which one read through it reads
        let fields = |on: &[TableId], name: &str| -> Vec<Node> {
            if on.is_empty() {
                return vec![Node::Field(None, name.to_string())]
            }
            let mut tables: Vec<TableId> = Vec::new();
            let mut pending: Vec<TableId> = on.to_vec();
            while let Some(at) = pending.pop() {
                // Deep enough for any real chain of mixins
                if tables.contains(&at) || tables.len() > 64 {
                    continue
                }
                tables.push(at);
                pending.extend(self.stand_in_members(at));
                pending.extend(self.mixins.get(&at).into_iter().flatten().filter_map(|mixin| mixin.named.identity()));
            }
            tables.sort();
            // A variable's stand-in for a table it can't identify: a read
            // through it is any table's field of that name, as for a table
            // that can't be identified at all (see `field_read`)
            let any = tables.iter().any(|at| at.slot & PSEUDO != 0).then(|| Node::Field(None, name.to_string()));
            tables.into_iter().map(|at| Node::Field(Some(at), name.to_string())).chain(any).collect()
        };
        let mut ids: Vec<&FunctionId> = self.functions.keys().collect();
        ids.sort();
        for &id in &ids {
            for edge in &self.functions[id].loop_edges {
                let steps: Vec<(Node, Node)> = match edge {
                    LoopEdge::ParameterToField(p, on, field) => fields(on, field).into_iter()
                        .map(|f| (Node::Parameter(*id, *p), f))
                        .collect(),
                    LoopEdge::ReturnToField(callee, on, field) => self.functions.get(callee).into_iter()
                        .flat_map(|f| f.returned_parameters.iter())
                        .flat_map(|&p| fields(on, field).into_iter().map(move |f| (Node::Parameter(*callee, p), f)))
                        .collect(),
                    LoopEdge::FieldToParameter(on, field, callee, i) => fields(on, field).into_iter()
                        .map(|f| (f, Node::Parameter(*callee, *i)))
                        .collect(),
                    LoopEdge::ParameterToParameter(p, callee, i) => vec![(Node::Parameter(*id, *p), Node::Parameter(*callee, *i))],
                };
                for (from, to) in steps {
                    let (from, to) = (node(from, &mut nodes, &mut edges), node(to, &mut nodes, &mut edges));
                    edges[from].push(to);
                }
            }
        }
        let groups = strongly_connected(&edges);
        let mut sizes: HashMap<usize, usize> = HashMap::default();
        let mut with_field: HashSet<usize> = HashSet::default();
        for (n, &group) in groups.iter().enumerate() {
            *sizes.entry(group).or_default() += 1;
            if matches!(nodes[n], Node::Field(..)) {
                with_field.insert(group);
            }
        }
        self.field_loops = nodes.iter().zip(&groups)
            .filter(|&(_, group)| sizes[group] > 1 && with_field.contains(group))
            .filter_map(|(n, _)| match n { Node::Parameter(id, i) => Some((*id, *i)), Node::Field(..) => None })
            .collect();
    }

    /// Finds `entry_loops` from what each body wrote under computed keys and
    /// what that depended on: the tables in a group that each reach the
    /// others, as a pool's tables of free and used objects do (`Free[k] =
    /// InUse[k]`, `InUse[k] = next(Free)`). Each read sees the walk before,
    /// so a value takes a walk per step to come round, and a loop of two
    /// tables could keep what odd and even walks see apart for ever
    pub(super) fn find_entry_loops(&mut self) {
        let mut nodes: Vec<TableId> = Vec::new();
        let mut index: HashMap<TableId, usize> = HashMap::default();
        let mut edges: Vec<Vec<usize>> = Vec::new();
        let mut node = |at: TableId, nodes: &mut Vec<TableId>, edges: &mut Vec<Vec<usize>>| -> usize {
            *index.entry(at).or_insert_with(|| { nodes.push(at); edges.push(Vec::new()); nodes.len() - 1 })
        };
        let mut ids: Vec<&FunctionId> = self.records.keys().collect();
        ids.sort();
        for id in ids {
            let record = &self.records[id];
            // Only a body that both reads entries and writes some can be a step
            let writes = record.own_writes.iter().any(|w| matches!(**w, Write::Entry(..)));
            if !writes || !record.reads.iter().any(|r| matches!(r, Read::Entries(_))) {
                continue
            }
            for (key, deps) in record.own_influences.get() {
                let Read::Entries(to) = key else { continue };
                for dep in deps.iter() {
                    if let Read::Entries(from) = dep {
                        if from != to {
                            let (from, to) = (node(*from, &mut nodes, &mut edges), node(*to, &mut nodes, &mut edges));
                            edges[from].push(to);
                        }
                    }
                }
            }
        }
        let groups = strongly_connected(&edges);
        let mut sizes: HashMap<usize, usize> = HashMap::default();
        for &group in &groups {
            *sizes.entry(group).or_default() += 1;
        }
        self.entry_loops = nodes.iter().zip(&groups)
            .filter(|&(_, group)| sizes[group] > 1)
            .map(|(&at, _)| at)
            .collect();
    }

    /// Finds `call_cycles` from what each function calls: the groups of
    /// functions that can each come round to calling the others
    pub(super) fn find_call_cycles(&mut self) {
        let mut ids: Vec<FunctionId> = self.functions.keys().copied().collect();
        ids.sort();
        let index: HashMap<FunctionId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let calls: Vec<Vec<usize>> = ids.iter()
            .map(|id| self.functions[id].calls.iter().filter_map(|callee| index.get(callee).copied()).collect())
            .collect();
        let mut called_by: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
        for (caller, callees) in calls.iter().enumerate() {
            for &callee in callees {
                called_by[callee].push(caller);
            }
        }
        // Kosaraju: the order calls finish in, then the groups reached
        // backwards from the last to finish
        let mut order = Vec::with_capacity(ids.len());
        let mut visited = vec![false; ids.len()];
        for start in 0..ids.len() {
            if visited[start] {
                continue
            }
            visited[start] = true;
            let mut stack = vec![(start, 0)];
            while let Some((at, next)) = stack.pop() {
                if let Some(&callee) = calls[at].get(next) {
                    stack.push((at, next + 1));
                    if !visited[callee] {
                        visited[callee] = true;
                        stack.push((callee, 0));
                    }
                } else {
                    order.push(at);
                }
            }
        }
        let mut group = vec![usize::MAX; ids.len()];
        let mut cycles = HashMap::default();
        for &start in order.iter().rev() {
            if group[start] != usize::MAX {
                continue
            }
            group[start] = start;
            let mut members = vec![start];
            let mut pending = vec![start];
            while let Some(at) = pending.pop() {
                for &caller in &called_by[at] {
                    if group[caller] == usize::MAX {
                        group[caller] = start;
                        members.push(caller);
                        pending.push(caller);
                    }
                }
            }
            // One function on its own is a cycle only if it calls itself
            if members.len() > 1 || calls[start].contains(&start) {
                let first = members.iter().map(|&i| ids[i]).min().expect("at least one");
                for &member in &members {
                    cycles.insert(ids[member], first);
                }
            }
        }
        self.call_cycles = cycles;
    }

    /// The tables stand-in `at` stands for, here and in what this addon depends
    /// on. None for any other table
    pub fn stand_in_members(&self, at: TableId) -> Vec<TableId> {
        if !is_stand_in(at) {
            return Vec::new()
        }
        let mut members: Vec<TableId> = Vec::new();
        for list in self.stand_ins.get(&at).into_iter().chain(self.layer.stand_ins.get(&at)) {
            for &member in list {
                if !members.contains(&member) {
                    members.push(member);
                }
            }
        }
        members
    }

    /// What a walk from `self` would read differently from one from `older`.
    /// Empty when the walks have settled
    pub(super) fn changed_reads(&self, older: &Analysis) -> HashSet<Read> {
        let mut changed = HashSet::default();
        for id in self.method_selves.keys().chain(older.method_selves.keys()) {
            if self.method_selves.get(id) != older.method_selves.get(id) {
                changed.insert(Read::MethodSelf(*id));
            }
        }
        for &at in self.saved_tables.symmetric_difference(&older.saved_tables) {
            changed.insert(Read::Saved(at));
        }
        fn keys<K: std::hash::Hash + Eq + Copy, V>(a: &HashMap<K, V>, b: &HashMap<K, V>) -> HashSet<K> {
            a.keys().chain(b.keys()).copied().collect()
        }
        let longest = self.bindings.len().max(older.bindings.len());
        for id in 0..longest {
            let same = match (self.bindings.get(id), older.bindings.get(id)) {
                (Some(a), Some(b)) => a.name == b.name && a.types == b.types && a.assigned == b.assigned
                    && a.last_write == b.last_write && a.closure_written == b.closure_written,
                _ => false,
            };
            if !same {
                changed.insert(Read::Binding(id));
            }
        }
        for id in keys(&self.functions, &older.functions) {
            let (a, b) = (self.functions.get(&id), older.functions.get(&id));
            if a.map(FunctionInfo::signature) != b.map(FunctionInfo::signature) {
                changed.insert(Read::Function(id));
            }
            if self.call_cycles.get(&id) != older.call_cycles.get(&id) {
                changed.insert(Read::CallCycle(id));
            }
            if a.map(|f| f.method) != b.map(|f| f.method) {
                changed.insert(Read::Method(id));
            }
            let types_differ = match (a, b) {
                (Some(a), Some(b)) => a.parameters.iter().map(|p| p.types).ne(b.parameters.iter().map(|p| p.types)),
                (a, b) => a.is_some() != b.is_some(),
            };
            if types_differ {
                changed.insert(Read::ParameterTypes(id));
            }
        }
        for at in keys(&self.fields, &older.fields) {
            let (a, b) = (self.fields.get(&at), older.fields.get(&at));
            if a == b {
                continue
            }
            changed.insert(Read::Fields(at));
            let empty = HashMap::default();
            let (a, b) = (a.unwrap_or(&empty), b.unwrap_or(&empty));
            for name in a.keys().chain(b.keys()) {
                if a.get(name) != b.get(name) {
                    changed.insert(Read::Field(at, name_key(name)));
                }
            }
        }
        for name in self.any_table_fields.keys().chain(older.any_table_fields.keys()) {
            if self.any_table_fields.get(name) != older.any_table_fields.get(name) {
                changed.insert(Read::AnyTableField(name_key(name)));
            }
        }
        for &id in self.opened_types.symmetric_difference(&older.opened_types) {
            changed.insert(Read::Opened(id));
        }
        for name in self.function_fields.symmetric_difference(&older.function_fields) {
            changed.insert(Read::FunctionField(name_key(name)));
        }
        for at in keys(&self.entries, &older.entries) {
            if self.entries.get(&at) != older.entries.get(&at) {
                changed.insert(Read::Entries(at));
            }
        }
        for at in keys(&self.metatables, &older.metatables) {
            if self.metatables.get(&at) != older.metatables.get(&at) {
                changed.insert(Read::Metatable(at));
            }
        }
        for at in keys(&self.mixins, &older.mixins) {
            if self.mixins.get(&at) != older.mixins.get(&at) {
                changed.insert(Read::Mixins(at));
            }
        }
        for at in keys(&self.mixed_into, &older.mixed_into) {
            if self.mixed_into.get(&at) != older.mixed_into.get(&at) {
                changed.insert(Read::MixedInto(at));
            }
        }
        for at in keys(&self.mixin_copies, &older.mixin_copies) {
            if self.mixin_copies.get(&at) != older.mixin_copies.get(&at) {
                changed.insert(Read::MixedInto(at));
            }
        }
        for at in keys(&self.parents, &older.parents) {
            if self.parents.get(&at) != older.parents.get(&at) {
                changed.insert(Read::Parent(at));
            }
        }
        for (at, event) in keys(&self.triggered, &older.triggered) {
            if self.triggered.get(&(at, event)) != older.triggered.get(&(at, event)) {
                changed.insert(Read::Triggered(at, event));
            }
        }
        for at in keys(&self.pool_objects, &older.pool_objects) {
            if self.pool_objects.get(&at) != older.pool_objects.get(&at) {
                changed.insert(Read::PoolObject(at));
            }
        }
        for id in keys(&self.vararg_values, &older.vararg_values) {
            if self.vararg_values.get(&id) != older.vararg_values.get(&id) {
                changed.insert(Read::Varargs(id));
            }
        }
        if self.lua_mixins != older.lua_mixins {
            changed.insert(Read::LuaMixins);
        }
        for at in keys(&self.stand_ins, &older.stand_ins) {
            if self.stand_ins.get(&at) != older.stand_ins.get(&at) {
                changed.insert(Read::StandIn(at));
            }
        }
        changed
    }

    /// Adds what `other` had to everything a walk reads from this one
    pub(super) fn combine_inputs(&mut self, other: &Analysis) {
        for (at, members) in &other.stand_ins {
            let mine = self.stand_ins.entry(*at).or_default();
            for &member in members {
                if !mine.contains(&member) {
                    mine.push(member);
                }
            }
            mine.sort();
        }
        for (at, fields) in &other.fields {
            let mine = self.fields.entry(*at).or_default();
            for (name, &ty) in fields {
                *mine.entry(name.clone()).or_default() |= ty;
            }
        }
        for (at, &ty) in &other.parents {
            *self.parents.entry(*at).or_default() |= ty;
        }
        for (key, values) in &other.triggered {
            self.triggered.entry(*key)
                .and_modify(|all| *all = all.union(values))
                .or_insert_with(|| values.clone());
        }
        for (at, mixins) in &other.mixins {
            let mine = self.mixins.entry(*at).or_default();
            for mixin in mixins {
                if !mine.contains(mixin) {
                    mine.push(*mixin);
                }
            }
        }
        for (at, &(named, extended)) in &other.mixed_into {
            match self.mixed_into.get(at).copied() {
                None => { self.mixed_into.insert(*at, (named, extended)); }
                Some((mine, mine_extended)) if mine == named => { self.mixed_into.insert(*at, (named, extended || mine_extended)); }
                // Mixed into objects of no one type
                Some(_) => { self.mixed_into.remove(at); }
            }
        }
        for (at, tables) in &other.mixin_copies {
            let mine = self.mixin_copies.entry(*at).or_default();
            for table in tables {
                if !mine.contains(table) {
                    mine.push(*table);
                }
            }
            mine.sort();
        }
        for (at, &ty) in &other.pool_objects {
            *self.pool_objects.entry(*at).or_default() |= ty;
        }
        for (name, &ty) in &other.any_table_fields {
            *self.any_table_fields.entry(name.clone()).or_default() |= ty;
        }
        self.opened_types.extend(other.opened_types.iter().copied());
        self.function_fields.extend(other.function_fields.iter().cloned());
        for (at, entries) in &other.entries {
            let mine = self.entries.entry(*at).or_default();
            mine.keys |= entries.keys;
            mine.values |= entries.values;
        }
        for (at, &ty) in &other.metatables {
            *self.metatables.entry(*at).or_default() |= ty;
        }
        for (id, values) in &other.vararg_values {
            let combined = match self.vararg_values.get(id) {
                Some(mine) => mine.union(values),
                None => values.clone(),
            };
            self.vararg_values.insert(*id, combined);
        }
        for (id, theirs) in &other.functions {
            let Some(mine) = self.functions.get_mut(id) else {
                self.functions.insert(*id, theirs.clone());
                continue
            };
            for (parameter, other_parameter) in mine.parameters.iter_mut().zip(&theirs.parameters) {
                parameter.types |= other_parameter.types;
            }
            if let (Some(varargs), Some(other_varargs)) = (&mut mine.varargs, &theirs.varargs) {
                varargs.types |= other_varargs.types;
            }
            mine.returns = mine.returns.union(&theirs.returns);
        }
        for (binding, theirs) in self.bindings.iter_mut().zip(&other.bindings) {
            if binding.name == theirs.name {
                binding.types |= theirs.types;
                binding.assigned |= theirs.assigned;
            }
        }
    }

    /// A hash of everything a walk reads from the one before
    pub(super) fn fingerprint(&self) -> u64 {
        use std::hash::{BuildHasher, Hash};
        // Spreads a weak hash's bits, so sums of them rarely collide
        fn mixed(value: impl Hash) -> u64 {
            let mut z = FastHash::default().hash_one(value).wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        // Maps iterate in no fixed order, so their entries' hashes are added,
        // which doesn't depend on it. Each map has its own tag
        fn map<K: Hash, V: Hash>(tag: u8, map: &HashMap<K, V>) -> u64 {
            map.iter().fold(0u64, |sum, entry| sum.wrapping_add(mixed((tag, entry))))
        }
        let fields = self.fields.iter()
            .flat_map(|(at, fields)| fields.iter().map(move |field| (at, field)))
            .fold(0u64, |sum, entry| sum.wrapping_add(mixed((0u8, entry))));
        let function_fields = self.function_fields.iter().fold(0u64, |sum, name| sum.wrapping_add(mixed((1u8, name))));
        let opened_types = self.opened_types.iter().fold(0u64, |sum, id| sum.wrapping_add(mixed((11u8, id))));
        let bindings = mixed(self.bindings.iter().map(|b| (&b.name, b.types, b.assigned)).collect::<Vec<_>>());
        mixed([
            fields,
            function_fields,
            bindings,
            map(2, &self.parents),
            map(12, &self.triggered),
            map(3, &self.mixins),
            map(4, &self.mixed_into),
            map(11, &self.mixin_copies),
            map(5, &self.pool_objects),
            map(6, &self.any_table_fields),
            map(7, &self.entries),
            map(8, &self.metatables),
            map(9, &self.vararg_values),
            map(10, &self.functions),
            map(13, &self.stand_ins),
            opened_types,
        ])
    }
}

/// The group of each node of a graph, by `edges` from each: nodes in one
/// group each reach the others (Kosaraju's algorithm)
pub(super) fn strongly_connected(edges: &[Vec<usize>]) -> Vec<usize> {
    let count = edges.len();
    let mut backwards: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (from, tos) in edges.iter().enumerate() {
        for &to in tos {
            backwards[to].push(from);
        }
    }
    // The order nodes finish in, going forwards
    let mut order = Vec::with_capacity(count);
    let mut visited = vec![false; count];
    for start in 0..count {
        if visited[start] {
            continue
        }
        visited[start] = true;
        let mut stack = vec![(start, 0)];
        while let Some((at, next)) = stack.pop() {
            if let Some(&to) = edges[at].get(next) {
                stack.push((at, next + 1));
                if !visited[to] {
                    visited[to] = true;
                    stack.push((to, 0));
                }
            } else {
                order.push(at);
            }
        }
    }
    // Then the groups reached backwards from the last to finish
    let mut group = vec![usize::MAX; count];
    for &start in order.iter().rev() {
        if group[start] != usize::MAX {
            continue
        }
        group[start] = start;
        let mut stack = vec![start];
        while let Some(at) = stack.pop() {
            for &from in &backwards[at] {
                if group[from] == usize::MAX {
                    group[from] = start;
                    stack.push(from);
                }
            }
        }
    }
    group
}
