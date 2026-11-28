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

//! Each body's record of what a walk read and did, for replaying it, and
//! what each key it influences depends on

use super::*;

/// A change a walk makes to what it's gathering across function bodies. Each
/// is made through `Walker::apply` and recorded, so a body whose inputs
/// haven't changed can make its changes again without being walked
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Write {
    Field(TableId, String, Type),
    AnyTableField(String, Type),
    /// A data type given fields that can't be named
    Opened(NamedId),
    StoredInField(FunctionId, String),
    Entry(TableId, Type, Type),
    Metatable(TableId, Type),
    CallArguments(FunctionId, Values),
    MixinsReplaced(TableId, Vec<Type>),
    MixinAdded(TableId, Type),
    MixedInto(TableId, NamedRef),
    /// A table `CopyTable` made, and the table it's a copy of
    CopiedFrom(TableId, TableId),
    PoolCallback(FunctionId),
    /// A function that may be called where that can't be seen
    Escaped(FunctionId),
    /// A method passed this as `self`: by a call in the files if true, else
    /// by the game, as a script handler
    SelfPassed(FunctionId, Type, bool),
    AssignedTo(FunctionId, BindingId),
    ParameterMixin(TableId, usize),
    Parent(TableId, Type),
    /// A callback registry's event, by its name's hash, triggered with these
    Triggered(TableId, u64, Values),
    PoolObject(TableId, Type),
    Function(FunctionId, Box<FunctionInfo>),
    /// A variable declared outside the body was assigned this
    Binding(BindingId, Type),
    /// A stand-in stands for these tables too
    StandIn(TableId, Vec<TableId>),
}

impl Write {
    /// The keys whose value this changes, as reading them sees it
    pub(super) fn keys(&self) -> Vec<Read> {
        match self {
            Write::Field(at, name, _) => vec![Read::Field(*at, name_key(name)), Read::Fields(*at)],
            Write::AnyTableField(name, _) => vec![Read::AnyTableField(name_key(name))],
            Write::Opened(id) => vec![Read::Opened(*id)],
            Write::StoredInField(_, name) => vec![Read::FunctionField(name_key(name))],
            Write::Entry(at, _, _) => vec![Read::Entries(*at)],
            Write::Metatable(at, _) => vec![Read::Metatable(*at)],
            Write::CallArguments(function, _) => vec![Read::ParameterTypes(*function), Read::Varargs(*function)],
            Write::MixinsReplaced(at, _) | Write::MixinAdded(at, _) | Write::ParameterMixin(at, _) => vec![Read::Mixins(*at)],
            Write::MixedInto(at, _) => vec![Read::MixedInto(*at)],
            Write::CopiedFrom(copy, _) => vec![Read::Fields(*copy), Read::Entries(*copy), Read::MixedInto(*copy)],
            Write::PoolCallback(function) | Write::Escaped(function) => vec![Read::ParameterTypes(*function)],
            Write::SelfPassed(function, _, _) => vec![Read::MethodSelf(*function)],
            Write::AssignedTo(function, _) => vec![Read::Function(*function)],
            Write::Parent(at, _) => vec![Read::Parent(*at)],
            Write::Triggered(at, event, _) => vec![Read::Triggered(*at, *event)],
            Write::PoolObject(at, _) => vec![Read::PoolObject(*at)],
            Write::Function(id, _) => vec![Read::Function(*id), Read::CallCycle(*id), Read::ParameterTypes(*id), Read::Method(*id)],
            Write::Binding(id, _) => vec![Read::Binding(*id)],
            Write::StandIn(at, _) => vec![Read::StandIn(*at)],
        }
    }
}

/// Something a function body's walk read that a later walk could see
/// differently: from the previous walk, or another body earlier in this one
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Read {
    /// What a method's `self` is, when not its table (`Analysis::method_selves`)
    MethodSelf(FunctionId),
    Binding(BindingId),
    Function(FunctionId),
    /// A field by name, which goes by its name's hash. Two names sharing a
    /// hash only means a body is walked again when it needn't be
    Field(TableId, u64),
    /// All of a table's fields, as `t[k]` and `pairs` see them
    Fields(TableId),
    AnyTableField(u64),
    Opened(NamedId),
    FunctionField(u64),
    Entries(TableId),
    Metatable(TableId),
    Mixins(TableId),
    MixedInto(TableId),
    Parent(TableId),
    Triggered(TableId, u64),
    PoolObject(TableId),
    Varargs(FunctionId),
    /// Which cycle of calls a function is in, if any, which finding
    /// recursion reads (see `Analysis::call_cycles`)
    CallCycle(FunctionId),
    /// Only whether a function is a method: all of its signature what a call
    /// passes it depends on. Never read by a body, only depended on (see
    /// `Walker::call`)
    Method(FunctionId),
    /// What a method's `self` is, which the statement defining it decides.
    /// Never read by a body, only depended on
    SelfType(FunctionId),
    /// The tables a stand-in stands for
    StandIn(TableId),
    /// Only the types of a function's parameters, as the previous walk settled them
    ParameterTypes(FunctionId),
    /// What the globals XML names as mixins are
    LuaMixins,
    /// Whether a table is one the game saves between sessions
    Saved(TableId),
}

/// A name's hash, for `Read`
pub(super) fn name_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// What walking a function body did: everything it added and changed, and
/// everything it read. A later walk where none of what it read has changed
/// does the same again instead of walking it
#[derive(Debug)]
pub(super) struct Record {
    /// What the body was walked with, which must be the same to replay it
    pub(super) self_type: Option<Type>,
    pub(super) with_prior: bool,
    pub(super) bindings_start: usize,
    pub(super) function_count_start: usize,
    /// The bindings it declared, as they were when it ended. Shared, like the
    /// rest, with the records replaying it on later walks, which are the same
    pub(super) bindings: Arc<[Binding]>,
    pub(super) current: Arc<[Type]>,
    /// What it added to the walk's lists, as positions in those of the walk
    /// that recorded it
    pub(super) references: std::ops::Range<usize>,
    pub(super) field_references: std::ops::Range<usize>,
    pub(super) diagnostics: std::ops::Range<usize>,
    pub(super) pending_calls: std::ops::Range<usize>,
    /// Upvalue reads, by position among its references
    pub(super) deferred: Arc<[usize]>,
    /// Globals it was first to use
    pub(super) globals: Arc<[(String, BindingId)]>,
    /// Functions it holds, itself included
    pub(super) function_count: usize,
    pub(super) writes: Arc<[Arc<Write>]>,
    pub(super) reads: Arc<HashSet<Read>>,
    /// Upvalues read as they were where the body is, which must be the same
    /// to replay it
    pub(super) upvalues: Arc<[(BindingId, Type)]>,
    /// The function it's in, if it's not at a file's top level
    pub(super) parent: Option<FunctionId>,
    /// What it wrote itself, not counting the functions inside it, which
    /// `writes` includes as replaying it does them too. What it read itself
    /// is in `own_influences`
    pub(super) own_writes: Arc<[Arc<Write>]>,
    /// Each key its own writes, the bindings it declares and the calls
    /// passing its parameters on influence, with the reads that went into
    /// it: those of the statement doing it, and of the locals it used
    pub(super) own_influences: Arc<BodyInfluences>,
}

/// Each key a body influences, with the keys it depends on
pub(super) type Influences = [(Read, Arc<[Read]>)];

/// What a body did and read as it was walked, from which what each key it
/// influences depended on is worked out when asked (see
/// `BodyInfluences::get`): only the last walk's are ever asked for
pub(super) struct BodyInfluences {
    pub(super) function: FunctionId,
    pub(super) body: BodyDeps,
    /// What it read outside any statement
    pub(super) top: Dep,
    pub(super) own_writes: Arc<[Arc<Write>]>,
    /// The locals it declares itself, not those of functions inside it
    pub(super) own_bindings: Vec<BindingId>,
    /// The function each call passing its parameters on calls, and what the
    /// call read
    pub(super) pending: Vec<(FunctionId, Arc<Dep>)>,
    pub(super) found: std::sync::OnceLock<Arc<Influences>>,
}

impl std::fmt::Debug for BodyInfluences {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "BodyInfluences({:?})", self.function)
    }
}

impl BodyInfluences {
    /// Each key it influences: each write, each local it declares, its
    /// signature, what it passes on. With what each depended on: what the
    /// statement doing it read, and what the locals it used read
    pub(super) fn get(&self) -> &Influences {
        self.found.get_or_init(|| self.work_out())
    }

    /// What it read itself, not counting the functions inside it
    pub(super) fn own_reads(&self) -> HashSet<Read> {
        self.body.own_reads()
    }

    pub(super) fn work_out(&self) -> Arc<Influences> {
        let (body, id) = (&self.body, self.function);
        let closed = body.closed();
        // What reaches these from its top (its parameters) does so through
        // the locals the statements read, not wholesale
        let requirements = body.final_reads(&body.requirements, &closed);
        let mut outside = Dep::default();
        outside.extend(&body.signature);
        outside.extend(&body.calls);
        outside.extend(&self.top);
        let (signature, calls, outside) = (body.final_reads(&body.signature, &closed), body.final_reads(&body.calls, &closed), body.final_reads(&outside, &closed));
        let mut influences: Vec<(Read, Arc<[Read]>)> = Vec::new();
        // Writes of one statement share its reads: found once
        let mut found: HashMap<*const Dep, Arc<[Read]>> = HashMap::default();
        let mut reads_of = |dep: &Arc<Dep>| found.entry(Arc::as_ptr(dep)).or_insert_with(|| body.final_reads(dep, &closed)).clone();
        for (index, write) in self.own_writes.iter().enumerate() {
            // Its own signature: what its returns, requirements and calls read
            if let Write::Function(written, _) = &**write
                && *written == id {
                influences.push((Read::Function(id), signature.clone()));
                // Its parameters' types: what calls pass (their own
                // influence) and what the body requires of them
                influences.push((Read::ParameterTypes(id), requirements.clone()));
                influences.push((Read::CallCycle(id), calls.clone()));
                // Whether it's a method is how it's written, which reads nothing
                influences.push((Read::Method(id), Arc::from([])));
                continue
            }
            let deps = match body.write_deps.get(index) {
                Some(Some(dep)) => reads_of(dep),
                // Outside any statement: as the signature and calls do
                _ => outside.clone(),
            };
            for key in write.keys() {
                influences.push((key, deps.clone()));
            }
        }
        for &binding in &self.own_bindings {
            let deps: Arc<[Read]> = body.local_numbers.get(&binding)
                .and_then(|&number| closed.get(number as usize))
                .map(|reads| reads.iter().map(|number| body.keys[number as usize].clone()).collect())
                .unwrap_or_default();
            influences.push((Read::Binding(binding), deps));
        }
        for (function, dep) in &body.self_type_deps {
            influences.push((Read::SelfType(*function), reads_of(dep)));
        }
        for (callee, dep) in &self.pending {
            let deps = reads_of(dep);
            influences.push((Read::ParameterTypes(*callee), deps.clone()));
            influences.push((Read::Varargs(*callee), deps));
        }
        influences.into()
    }
}

/// A set of a body's numbers for keys or locals (see `BodyDeps`): a bit
/// each, as a body reads few
#[derive(Default, Clone, Debug, PartialEq)]
pub(super) struct Bits(Vec<u64>);

impl Bits {
    pub(super) fn of(numbers: &[u32]) -> Bits {
        let mut bits = Bits::default();
        for &number in numbers {
            bits.insert(number);
        }
        bits
    }

    pub(super) fn insert(&mut self, number: u32) {
        let word = number as usize / 64;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (number % 64);
    }

    /// Takes it out, returning whether it was there
    pub(super) fn remove(&mut self, number: u32) -> bool {
        let Some(word) = self.0.get_mut(number as usize / 64) else { return false };
        let bit = 1 << (number % 64);
        let there = *word & bit != 0;
        *word &= !bit;
        there
    }

    /// Adds `other`'s, returning whether that added any
    pub(super) fn union(&mut self, other: &Bits) -> bool {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        let mut grew = false;
        for (mine, theirs) in self.0.iter_mut().zip(&other.0) {
            let joined = *mine | theirs;
            grew |= joined != *mine;
            *mine = joined;
        }
        grew
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            (0..64u32).filter(move |bit| bits & (1 << bit) != 0).map(move |bit| word as u32 * 64 + bit)
        })
    }
}

/// What a statement read: the keys, and the body's own locals, whose own
/// reads are added once the body is done (see `BodyDeps::closed`), as a
/// local can be assigned after it's read, round a loop. By their numbers in
/// the body
#[derive(Default, Debug, Clone)]
pub(super) struct Dep {
    pub(super) reads: Bits,
    pub(super) locals: Bits,
}

impl Dep {
    /// From stretches of a statement's logs
    pub(super) fn of(reads: &[u32], locals: &[u32]) -> Dep {
        Dep { reads: Bits::of(reads), locals: Bits::of(locals) }
    }

    pub(super) fn extend(&mut self, other: &Dep) {
        self.reads.union(&other.reads);
        self.locals.union(&other.locals);
    }
}

/// A statement being walked: what it read, and what it did
#[derive(Default)]
pub(super) struct DepLevel {
    /// The numbers of the keys it read, in order, repeats and all: a call's
    /// reads are a stretch of it (see `CallScope`)
    pub(super) reads: Vec<u32>,
    /// The numbers of the body's own locals it read, likewise
    pub(super) locals: Vec<u32>,
    /// Positions in `BodyDeps::write_deps`
    pub(super) writes: Vec<usize>,
    /// The body's own locals it assigned or narrowed
    pub(super) bindings: Vec<BindingId>,
    /// Positions in the walk's pending calls it made
    pub(super) pending: Vec<usize>,
    /// Methods it defines, whose `self` it decides
    pub(super) self_types: Vec<FunctionId>,
    /// Whether it gives the body's signature something: a return, a
    /// requirement of a parameter, a field given what's passed, a call
    /// passing a method on
    pub(super) signature: bool,
    /// Whether it calls a function, which finding cycles of calls reads
    pub(super) calls: bool,
    /// Whether it requires a type of a parameter of the body, which its
    /// parameters' types depend on (as well as what calls pass)
    pub(super) requirement: bool,
}

/// What each thing a body does depends on, as it's walked (see
/// `Record::own_influences`). The keys it reads and its own locals are
/// numbered in the order it first meets them, so what a statement read is
/// a few bits
#[derive(Default)]
pub(super) struct BodyDeps {
    pub(super) bindings_start: usize,
    /// The body's top, then each statement being walked, innermost last
    pub(super) levels: Vec<DepLevel>,
    pub(super) keys: Vec<Read>,
    pub(super) key_numbers: HashMap<Read, u32>,
    /// The keys it read, which replaying it checks: not `Read::Method`,
    /// which is only depended on
    pub(super) noted: Bits,
    pub(super) locals: Vec<BindingId>,
    pub(super) local_numbers: HashMap<BindingId, u32>,
    /// Each write's statement's reads, once the statement is done
    pub(super) write_deps: Vec<Option<Arc<Dep>>>,
    /// The reads of each statement assigning each local, by its number,
    /// joined once the body is done
    pub(super) binding_deps: HashMap<u32, Vec<Arc<Dep>>>,
    pub(super) pending_deps: Vec<(usize, Arc<Dep>)>,
    pub(super) self_type_deps: Vec<(FunctionId, Arc<Dep>)>,
    pub(super) signature: Dep,
    pub(super) calls: Dep,
    pub(super) requirements: Dep,
}

impl BodyDeps {
    pub(super) fn key(&mut self, read: &Read) -> u32 {
        if let Some(&number) = self.key_numbers.get(read) {
            return number
        }
        let number = self.keys.len() as u32;
        self.keys.push(read.clone());
        self.key_numbers.insert(read.clone(), number);
        number
    }

    pub(super) fn local(&mut self, id: BindingId) -> u32 {
        if let Some(&number) = self.local_numbers.get(&id) {
            return number
        }
        let number = self.locals.len() as u32;
        self.locals.push(id);
        self.local_numbers.insert(id, number);
        number
    }

    /// A statement is done: what it did depends on all it read
    pub(super) fn resolve(&mut self, level: DepLevel) {
        // Most statements did nothing that depends on what they read
        if level.writes.is_empty() && level.bindings.is_empty() && level.pending.is_empty() && level.self_types.is_empty()
            && !level.signature && !level.calls && !level.requirement {
            return
        }
        let dep = Arc::new(Dep::of(&level.reads, &level.locals));
        for write in level.writes {
            self.write_deps[write] = Some(dep.clone());
        }
        for binding in level.bindings {
            let number = self.local(binding);
            let deps = self.binding_deps.entry(number).or_default();
            // Assigned again in the same statement, or narrowed and joined
            if !deps.last().is_some_and(|last| Arc::ptr_eq(last, &dep)) {
                deps.push(dep.clone());
            }
        }
        for pending in level.pending {
            self.pending_deps.push((pending, dep.clone()));
        }
        for function in level.self_types {
            self.self_type_deps.push((function, dep.clone()));
        }
        if level.signature {
            self.signature.extend(&dep);
        }
        if level.calls {
            self.calls.extend(&dep);
        }
        if level.requirement {
            self.requirements.extend(&dep);
        }
    }

    /// Each of the body's locals' keys, by its number, with those of the
    /// locals that went into it, however indirectly
    pub(super) fn closed(&self) -> Vec<Bits> {
        let mut direct: Vec<Dep> = vec![Dep::default(); self.locals.len()];
        for (&number, deps) in &self.binding_deps {
            for dep in deps {
                direct[number as usize].extend(dep);
            }
        }
        let mut closed: Vec<Bits> = direct.iter().map(|dep| dep.reads.clone()).collect();
        loop {
            let mut grew = false;
            for (number, dep) in direct.iter().enumerate() {
                for local in dep.locals.iter().filter(|&local| local as usize != number) {
                    let theirs = closed[local as usize].clone();
                    grew |= closed[number].union(&theirs);
                }
            }
            if !grew {
                return closed
            }
        }
    }

    /// The keys `dep` reads, with those its locals read
    pub(super) fn final_reads(&self, dep: &Dep, closed: &[Bits]) -> Arc<[Read]> {
        let mut reads = dep.reads.clone();
        for local in dep.locals.iter() {
            if let Some(theirs) = closed.get(local as usize) {
                reads.union(theirs);
            }
        }
        reads.iter().map(|number| self.keys[number as usize].clone()).collect()
    }

    /// The keys it read
    pub(super) fn own_reads(&self) -> HashSet<Read> {
        self.noted.iter().map(|number| self.keys[number as usize].clone()).collect()
    }
}
