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

use super::*;
use crate::syntax::syntax::Generator;

fn analyse(source: &str) -> Analysis {
    analyse_with(source, &Arc::new(Definitions::default()))
}

fn analyse_with(source: &str, definitions: &Arc<Definitions>) -> Analysis {
    let mut generator = Generator::new(source);
    let root = SyntaxNode::new_root(generator.process_all());
    Analysis::analyse_file(&root, definitions)
}

/// Type at each use of `name`, in source order
fn types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse(source), name)
}

fn describe_uses(file: &Analysis, name: &str) -> Vec<String> {
    file.references.iter()
        .filter(|r| file.binding(r).name.as_ref() == name)
        .map(|r| file.describe(r.types))
        .collect()
}

/// With records kept: the analysis, the table each of `tables` names
/// holds, and the function each of `functions` holds
fn analyse_keeping(source: &str) -> Analysis {
    keeping_records(|| analyse(source))
}

fn table_of(analysis: &Analysis, name: &str) -> TableId {
    analysis.bindings.iter().find(|b| &*b.name == name).and_then(|b| b.types.named.identity()).expect("a table")
}

fn function_of(analysis: &Analysis, name: &str) -> FunctionId {
    analysis.bindings.iter().find(|b| &*b.name == name)
        .and_then(|b| match b.types.function { FunctionRef::One(id) => Some(id), _ => None })
        .expect("a function")
}

/// What body `function`'s record says `key` depends on
fn depends(analysis: &Analysis, function: FunctionId, key: &Read) -> Vec<Read> {
    let record = analysis.records.get(&function).expect("a record");
    record.own_influences.get().iter()
        .filter(|(k, _)| k == key)
        .flat_map(|(_, deps)| deps.iter().cloned())
        .collect()
}

#[test]
fn tables_filling_each_other_are_an_entry_loop() {
    // A pool's free and used objects, each table filled from the other's
    // entries, and one only filled from itself, which is no loop
    let analysis = keeping_records(|| analyse_with("local Pool = { InUse = {}, Free = {} }
        local own = {}
        function Fetch(name)
            local _, frame = next(Pool.Free)
            Pool.InUse[name] = frame or {}
            own[name] = own[1]
        end
        function Release(name)
            Pool.Free[name] = Pool.InUse[name]
        end", &library()));
    let at = |field: &str| analysis.fields.values()
        .find_map(|fields| fields.get(field))
        .and_then(|ty| ty.named.identity())
        .expect("a table");
    assert!(analysis.entry_loops.contains(&at("InUse")));
    assert!(analysis.entry_loops.contains(&at("Free")));
    assert!(!analysis.entry_loops.contains(&table_of(&analysis, "own")));
}

#[test]
fn reading_a_copy_reads_what_its_function_made() {
    // `TableKeys` returns a new table each call, holding what `keys`
    // does: so a body iterating one reads `keys`, and must be walked
    // again when that changes, not replayed
    let analysis = keeping_records(|| analyse_with("local function TableKeys(original)
            local keys = {}
            for key in pairs(original) do table.insert(keys, key) end
            return keys
        end
        local function Days(h)
            local days = TableKeys(h)
            for _, day in ipairs(days) do local d = day end
        end
        Days({ x = 1 })", &library()));
    let (keys, days) = (table_of(&analysis, "keys"), function_of(&analysis, "Days"));
    let record = analysis.records.get(&days).expect("a record");
    assert!(record.reads.contains(&Read::Entries(keys)));
}

#[test]
fn writes_depend_on_what_their_statement_and_its_locals_read() {
    let analysis = analyse_keeping("local t, u = { x = 1 }, {}
        local function f()
            local x = t.x
            u.y = x
            u.z = 1
        end");
    let (t, u, f) = (table_of(&analysis, "t"), table_of(&analysis, "u"), function_of(&analysis, "f"));
    let read_x = Read::Field(t, name_key("x"));
    assert!(depends(&analysis, f, &Read::Field(u, name_key("y"))).contains(&read_x));
    assert!(!depends(&analysis, f, &Read::Field(u, name_key("z"))).contains(&read_x));
}

#[test]
fn a_local_assigned_later_round_a_loop_counts() {
    let analysis = analyse_keeping("local t, u = { x = 1 }, {}
        local function f()
            local v
            for i = 1, 2 do
                u.a = v
                v = t.x
            end
        end");
    let (t, u, f) = (table_of(&analysis, "t"), table_of(&analysis, "u"), function_of(&analysis, "f"));
    assert!(depends(&analysis, f, &Read::Field(u, name_key("a"))).contains(&Read::Field(t, name_key("x"))));
}

#[test]
fn what_a_call_passes_depends_on_its_arguments_not_its_returns() {
    let analysis = analyse_keeping("local t = { arg = 1, r = 2 }
        local function g(p) return t.r end
        local function f() local y = g(t.arg) return y end");
    let (t, g, f) = (table_of(&analysis, "t"), function_of(&analysis, "g"), function_of(&analysis, "f"));
    let passed = depends(&analysis, f, &Read::ParameterTypes(g));
    assert!(passed.contains(&Read::Field(t, name_key("arg"))), "{passed:?}");
    assert!(passed.contains(&Read::Method(g)), "{passed:?}");
    assert!(!passed.contains(&Read::Function(g)), "{passed:?}");
    // What it returns does depend on `g`'s
    assert!(depends(&analysis, f, &Read::Function(f)).contains(&Read::Function(g)));
}

#[test]
fn a_signature_depends_only_on_what_gives_it_something() {
    let analysis = analyse_keeping("local t, u = { w = 1 }, {}
        local function f(a) u.q = t.w return a end");
    let (t, f) = (table_of(&analysis, "t"), function_of(&analysis, "f"));
    let signature = depends(&analysis, f, &Read::Function(f));
    assert!(!signature.contains(&Read::Field(t, name_key("w"))), "{signature:?}");
    assert!(signature.contains(&Read::ParameterTypes(f)), "{signature:?}");
    // Its parameters' types: what it requires of them, and nothing here
    assert!(depends(&analysis, f, &Read::ParameterTypes(f)).is_empty());
}

#[test]
fn cycles_of_calls_are_found_once_a_walk_is_done() {
    let analysis = analyse("local a, b, c
        function a() return b() end
        function b() return a() end
        function c() return a() end");
    let cycle = |name: &str| analysis.bindings.iter().find(|b| &*b.name == name)
        .and_then(|b| match b.types.function { FunctionRef::One(id) => analysis.call_cycles.get(&id).copied(), _ => None });
    assert!(cycle("a").is_some());
    assert_eq!(cycle("a"), cycle("b"));
    // Calls into the cycle, but isn't called back
    assert_eq!(cycle("c"), None);
}

#[test]
fn partly_meeting_requirement_settles() {
    // What's passed only partly meets `ipairs`' table: its tables, not
    // a table of unknown origin, whose contents would read as nothing
    // yet, so passing them back would swap the two each walk
    let source = "local function Combine(elements)
        local result = ''
        for _, entry in ipairs(elements) do
            if entry.kind == 1 then result = result .. Combine(entry.value) else result = result .. entry.value end
        end
        return result
    end
    local root = { kind = 1, value = { { kind = 2, value = 3 } } }
    Combine({ root })";
    let analysis = analyse_with(source, &library());
    assert!(analysis.walks <= 5, "{} walks", analysis.walks);
    assert_eq!(describe_uses(&analysis, "elements")[0], "table[]");
}

#[test]
fn own_variable_passed_back_settles() {
    // A table of unknown origin read in the function, passed back to it:
    // which of its variables the parameter holds would swap each walk
    let source = "local function Degroup(details)
        for _, entry in ipairs(details.entries) do
            if entry.kind == 'group' then Degroup(entry) end
        end
        local final = {}
        for _, entry in ipairs(details.entries) do
            if entry.kind == 'group' then table.insert(final, entry.entries[1]) else table.insert(final, entry) end
        end
        details.entries = final
    end
    function Start(root) Degroup(root) end";
    assert!(!analyse_with(source, &library()).cycled);
}

#[test]
fn stand_in_for_unknown_table_reads_nothing_yet() {
    // A stand-in for tables including one of unknown origin: a field
    // nothing's seen written reads as through that table, not as
    // anything it may hold under any key, so passing what it reads back
    // doesn't swap what the parameter is each walk
    let source = "local function Find(details, root)
        if root.details == details then return root end
        for _, e in ipairs(root.children) do
            local result = Find(details, e)
            if result then return result end
        end
    end
    local function Delete(root)
        root.deleted = true
        if root.details.last then Delete(root:GetParent()) end
    end
    function Start(details, top) Delete(Find(details, top)) end";
    assert!(!analyse_with(source, &library()).cycled);
}

#[test]
fn chained_calls_make_their_own_tables() {
    // `Make(...)` and `Make(...):Items()` start at the same place: the
    // object one makes and the copy the other does are separate tables,
    // not one with the mixins and contents of both, which swapped each walk
    let source = "local M = {}
    function M:Init(data) self.data = data end
    function M:Items() return CopyTable(self.data.items) end
    local function Make(data) return CreateAndInitFromMixin(M, data) end
    local x = Make({ items = { 'a' } }):Items()";
    assert!(!analyse_with(source, &library()).cycled);
}

#[test]
fn write_through_other_tables_field_settles() {
    // `entry.resource` of an unknown `entry` reads as the string written
    // through it: writing to it is still a write to some table, or it
    // would be gone the next walk, and back the one after
    let source = "local function Migrate(group)
        for i = #group.entries, 1, -1 do
            local entry = group.entries[i]
            if entry.kind == 'group' then
                Migrate(entry)
            elseif entry.resource and entry.resource.resource == 'pain' then
                entry.resource.resource = 'fury'
            end
        end
    end
    local migrations = { Migrate }
    function Run(config) for _, m in ipairs(migrations) do m(config) end end";
    assert!(!analyse_with(source, &library()).cycled);
}

#[test]
fn loop_through_field_settles() {
    // What `text` is passed comes back to it through `entry.message`,
    // two walks round: walks before and after the switch to unknowns
    // left one side a string and the other unknown, swapping each walk
    let source = "local module = {}
    function module:AddMessage(frame)
        local entry = frame.historyBuffer:GetEntryAtIndex(1)
        entry.message = self:InsertTimeStamp(entry.message, frame)
    end
    function module:InsertTimeStamp(text, cf)
        if type(text) == 'string' then return '[time] ' .. text end
        return text
    end";
    let analysis = analyse_with(source, &library());
    assert!(!analysis.cycled);
    // Only `text` is in the loop, not `cf`
    let loops: Vec<usize> = analysis.field_loops.iter().map(|&(_, i)| i).collect();
    assert_eq!(loops, [0]);
}

/// The parameters found in loops through fields, by name
fn loop_parameters(source: &str) -> Vec<String> {
    let analysis = analyse_with(source, &library());
    let mut names: Vec<String> = analysis.field_loops.iter()
        .map(|(id, i)| analysis.functions[id].parameters[*i].name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn loop_through_constructor_field() {
    // `{ rollID = rollID }` put in a list, read back and passed on
    let source = "local M = {}
    local waiting = {}
    function M:Next(event)
        local roll = waiting[1]
        M:Start(event, roll.rollID)
    end
    function M:Start(event, rollID)
        table.insert(waiting, { rollID = rollID })
    end";
    assert_eq!(loop_parameters(source), ["rollID"]);
}

#[test]
fn loop_through_unknown_tables_field() {
    // `buffs.filter` of a table it can't identify is any table's
    // `filter`, as `button.filter` of each button it can't identify is.
    // What's written may be the parameter, through `or`
    let source = "local function update(button, filter)
        button.filter = filter or nil
    end
    local function icons(frame, filter)
        for i = 1, 3 do update(frame.buttons[i], filter) end
    end
    function Update(self)
        local buffs = self.Buffs
        icons(buffs, buffs.filter or 'HELPFUL')
    end";
    assert_eq!(loop_parameters(source), ["filter", "filter"]);
}

#[test]
fn recursive_varargs_settle() {
    // Passing `...` back with more in front would make it one longer each
    // time round, as AceConfigRegistry's `validate(v, errlvl, k, "args", ...)`
    let source = "local function f(stop, ...) local a, b, c = ... if not stop then f(true, 's', ...) end end f(false, 1)";
    assert!(analyse(source).walks <= 5);
    assert_eq!(types_of(source, "a")[0], "number|string");
    assert_eq!(types_of(source, "b")[0], "number|string|nil");
    assert_eq!(types_of(source, "c")[0], "number|string|nil");
}

#[test]
fn recursive_returns_settle() {
    // Each walk would add a position to what `f` returns
    let analysis = analyse("local function f() return 1, f() end local a, b, c = f()");
    assert!(analysis.walks <= 5, "{} walks", analysis.walks);
    assert_eq!(describe_uses(&analysis, "c")[0], "number");
    let source = "local function f(stop) if stop then return 1 end return 1, f(true) end local a, b, c = f()";
    assert_eq!(types_of(source, "b")[0], "number?");
    assert_eq!(types_of(source, "c")[0], "number?");
    // Through another function, and through `pcall`
    let source = "local g local function f(x) if x then return 1 end return 'a', g() end g = function() return true, f() end local a, b, c = f()";
    assert_eq!(types_of(source, "a")[0], "number|string");
    assert_eq!(types_of(source, "c")[0], "number|string|true|nil");
    let source = "local function f(x) if x then return 1 end return 2, select(2, pcall(f, true)) end local a, b, c = f()";
    assert!(analyse(source).walks <= 6);
}

#[test]
fn literals() {
    assert_eq!(types_of("local x = 5", "x"), ["number"]);
    assert_eq!(types_of("local x = 'a'", "x"), ["string"]);
    assert_eq!(types_of("local x = true", "x"), ["true"]);
    assert_eq!(types_of("local x = nil", "x"), ["nil"]);
    assert_eq!(types_of("local x = {}", "x"), ["table"]);
    assert_eq!(types_of("local x = function() end", "x"), ["function()"]);
    assert_eq!(types_of("local x", "x"), ["nil"]);
}

#[test]
fn operators() {
    assert_eq!(types_of("local a = 1 + 2 * 3", "a"), ["number"]);
    assert_eq!(types_of("local a = 'a' .. 1", "a"), ["string"]);
    assert_eq!(types_of("local a = 1 < 2", "a"), ["boolean"]);
    assert_eq!(types_of("local a = not x", "a"), ["boolean"]);
    assert_eq!(types_of("local a = #t", "a"), ["number"]);
    assert_eq!(types_of("local a = -#t", "a"), ["number"]);
    assert_eq!(types_of("local a = 2 ^ 3", "a"), ["number"]);
    assert_eq!(types_of("local a = (1 .. 2)", "a"), ["string"]);
}

#[test]
fn operators_on_values_they_cant_take() {
    let warnings = |source: &str| -> Vec<String> { analyse(source).diagnostics.iter().map(|d| d.message.clone()).collect() };
    assert_eq!(warnings("local b = true local n = b + 1"), ["Can't do arithmetic on a boolean"]);
    assert_eq!(warnings("local f = function() end local n = 2 * f"), ["Can't do arithmetic on a function"]);
    assert_eq!(warnings("local b = 1 > 2 local n = -b"), ["Can't do arithmetic on a boolean"]);
    assert_eq!(warnings("local b = false local s = 'x' .. b"), ["Can't concatenate a boolean"]);
    assert_eq!(warnings("local n = 5 local l = #n"), ["Can't get the length of a number"]);
    assert_eq!(warnings("local a, b = 1, 'x' local c = a < b"), ["Can't compare a number with a string"]);
    assert_eq!(warnings("local t = {} local c = t >= 1"), ["Can't compare a table with a number"]);
    assert_eq!(warnings("local c = true < false"), ["Can't compare a boolean with a boolean"]);
    // One report for an expression, though both sides are wrong
    assert_eq!(warnings("local b = true local n = b + b"), ["Can't do arithmetic on a boolean"]);
    // Perhaps nil as well: nil raises an error too
    assert_eq!(warnings("local b = x and true or nil local n = b + 1"), ["Can't do arithmetic on a boolean"]);
    // Numbers, strings that may be numbers, and tables with metamethods
    assert!(warnings("local n = 1 + '2' .. 3 local m = -'4'").is_empty());
    assert!(warnings("local t = setmetatable({}, {}) local n = t + 1 local s = t .. 'x' local u = -t").is_empty());
    assert!(warnings("local s = 'ab' local t = {} local n = #s + #t").is_empty());
    assert!(warnings("local c = 1 < 2 or 'a' <= 'b' local t = {} local d = t < t").is_empty());
    // Not when it may be something that works, or isn't known
    assert!(warnings("local b = x and true or 1 local n = b + 1 local c = b < 2").is_empty());
    assert!(warnings("local n = x + 1 local c = y < 'a' local l = #z").is_empty());
    // Nor nil alone, nor a check that rules out what fails
    assert!(warnings("local v local n = v + 1 local s = v .. 'x'").is_empty());
    assert!(warnings("local v = x and 1 or 'a' if type(v) == 'number' then local c = v < 2 end").is_empty());
    // `==` never raises an error
    assert!(warnings("local c = 1 == 'a' local d = true ~= {}").is_empty());
}

#[test]
fn narrowed_fields() {
    let warnings = |source: &str| -> Vec<String> { analyse_with(source, &library()).diagnostics.iter().map(|d| d.message.clone()).collect() };
    let types = |source: &str| -> Vec<String> {
        let file = analyse_with(source, &library());
        file.field_references.iter().filter(|f| !f.write && f.name() == "m").map(|f| file.describe(f.types)).collect()
    };
    // A number or function, as `ScrollUtil.lua`'s `areaMargin`
    let margin = "local T = {} function T:Set(m) self.m = m end T:Set(5) T:Set(function() end)\n";
    assert_eq!(types(&format!("{margin}function T:Get() if type(self.m) == 'number' then return self.m end return self.m end")), ["number|function()", "number", "function()"]);
    // Only a number reaches it: past the check, nothing does
    let number = "local T = {} function T:Set(m) self.m = m end function T:Reset() end T:Set(5)\n";
    assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'number' then return self.m end return self.m(x) end")).is_empty());
    assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) ~= 'function' then return 0 end return self.m(x) end")).is_empty());
    assert!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then return self.m(x) end end")).is_empty());
    assert!(warnings(&format!("{number}function T:Get(x) return type(self.m) == 'function' and self.m(x) end")).is_empty());
    assert!(warnings(&format!("{number}function T:Get(x) assert(type(self.m) == 'function') return self.m(x) end")).is_empty());
    // Nested fields, and through a local that isn't `self`
    assert!(warnings("local t = {a = {m = 1}} if type(t.a.m) == 'function' then t.a.m() end").is_empty());
    // Unnarrowed, it's reported
    assert_eq!(warnings(&format!("{number}function T:Get(x) return self.m(x) end")), ["`m` is number, not a function"]);
    // A call or a field write may change it, so it's not narrowed past one
    let changed = |between: &str| warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then {between} return self.m(x) end end"));
    assert_eq!(changed("self:Reset()"), ["`m` is number, not a function"]);
    assert_eq!(changed("self.other = 1"), ["`m` is number, not a function"]);
    assert_eq!(changed("local o = self o.k = 1"), ["`m` is number, not a function"]);
    assert_eq!(changed("self = T"), ["`m` is number, not a function"]);
    assert!(changed("local y = x + 1").is_empty());
    assert_eq!(warnings(&format!("{number}function T:Get(x) return type(self.m) == 'function' and self:Reset() and self.m(x) end")), ["`m` is number, not a function"]);
    assert_eq!(warnings(&format!("{number}function T:Get(x) if type(self.m) ~= 'function' then return elseif self:Reset() then return end return self.m(x) end")), ["`m` is number, not a function"]);
    // Nor into a loop, whose earlier passes may change it, nor a function
    // (and after them, past the loop and the call)
    assert_eq!(changed("for i = 1, 2 do self.m(i) end"), ["`m` is number, not a function"; 2]);
    assert_eq!(changed("local function f() self.m(x) end f()"), ["`m` is number, not a function"; 2]);
    // Defining a function doesn't run it, so the narrowing holds past one
    assert!(changed("local function f() end").is_empty());
    // Nor past the branch that narrowed it
    assert_eq!(warnings(&format!("{number}function T:Get(x) if type(self.m) == 'function' then end return self.m(x) end")), ["`m` is number, not a function"]);
}

#[test]
fn and_or() {
    assert_eq!(types_of("local a = x and 1 or 's'", "a"), ["number|string"]);
    assert_eq!(types_of("local a = x or 0", "a"), ["unknown (truthy)"]);
    assert_eq!(types_of("local y = nil local a = y or 0", "a"), ["number"]);
    assert_eq!(types_of("local y = 1 local a = y and 's'", "a"), ["string"]);
    // `or` is lower precedence than `==`
    assert_eq!(types_of("local a = 1 == 2 or 's'", "a"), ["string|true"]);
    assert_eq!(types_of("local y = 's' local a = y or 0", "a"), ["string"]);
    assert_eq!(types_of("local a = nil and 1", "a"), ["nil"]);
}

#[test]
fn reassignment() {
    assert_eq!(types_of("local x = 5 x = 'a' local y = x", "x"), ["number", "string", "string"]);
}

#[test]
fn variables_flow_into_others() {
    assert_eq!(types_of("local x = 5 local y = x + 1 local z = x", "z"), ["number"]);
}

#[test]
fn shadowing() {
    assert_eq!(types_of("local x = 1 do local x = 's' end local y = x", "y"), ["number"]);
    assert_eq!(types_of("local x = 1 local x = x .. 'a' local y = x", "y"), ["string"]);
}

#[test]
fn do_block_assignment_replaces() {
    assert_eq!(types_of("local x = 1 do x = 's' end local y = x", "y"), ["string"]);
}

#[test]
fn conditional_assignment_unions() {
    assert_eq!(types_of("local x = 1 if c then x = 's' end local y = x", "y"), ["number|string"]);
    assert_eq!(types_of("local x = 1 while c do x = 's' end local y = x", "y"), ["number|string"]);
    // Every branch assigning it replaces what it held
    assert_eq!(types_of("local x if c then x = 1 else x = 's' end local y = x", "y"), ["number|string"]);
    assert_eq!(types_of("local x if c then x = 1 elseif d then x = 's' end local y = x", "y"), ["number|string|nil"]);
    // Within a branch, as anywhere, an assignment replaces it
    assert_eq!(types_of("local x = 1 if c then x = 's' local y = x end", "y"), ["string"]);
    // A branch that returns doesn't go on past the `if`
    assert_eq!(types_of("local x = 1 if c then x = 's' return end local y = x", "y"), ["number"]);
    // What a condition does happens whichever way it goes
    let source = "local x = 1 local function g() x = 's' return true end";
    assert_eq!(types_of(&format!("{source} if g() then end local y = x"), "y"), ["number|string"]);
    assert_eq!(types_of(&format!("{source} if c then elseif g() then x = true end local y = x"), "y"), ["number|string|true"]);
    // Nor what a loop's pass leaves, as it may run again
    assert_eq!(types_of("local x = 1 while c do if d then x = 's' end end local y = x", "y"), ["number|string"]);
}

#[test]
fn loops_carry_types_to_the_next_iteration() {
    assert_eq!(types_of("local x = 1 while c do local y = x x = 's' end", "y"), ["number|string"]);
    assert_eq!(types_of("local x = 1 repeat local y = x x = 's' until x", "y"), ["number|string"]);
    assert_eq!(types_of("local x = 1 for i = 1, 2 do local y = x x = {} end", "y"), ["number|table"]);
    assert_eq!(types_of("local x = 1 for k in pairs(t) do local y = x x = {} end", "y"), ["number|table"]);
    // The `while` condition is checked again after each iteration
    assert_eq!(types_of("local x = 1 while x do x = nil end", "x"), ["number", "number?", "nil"]);
    // Unchanged types need no extra pass
    assert_eq!(types_of("local x = 1 while c do local y = x end", "y"), ["number"]);
}

#[test]
fn loop_carried_chains_converge() {
    let source = "local a, b, c = 1, 1, 1 while w do local y = c c = b b = a a = 's' end";
    assert_eq!(types_of(source, "y"), ["number|string"]);
    let nested = "local x = 1 while p do while q do local y = x x = 's' end x = {} end";
    assert_eq!(types_of(nested, "y"), ["number|string|table"]);
}

/// Each pass moves `string` one step down the chain, so this needs more
/// passes than LOOP_PASSES_BEFORE_WIDENING. The variables still changing are
/// widened to unknown, which still includes every real type
#[test]
fn long_loop_chains_widen() {
    let names: Vec<String> = (0..12).map(|i| format!("v{i}")).collect();
    let mut source = format!("local {} = {} while w do local y = v11 ", names.join(", "), vec!["1"; 12].join(", "));
    for i in (1..12).rev() {
        source += &format!("v{i} = v{} ", i - 1);
    }
    source += "v0 = 's' end";
    let file = analyse(&source);
    let y = file.references.iter().find(|r| file.binding(r).name.as_ref() == "y").unwrap();
    assert!(y.types.set.contains(TypeSet::NUMBER | TypeSet::STRING), "{}", y.types.set);
    assert_eq!(y.types.set, TypeSet::UNKNOWN);
    assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "y").count(), 1);
}

#[test]
fn loop_passes_leave_one_set_of_records() {
    let file = analyse("local x = 1 while c do local y = x x = 's' print(Global) end");
    assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "y").count(), 1);
    assert_eq!(file.bindings.iter().filter(|b| b.name.as_ref() == "Global").count(), 1);
    assert_eq!(file.references.iter().filter(|r| file.binding(r).name.as_ref() == "x").count(), 3);
    assert_eq!(file.references.iter().filter(|r| file.binding(r).name.as_ref() == "print").count(), 1);
}

#[test]
fn locals_inside_branches_replace() {
    assert_eq!(types_of("if c then local x = 1 x = 's' local y = x end", "y"), ["string"]);
}

#[test]
fn upvalues_see_every_assignment() {
    let source = "local x = 1 local function f() return x end x = 's'";
    assert_eq!(types_of(source, "x"), ["number", "number|string", "string"]);
}

#[test]
fn multiple_values() {
    assert_eq!(types_of("local a, b = 1", "b"), ["nil"]);
    assert_eq!(types_of("local a, b = f()", "b"), ["unknown"]);
    assert_eq!(types_of("local a, b = ...", "b"), ["unknown"]);
    assert_eq!(types_of("local a, b = 1, 's'", "b"), ["string"]);
}

#[test]
fn globals() {
    assert_eq!(types_of("Foo = {} local y = Foo", "y"), ["table"]);
    assert_eq!(types_of("local y = Bar", "y"), ["unknown"]);
    assert_eq!(types_of("function Baz() end local y = Baz", "y"), ["function()"]);
    let file = analyse("Foo = 1");
    assert_eq!(file.bindings[0].kind, BindingKind::Global);
}

#[test]
fn loops() {
    assert_eq!(types_of("for i = 1, 3 do local j = i end", "j"), ["number"]);
    assert_eq!(types_of("for k, v in ipairs(t) do end", "k"), ["number"]);
    assert_eq!(types_of("for k, v in ipairs(t) do end", "v"), ["unknown (not nil)"]);
    assert_eq!(types_of("for k, v in next, t do end", "v"), ["unknown"]);
    assert_eq!(types_of("local ipairs = pairs for k in ipairs(t) do end", "k"), ["unknown (not nil)"]);
}

#[test]
fn repeat_condition_sees_block_locals() {
    let file = analyse("repeat local q = 1 until q");
    let qs: Vec<_> = file.references.iter().filter(|r| file.binding(r).name.as_ref() == "q").collect();
    assert_eq!(qs.len(), 2);
    assert_eq!(qs[0].binding, qs[1].binding);
    assert_eq!(file.binding(qs[1]).kind, BindingKind::Local);
}

#[test]
fn parameters_and_self() {
    assert_eq!(types_of("function T:m(a) return self, a end", "self"), ["table"]);
    assert_eq!(types_of("function T:m(a) return self, a end", "a"), ["unknown", "unknown"]);
    assert_eq!(types_of("local function f(a) end", "f"), ["function(a)"]);
}

#[test]
fn field_names_are_not_variables() {
    assert!(types_of("local t = {a = 1}", "a").is_empty());
    assert!(types_of("local t = {} local y = t.x", "x").is_empty());
    assert!(types_of("local t = {} t.x = 1", "x").is_empty());
    assert_eq!(types_of("local k = 1 local t = {[k] = 1}", "k").len(), 2);
    assert_eq!(types_of("local t = {} local y = t[t]", "t").len(), 3);
    assert_eq!(types_of("local t = {} local y = t.x", "t"), ["table", "table"]);
}

#[test]
fn calls_resolve_arguments() {
    assert_eq!(types_of("local a = 1 print(a)", "a"), ["number", "number"]);
    assert_eq!(types_of("local a = 1 local o = {} o:m(a).z(a)", "a").len(), 3);
}

#[test]
fn call_results() {
    assert_eq!(types_of("local function f() return 1 end local x = f()", "x"), ["number"]);
    assert_eq!(types_of("local f = function() return true end local g = f local x = g()", "x"), ["true"]);
    assert_eq!(types_of("function F() return {} end local x = F()", "x"), ["table"]);
    assert_eq!(types_of("local function f() return 1 end local x = f() .. 'a'", "x"), ["string"]);
    assert_eq!(types_of("local function f() return 's' end local x = f() or 1", "x"), ["string"]);
}

#[test]
fn multiple_results() {
    let source = "local function f() return 1, 's' end local a, b, c = f()";
    assert_eq!(types_of(source, "a"), ["number"]);
    assert_eq!(types_of(source, "b"), ["string"]);
    assert_eq!(types_of(source, "c"), ["nil"]);
    // Only the last expression expands
    assert_eq!(types_of("local function f() return 1, 's' end local a, b = f(), true", "b"), ["true"]);
    assert_eq!(types_of("local function f() return 1, 's' end local a, b = (f())", "b"), ["nil"]);
    assert_eq!(types_of("local function f() return 1, 's' end local function g() return f() end local a, b = g()", "b"), ["string"]);
    // `...` is what every call passed: nothing here, so `a` is nil
    assert_eq!(types_of("local function f(...) return ... end local a = f()", "a"), ["nil"]);
    assert_eq!(types_of("local function f(...) return ... end local a, b = f(1, 's')", "b"), ["string"]);
    assert_eq!(types_of("local function f() return 1, ... end local a, b = f()", "b"), ["unknown"]);
}

#[test]
fn returns_from_every_path() {
    assert_eq!(types_of("local function f(x) if x then return 1 end return 's' end local y = f()", "y"), ["number|string"]);
    // Falling off the end returns nothing
    assert_eq!(types_of("local function f(x) if x then return 1 end end local y = f()", "y"), ["number?"]);
    assert_eq!(types_of("local function f(x) if x then return 1 else return 2 end end local y = f()", "y"), ["number"]);
    assert_eq!(types_of("local function f(x) if x then return 1 elseif z then return 2 end end local y = f()", "y"), ["number?"]);
    assert_eq!(types_of("local function f() do return 1 end end local y = f()", "y"), ["number"]);
    assert_eq!(types_of("local function f() while c do return 1 end end local y = f()", "y"), ["number?"]);
    assert_eq!(types_of("local function f() end local y = f()", "y"), ["nil"]);
    assert_eq!(types_of("local function f() return end local y = f()", "y"), ["nil"]);
}

#[test]
fn calls_to_upvalues_and_later_functions() {
    // The usual addon shape: a local helper called from a method
    let source = "local function helper() return {} end function Addon:Load() local t = helper() end";
    assert_eq!(types_of(source, "t"), ["table"]);
    // Defined further down the file
    let source = "local function a() return B() end function B() return 's' end local y = a()";
    assert_eq!(types_of(source, "y"), ["string"]);
    // Reassigned later, so the closure could call either
    let source = "local function g() return 1 end local function f() return g() end g = function() return 's' end local y = f()";
    assert_eq!(types_of(source, "y"), ["number|string"]);
}

#[test]
fn string_literals() {
    let _shown = LiteralsShown::shown();
    assert_eq!(types_of("local s = c and 'LEFT' or \"RIGHT\"", "s"), ["\"LEFT\"|\"RIGHT\""]);
    assert_eq!(types_of("local s = c and 'a' or nil", "s"), ["\"a\"?"]);
    // Written either way, the same string
    assert_eq!(types_of("local s = c and 'a' or \"a\"", "s"), ["\"a\""]);
    // What can't be written out plainly is just a string
    assert_eq!(types_of("local s = 'a\\nb'", "s"), ["string"]);
    assert_eq!(types_of("local s = [[a]]", "s"), ["string"]);
    assert_eq!(types_of(&format!("local s = '{}'", "x".repeat(65)), "s"), ["string"]);
    // Joining makes any string
    assert_eq!(types_of("local s = 'a' .. 'b'", "s"), ["string"]);
    // More than are kept apart: any string
    let choice: String = (0..9).map(|i| format!("if c{i} then s = 's{i}' end ")).collect();
    assert_eq!(types_of(&format!("local s = 'x' {choice} local t = s"), "t"), ["string"]);
    // Through fields and parameters
    assert_eq!(types_of("local t = {} t.kind = 'a' t.kind = 'b' local k = t.kind", "k"), ["\"a\"|\"b\""]);
    assert_eq!(types_of("local function f(p) return p end f('TOP') f('BOTTOM')", "p"), ["\"BOTTOM\"|\"TOP\""; 2]);
}

#[test]
fn narrowing_by_string_equality() {
    let _shown = LiteralsShown::shown();
    let source = "local s = c and 'a' or (d and 'b' or 'c') if s == 'a' then local x = s else local y = s end";
    assert_eq!(types_of(source, "x"), ["\"a\""]);
    assert_eq!(types_of(source, "y"), ["\"b\"|\"c\""]);
    let source = "local s = c and 'a' or 'b' if s ~= 'a' then local x = s end";
    assert_eq!(types_of(source, "x"), ["\"b\""]);
    // Of anything, equal to the string is that string, unequal still anything
    let source = "local function f(p) if p == 'on' then local x = p else local y = p end end Unknown(f)";
    assert_eq!(types_of(source, "x"), ["\"on\""]);
    assert_eq!(types_of(source, "y"), ["unknown"]);
    // The only string it may be, ruled out
    let source = "local s = c and 'a' or 1 if s ~= 'a' then local x = s end";
    assert_eq!(types_of(source, "x"), ["number"]);
}

#[test]
fn several_functions() {
    let source = "local function f(a) return 1 end local function g(a) return 's' end
        local h = c and f or g local y = h(1)";
    assert_eq!(types_of(source, "h"), ["(function(a) -> number)|(function(a) -> string)"; 2]);
    assert_eq!(types_of(source, "y"), ["number|string"]);
    // Optional together
    assert_eq!(types_of("local function f() return 1 end local function g() end local h = c and (d and f or g) or nil local y = h", "h"),
        ["((function() -> number)|(function()))?"; 2]);
    // Given to one field at different times
    let source = "local function f() return 1 end local function g() return 's' end
        local t = {} t.cb = f t.cb = g local y = t.cb()";
    assert_eq!(types_of(source, "y"), ["number|string"]);
    // Through `pcall`
    let source = "local function f() return 1 end local function g() return true end
        local h = c and f or g local ok, y = xpcall(h, function() return true end)";
    assert_eq!(library_types_of(source, "y")[0], "number|true");
    // More than are kept apart: unknown
    let functions: String = (0..9).map(|i| format!("local function f{i}() return {i} end ")).collect();
    let choice: String = (0..9).map(|i| format!("if c{i} then h = f{i} end ")).collect();
    let source = format!("{functions} local h {choice} local y = h()");
    assert_eq!(types_of(&source, "y"), ["unknown"]);
}

#[test]
fn several_functions_each_get_arguments() {
    // Called only through the field, so each gets what's passed
    let source = "local t = {} t.cb = function(x) return x end t.cb = function(y) return y end t.cb(1)";
    assert_eq!(types_of(source, "x"), ["number"; 2]);
    assert_eq!(types_of(source, "y"), ["number"; 2]);
    // Read other than to call, through a variable: either may be called anywhere
    let source = "local t = {} t.cb = function(x) return x end t.cb = function(y) return y end local h = t.cb h(1) Unknown(h)";
    assert_eq!(types_of(source, "x"), ["unknown"; 2]);
    assert_eq!(types_of(source, "y"), ["unknown"; 2]);
}

#[test]
fn fields_only_tested_dont_let_functions_escape() {
    // `x.track` may be the function stored as `t.track`, but testing it
    // calls nothing and passes it nowhere
    let source = "local t = {} t.track = function(item) return item end
        local function use(x) if x.track then end if not x.track then end end
        use(Unknown) t.track(1)";
    assert_eq!(types_of(source, "item"), ["number"; 2]);
    // Given on, though, it could be called anywhere
    let source = "local t = {} t.track = function(item) return item end
        local function use(x) local f = x.track and x.track Unknown(f) end
        use(Unknown) t.track(1)";
    assert_eq!(types_of(source, "item"), ["unknown"; 2]);
}

#[test]
fn functions_read_through_computed_keys_escape() {
    let source = "local t = {} t['a'] = function(x) return x end local k = c and 'a' or 'b' t.a(1)";
    assert_eq!(types_of(&format!("{source} Unknown(t[k])"), "x"), ["unknown"; 2]);
    assert_eq!(types_of(&format!("{source} local g = t[k] Unknown(g)"), "x"), ["unknown"; 2]);
    // Called right there, it's a call seen
    assert_eq!(types_of(&format!("{source} t[k](2)"), "x"), ["number"; 2]);
}

#[test]
fn methods_of_computed_entries_arent_each_called() {
    // `self:Fire()` may be a method the table gets elsewhere (a mixin the
    // analysis can't see), not one of those stored under computed keys
    let source = "local methods = {
            SetValues = function(self, step) self.step = step end,
            SetText = function(self, text) self.text = text end,
        }
        local function Constructor()
            local widget = {}
            for name, f in pairs(methods) do widget[name] = f end
            widget:Fire('OnChanged')
            local s = widget.step
        end";
    // What any entry is, as before: not `string`, as calling `SetValues`
    // for `Fire` would make it
    assert_eq!(library_types_of(source, "s"), ["function?"]);
}

#[test]
fn unknown_calls() {
    assert_eq!(types_of("local y = print()", "y"), ["unknown"]);
    assert_eq!(types_of("local function f(g) return g() end local y = f()", "y"), ["unknown"]);
    // Either of two functions: what either returns
    assert_eq!(types_of("local f = c and function() return 1 end or function() return 2 end local y = f()", "y"), ["number"]);
    // Recursion converges on the first walk's unknown
    let source = "local function f(n) if n then return f(n) end return 1 end local y = f()";
    assert!(analyse(source).references.iter().any(|r| r.types.set.contains(TypeSet::NUMBER)));
}

#[test]
fn function_values_through_and_or() {
    // `and` passes a falsy left side through, but calling one of those fails
    assert_eq!(types_of("local function f() return 1 end local g = x and f local y = g()", "y"), ["number"]);
    assert_eq!(types_of("local function f() return 1 end local g = f or nil local y = g()", "y"), ["number"]);
    assert_eq!(types_of("local function f() return 1 end local g = nil local h = g or f local y = h()", "y"), ["number"]);
}

#[test]
fn signatures() {
    assert_eq!(types_of("local function f(a, ...) return 1, 's' end", "f"), ["function(a, ...) -> number, string"]);
    assert_eq!(types_of("local function f() return g() end", "f"), ["function() -> ..."]);
    assert_eq!(types_of("local function f(x) if x then return 1 end end", "f"), ["function(x) -> number?"]);
    assert_eq!(types_of("local function f() return 1 end local g = c and f", "g"), ["false|(function() -> number)|nil"]);
    assert_eq!(types_of("local function f() return f end", "f"), ["function() -> function", "function() -> function"]);
}

fn builtins() -> Arc<Definitions> {
    Arc::new(Definitions::from_json(r#"{"functions": {
        "tostring": {"parameters": ["value"], "returns": ["string"]},
        "tonumber": {"parameters": ["value", "base: number|nil"], "returns": ["number|nil"]},
        "select": {"parameters": ["index", "..."], "returns": ["unknown"], "rest": "unknown"},
        "floor": {"parameters": ["x: number"], "returns": ["number"]},
        "max": {"parameters": ["x: number", "...: number"], "returns": ["number"]},
        "strlen": {"parameters": ["s: string"], "returns": ["number"]}
    }, "tables": {
        "string": {"functions": {"format": {"parameters": ["format: string", "..."], "returns": ["string"]},
            "rep": {"parameters": ["s: string", "n: number"], "returns": ["string"]}}},
        "a": {"tables": {"b": {"functions": {"c": {"returns": ["table"]}}}}}
    }}"#).unwrap())
}

fn builtin_types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse_with(source, &builtins()), name)
}

#[test]
fn preset_globals() {
    assert_eq!(builtin_types_of("local x = tostring(1)", "x"), ["string"]);
    assert_eq!(builtin_types_of("local x = tonumber('1')", "x"), ["number?"]);
    assert_eq!(builtin_types_of("local x = tostring", "x"), ["function(value) -> string"]);
    // Narrowed through, in the main chunk as in a function
    assert_eq!(builtin_types_of("local x = string.format and string.format('%d', 1)", "x"), ["string"]);
    assert_eq!(builtin_types_of("local x, y = select(2, 'a', 'b')", "x"), ["string"]);
    assert_eq!(builtin_types_of("local x, y = select(2, 'a', 'b')", "y"), ["nil"]);
    assert_eq!(builtin_types_of("local x, y = select(2, f())", "y"), ["unknown"]);
    assert_eq!(builtin_types_of("local x = undefined()", "x"), ["unknown"]);
    // Inside functions too, where globals are read like upvalues
    assert_eq!(builtin_types_of("function F() local x = tostring(1) end", "x"), ["string"]);
}

#[test]
fn localised_presets() {
    // The usual WoW idiom of caching globals in locals
    let source = "local tostring, tonumber = tostring, tonumber function F() local x = tostring(1) end";
    assert_eq!(builtin_types_of(source, "x"), ["string"]);
}

#[test]
fn preset_fields() {
    assert_eq!(builtin_types_of("local x = string.format('%d', 1)", "x"), ["string"]);
    assert_eq!(builtin_types_of("local f = string.format local x = f()", "x"), ["string"]);
    assert_eq!(builtin_types_of("local x = a.b.c()", "x"), ["table"]);
    assert_eq!(builtin_types_of("local x = string.nope()", "x"), ["unknown"]);
    // Members are fields of the table, so they work through other variables
    assert_eq!(builtin_types_of("local s = string local x = s.format('%d', 1)", "x"), ["string"]);
    assert_eq!(builtin_types_of("local string = string function F() local x = string.format() end", "x"), ["string"]);
    assert_eq!(builtin_types_of("local b = a.b local x = b.c()", "x"), ["table"]);
    // A namespace is shown as a plain table
    assert_eq!(builtin_types_of("local s = string", "s"), ["table"]);
    assert_eq!(builtin_types_of("local x = string['format']()", "x"), ["string"]);
    // A local named `string` isn't the library
    assert_eq!(builtin_types_of("local string = {} local x = string.format()", "x"), ["unknown"]);
}

#[test]
fn files_can_replace_presets() {
    assert_eq!(builtin_types_of("function tostring() return 1 end local x = tostring()", "x"), ["number"]);
    // Until the files assign it, it's what the preset says
    assert_eq!(builtin_types_of("local x = tostring() tostring = nil", "x"), ["string"]);
    assert_eq!(builtin_types_of("tostring = nil local x = tostring", "x"), ["nil"]);
    assert_eq!(builtin_types_of("local function tostring() return {} end local x = tostring()", "x"), ["table"]);
    assert_eq!(builtin_types_of("string = {} local x = string.format()", "x"), ["unknown"]);
}

fn enums() -> Arc<Definitions> {
    Arc::new(Definitions::from_json(r#"{
        "types": {"Enum.Quality": {"base": "number"}, "Enum.Layer": {"base": "number"}},
        "tables": {"Enum": {"tables": {
            "Quality": {"values": {"Poor": "Enum.Quality", "Good": "Enum.Quality"}},
            "Layer": {"values": {"Top": "Enum.Layer"}}
        }}},
        "functions": {"GetQuality": {"returns": ["Enum.Quality|nil"]}}
    }"#).unwrap())
}

fn enum_types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse_with(source, &enums()), name)
}

#[test]
fn enum_members() {
    assert_eq!(enum_types_of("local q = Enum.Quality.Poor", "q"), ["Enum.Quality"]);
    assert_eq!(enum_types_of("local q = c and Enum.Quality.Poor or Enum.Quality.Good", "q"), ["Enum.Quality"]);
    assert_eq!(enum_types_of("local Q = Enum.Quality local q = Q.Good", "q"), ["Enum.Quality"]);
    assert_eq!(enum_types_of("local q = GetQuality()", "q"), ["Enum.Quality?"]);
    // The enum's own table, and its parent, are plain tables
    assert_eq!(enum_types_of("local Q = Enum.Quality", "Q"), ["table"]);
}

#[test]
fn enums_mix_into_plain_numbers() {
    // Arithmetic gives a plain number
    assert_eq!(enum_types_of("local n = Enum.Quality.Poor + 1", "n"), ["number"]);
    // So does mixing with a plain number, or another enum
    assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or 5", "n"), ["number"]);
    assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or Enum.Layer.Top", "n"), ["number"]);
    // Other types alongside keep the enum
    assert_eq!(enum_types_of("local n = c and Enum.Quality.Poor or 's'", "n"), ["Enum.Quality|string"]);
}

#[test]
fn mandated_parameters() {
    assert_eq!(builtin_types_of("local function f(x) return floor(x) end", "x"), ["number", "number"]);
    // A string's method takes the string first, so `x` isn't the format
    assert_eq!(builtin_types_of("local function f(x) return ('%d'):format(x) end f(1)", "x"), ["number", "number"]);
    assert_eq!(builtin_types_of("local function f(x) return ('a'):rep(x) end", "x"), ["number", "number"]);
    assert_eq!(builtin_types_of("local function f(s) return string.format(s) end", "s"), ["string", "string"]);
    // Varargs can require a type too
    assert_eq!(builtin_types_of("local function f(a, b) return max(a, b) end", "b"), ["number", "number"]);
    // Globals and methods can be called from anywhere, but still have to obey
    assert_eq!(builtin_types_of("function T:m(x) return strlen(x) end", "x"), ["string", "string"]);
    assert_eq!(builtin_types_of("function F(x) local y = floor(x) end", "x"), ["number", "number"]);
    // What the body requires beats what callers pass
    assert_eq!(builtin_types_of("local function f(x) return floor(x) end f('1')", "x"), ["number", "number"]);
    // Any of several tables meets a requirement of a table, so they're kept
    let source = "local a = { x = 1 } local b = { y = 2 } local u = c and a or b local function g(t) local k = next(t) local v = t.x end g(u)";
    assert_eq!(library_types_of(source, "v")[0], "number");
}

#[test]
fn requirements_must_always_run() {
    let unknown = ["unknown", "unknown"];
    assert_eq!(builtin_types_of("function F(x) if c then floor(x) end end", "x"), unknown);
    assert_eq!(builtin_types_of("function F(x) if type(x) == 'number' then return floor(x) end end", "x")[..1], ["unknown"]);
    assert_eq!(builtin_types_of("function F(x) while c do floor(x) end end", "x"), unknown);
    assert_eq!(builtin_types_of("function F(x) return c and floor(x) end", "x"), unknown);
    assert_eq!(builtin_types_of("function F(x) return floor(x) or 1 end", "x"), ["number", "number"]);
    assert_eq!(builtin_types_of("function F(x) if c then elseif floor(x) then end end", "x"), unknown);
    assert_eq!(builtin_types_of("function F(x) if floor(x) then end end", "x"), ["number", "number"]);
    // A closure might never be called
    assert_eq!(builtin_types_of("function F(x) return function() floor(x) end end", "x"), unknown);
    // After reassignment, the call isn't seeing the caller's value
    assert_eq!(builtin_types_of("function F(x) x = x or 1 floor(x) end", "x")[0], "unknown");
    // Optional parameters and ones taking anything don't require a type
    assert_eq!(builtin_types_of("function F(x) tonumber('1', x) tostring(x) end", "x"), ["unknown"; 3]);
}

#[test]
fn conflicting_requirements_are_ignored() {
    assert_eq!(builtin_types_of("function F(x) floor(x) strlen(x) end", "x"), ["unknown"; 3]);
}

#[test]
fn requirements_pass_through_user_functions() {
    let source = "local function g(n) return floor(n) end function F(x) g(x) end";
    assert_eq!(builtin_types_of(source, "x"), ["number", "number"]);
    // Even when the required function is defined further down
    let source = "function F(x) G(x) end function G(n) return floor(n) end";
    assert_eq!(builtin_types_of(source, "x"), ["number", "number"]);
}

#[test]
fn parameters_from_call_sites() {
    assert_eq!(types_of("local function f(x) return x end f(1) f('a')", "x"), ["number|string", "number|string"]);
    assert_eq!(types_of("local function f(x) return x end local y = f(1)", "y"), ["number"]);
    assert_eq!(types_of("local function f(a, b) end f(1)", "b"), ["nil"]);
    assert_eq!(types_of("local f = function(x) end f({})", "x"), ["table"]);
    // Calls from inside other functions count too
    assert_eq!(types_of("local function f(x) end function G() f(true) end", "x"), ["true"]);
    // Never called, so there's nothing to go on
    assert_eq!(types_of("local function f(x) end", "x"), ["unknown"]);
    // Arguments expand like any other expression list
    assert_eq!(types_of("local function g() return 1, 's' end local function f(a, b) end f(g())", "b"), ["string"]);
}

#[test]
fn call_site_types_chain_through_functions() {
    let source = "local function c(z) end local function b(y) c(y) end local function a(x) b(x) end a(1)";
    assert_eq!(types_of(source, "z"), ["number"]);
    let source = "local function f(n) if n > 0 then return f(n - 1) end return 0 end f(10)";
    assert_eq!(types_of(source, "n")[0], "number");
}

#[test]
fn escaping_functions_have_unknown_parameters() {
    // Stored, passed on, returned, or global: other callers could pass anything
    assert_eq!(types_of("local function f(x) end f(1) T.cb = f", "x"), ["unknown"]);
    assert_eq!(types_of("local function f(x) end f(1) pcall(f, 's')", "x"), ["unknown"]);
    assert_eq!(types_of("local function f(x) end f(1) local g = f", "x"), ["unknown"]);
    assert_eq!(types_of("local function f(x) end f(1) return f", "x"), ["unknown"]);
    assert_eq!(types_of("function F(x) end F(1) local g = F", "x"), ["unknown"]);
    assert_eq!(types_of("local t = {} function t.f(x) end t.f(1) pcall(t.f)", "x"), ["unknown"]);
    assert_eq!(types_of("pcall(function(x) end, 1)", "x"), ["unknown"]);
    // Holding two different functions, so a call doesn't say which one ran
    assert_eq!(types_of("local f = function(x) end f = function(y) end f(1)", "x"), ["unknown"]);
}

#[test]
fn typed_signatures() {
    assert_eq!(builtin_types_of("local function f(x, y) return floor(x) end f(1, 's')", "f")[0], "function(x: number, y: string) -> number");
    assert_eq!(builtin_types_of("local f = string.format", "f"), ["function(format: string, ...) -> string"]);
    assert_eq!(builtin_types_of("local f = max", "f"), ["function(x: number, ...: number) -> number"]);
}

#[test]
fn optional_types() {
    assert_eq!(types_of("local x = c and 1 or nil", "x"), ["number?"]);
    // More than one other type keeps `|nil`, as `number|string?` would be ambiguous
    assert_eq!(types_of("local x = c and 1 or (d and 's' or nil)", "x"), ["number|string|nil"]);
    assert_eq!(types_of("local x = nil", "x"), ["nil"]);
    assert_eq!(types_of("local x = c and f or nil local y = x", "y"), ["unknown (not false)"]);
    assert_eq!(types_of("local function f() return 1 end local g = c and f or nil", "g"), ["(function() -> number)?"]);
}

fn objects() -> Arc<Definitions> {
    Arc::new(Definitions::from_json(r#"{
        "types": {"Frame": {"base": "table"}, "Texture": {"base": "table"}, "Curve": {"base": "userdata"}},
        "functions": {
            "CreateFrame": {"parameters": ["frameType: string"], "returns": ["Frame"]},
            "GetMouseFocus": {"returns": ["Frame|nil"]},
            "CreateCurve": {"returns": ["Curve"]},
            "GetTexture": {"returns": ["Texture"]},
            "SetPortrait": {"parameters": ["texture: Texture"]}
        }
    }"#).unwrap())
}

fn object_types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse_with(source, &objects()), name)
}

#[test]
fn named_object_types() {
    assert_eq!(object_types_of("local f = CreateFrame('Frame')", "f"), ["Frame"]);
    assert_eq!(object_types_of("local f = GetMouseFocus()", "f"), ["Frame?"]);
    assert_eq!(object_types_of("local c = CreateCurve()", "c"), ["Curve"]);
    assert_eq!(object_types_of("local f = CreateFrame", "f"), ["function(frameType: string) -> Frame"]);
    // Still a table, so table operations keep it
    assert_eq!(object_types_of("local f = GetMouseFocus() or CreateFrame('Frame')", "f"), ["Frame"]);
}

#[test]
fn mixing_named_types() {
    // Different named types are either, as are a named type and a table
    // made in the files
    assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or GetTexture()", "f"), ["Texture|Frame"]);
    assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or {}", "f"), ["Frame|table"]);
    // Which keeps the table's fields, and what's written through either
    let source = "local t = { y = 's' } local f = c and CreateFrame('Frame') or t f.x = 1 local x, y = t.x, f.y";
    assert_eq!(object_types_of(source, "x"), ["number"]);
    // The frame may have one set where that can't be seen
    assert_eq!(object_types_of(source, "y"), ["unknown"]);
    assert_eq!(object_types_of("local f = c and CreateFrame('Frame') or 1", "f"), ["number|Frame"]);
    assert_eq!(object_types_of("local f = CreateFrame('Frame') if c then f = CreateFrame('Frame') end local g = f", "g"), ["Frame"]);
}

#[test]
fn named_parameter_requirements() {
    assert_eq!(object_types_of("function F(t) SetPortrait(t) end", "t"), ["Texture", "Texture"]);
    assert_eq!(object_types_of("local function f(t) end f(CreateFrame('Frame'))", "t"), ["Frame"]);
}

#[test]
fn array_entries() {
    assert_eq!(types_of("local t = { 'a', 'b' }", "t"), ["string[]"]);
    assert_eq!(types_of("local t = { 'a', 1 }", "t"), ["(number|string)[]"]);
    assert_eq!(types_of("local t = { 'a' } local a = t[1]", "a"), ["string?"]);
    assert_eq!(types_of("local t = { 'a' } local i = 1 local a = t[i]", "a"), ["string?"]);
    // Nothing known: still unknown rather than nil
    assert_eq!(types_of("local t = {} local a = t[1]", "a"), ["unknown"]);
    // Nested tables show as plain tables
    assert_eq!(types_of("local t = { { 1 } } local a = t[1][1]", "a"), ["number?"]);
    assert_eq!(types_of("local t = { { 1 } }", "t"), ["table[]"]);
    // A table holding itself
    assert_eq!(types_of("local t = {} t[1] = t", "t")[0], "table[]");
    // Unknown values are just unknown, as nothing stored is nil
    assert_eq!(types_of("local function f(x) local t = { x } end", "t"), ["unknown[]"]);
}

#[test]
fn array_items_are_read() {
    // `{ x }` is an item, so `x` is read, not taken as a key
    assert_eq!(types_of("local x = 1 local t = { x }", "x"), ["number", "number"]);
}

#[test]
fn computed_keys() {
    assert_eq!(types_of("local t = {} t[1] = 'a'", "t"), ["string[]", "string[]"]);
    assert_eq!(types_of("local t = { [true] = 1 }", "t"), ["table<true, number>"]);
    assert_eq!(types_of("local k = 'x' .. 1 local t = {} t[k] = 1", "t"), ["table<string, number>", "table<string, number>"]);
    // `t["name"]` is the field `name`
    assert_eq!(types_of("local t = {} t['n'] = 1 local a = t.n", "a"), ["number"]);
    assert_eq!(types_of("local t = { n = 1 } local a = t['n']", "a"), ["number"]);
    // A string key could be any named field
    assert_eq!(types_of("local t = { n = 1 } local k = 'x' .. 1 local a = t[k]", "a"), ["number?"]);
    // And a field not named could be any entry under a string key
    assert_eq!(types_of("local k = 'x' .. 1 local t = {} t[k] = 'v' local a = t.n", "a"), ["string?"]);
    assert_eq!(types_of("local t = {} for k in pairs({ a = 1 }) do t[k] = 'v' end local a = t.a", "a"), ["string?"]);
    // Number keys only: no field of a name is among them
    assert_eq!(types_of("local t = {} t[1] = 'a' local a = t.n", "a"), ["unknown"]);
    // Storing nil removes
    assert_eq!(types_of("local t = {} t[1] = nil", "t"), ["table", "table"]);
    // Only named fields: still a plain table
    assert_eq!(types_of("local t = { n = 1 }", "t"), ["table"]);
    // Mixed: named fields are string keys
    assert_eq!(types_of("local t = { n = 1, 'a' }", "t"), ["table<number|string, number|string>"]);
}

/// The repository's definitions, for the real Lua and WoW libraries.
/// Loaded once, as they're large
fn library() -> Arc<Definitions> {
    static LIBRARY: std::sync::OnceLock<Arc<Definitions>> = std::sync::OnceLock::new();
    LIBRARY.get_or_init(|| Arc::new(crate::definitions::load(None))).clone()
}

fn library_types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse_with(source, &library()), name)
}

#[test]
fn table_insert() {
    assert_eq!(library_types_of("local t = {} table.insert(t, 'a')", "t")[0], "string[]");
    assert_eq!(library_types_of("local t = {} table.insert(t, 1, 'a')", "t")[0], "string[]");
    assert_eq!(library_types_of("local t = {} tinsert(t, true)", "t")[0], "true[]");
    // A local named `table` isn't the library
    assert_eq!(library_types_of("local table = { insert = function() end } local t = {} table.insert(t, 'a')", "t")[0], "table");
}

#[test]
fn tables_a_field_is_reset_to() {
    // A field given a new table again (emptied) is any of them: what's
    // put in it is in whichever it is, so taking it back out gives that
    let source = "local P = {} function P:Init() self.list = {} end
        function P:Add() table.insert(self.list, true) self.list.count = 1 end
        function P:Reset() self.list = {} end
        function P:Take() local x, c = table.remove(self.list), self.list.count
            for _, v in ipairs(self.list) do local each = v end end";
    assert_eq!(library_types_of(source, "x")[0], "true?");
    assert_eq!(library_types_of(source, "each")[0], "true");
    assert_eq!(library_types_of(source, "c")[0], "number");
    // One without the field doesn't make it unknown: Auctionator's
    // `Constants = {}`, then `Auctionator.Constants = { IsForever = ... }`
    let source = "local A = { C = {} } A.C = { F = true } local x = A.C.F";
    assert_eq!(types_of(source, "x"), ["true"]);
    assert_eq!(types_of("local A = { C = { G = 1 } } A.C = { F = true } local x, y = A.C.F, A.C.G", "y"), ["number"]);
}

#[test]
fn many_records_of_one_constructor() {
    // More records than are kept apart: any of the constructor's
    // elements, with each one's fields
    let records: Vec<String> = (0..12).map(|i| format!("K{i} = {{ key = 'k{i}'{} }}", if i == 3 { ", refresh = 1" } else { "" })).collect();
    let source = format!("local s = {{ {} }} for _, d in pairs(s) do local x, r = d.key, d.refresh d.seen = true end local z = s.K2.seen", records.join(", "));
    assert_eq!(library_types_of(&source, "x")[0], "string");
    assert_eq!(library_types_of(&source, "r")[0], "number");
    // What's written through any of them, each may have
    assert_eq!(library_types_of(&source, "z")[0], "true");
    // Array items too, joined with others put in after
    let items: Vec<&str> = (0..12).map(|_| "{ text = 's' }").collect();
    let source = format!("local lines = {{ {} }} table.insert(lines, {{ text = 1 }}) for _, l in ipairs(lines) do local t = l.text end", items.join(", "));
    assert_eq!(library_types_of(&source, "t")[0], "number|string");
}

#[test]
fn loops_over_what_functions_return() {
    // `return ipairs(self.list)`: a loop over a call gets what the list holds
    let source = "local P = {} function P:Init() self.list = {} end
        function P:Add() table.insert(self.list, 'a') end
        function P:Enumerate() return ipairs(self.list) end
        function P:Use() for i, v in self:Enumerate() do local index, each = i, v end end";
    assert_eq!(library_types_of(source, "each")[0], "string");
    assert_eq!(library_types_of(source, "index")[0], "number");
}

#[test]
fn table_create_makes_a_table() {
    assert_eq!(library_types_of("local t = table.create(2) t[1] = 's' local x = t[1]", "x")[0], "string?");
}

#[test]
fn values_passed_round_a_loop_settle_from_what_they_are_given() {
    // A pool's free frames: released into its list and acquired from it
    // again, so what's acquired is only ever what was made
    let pool = "local P = {} function P:Init() self.free = {} end
        function P:Create() table.insert(self.free, CreateFrame('Button')) end
        function P:Acquire() return table.remove(self.free) end
        function P:Release(f) table.insert(self.free, f) end";
    let source = format!("{pool} local a = P:Acquire() P:Release(a)");
    assert_eq!(library_types_of(&source, "a")[0], "Button?");
    // Through a map of what's in use, found through a parameter, whose
    // type isn't known until a later walk
    let source = format!("{pool} local G = {{}} function G:Pool() return self.pool end
        local function Refresh(group)
            local old = group.used group.used = {{}}
            for _, f in pairs(old) do group:Pool():Release(f) end
            local got = group:Pool():Acquire() group.used[1] = got
        end
        local g = {{ pool = P, used = {{}} }} setmetatable(g, {{ __index = G }}) Refresh(g)");
    assert_eq!(library_types_of(&source, "got")[0], "Button?");
    // Released where it isn't seen: anything may be put back
    let source = format!("{pool} Pool = P");
    assert_eq!(library_types_of(&source, "f")[0], "unknown");
}

#[test]
fn methods_of_tables_a_mixin_is_copied_into() {
    // `self` in `S`'s methods is a frame that got `S` through `P`, so it
    // has `P`'s methods too
    let source = "S = {} function S:A() local b = self:B() return b end
        P = CreateFromMixins(S) function P:B() return 1 end
        local f = CreateFrame('Frame') Mixin(f, P) local x = f:A()";
    assert_eq!(library_types_of(source, "b")[0], "number");
    assert_eq!(library_types_of(source, "x")[0], "number");
}

#[test]
fn frames_of_the_kind_a_function_is_passed() {
    let source = "local Make function Make(kind, parent) return CreateFrame(kind, nil, parent) end
        local function Outer(kind) return Make(kind) end
        local a, b, c, d = Make('Button'), Make('StatusBar'), Outer('Slider'), Make(GetKind())";
    assert_eq!(library_types_of(source, "a")[0], "Button");
    assert_eq!(library_types_of(source, "b")[0], "StatusBar");
    assert_eq!(library_types_of(source, "c")[0], "Slider");
    // Not a literal: any frame
    assert_eq!(library_types_of(source, "d")[0], "Frame");
}

#[test]
fn returned_templates() {
    let definitions = Arc::new(Definitions::from_json(r#"{
        "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}, "Button": {"base": "table", "inherits": ["Frame"]}},
        "method_tables": {"FrameAPI": {"functions": {"Show": {}}}},
        "mixins": {"SlotMixin": {"functions": {"Glow": {"returns": ["number"]}}}},
        "elements": {"Button": "Button", "Frame": "Frame"},
        "templates": {"SlotT": {"type": "Button", "mixins": ["SlotMixin"]}},
        "functions": {"CreateFrame": {"parameters": ["frameType: string"], "returns": ["Frame"]}},
        "returned_templates": {"Provider.Acquire": {"Button": "SlotT"}, "Other.Acquire": {"Button": "NoSuchT"}}
    }"#).unwrap());
    let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
    // Each object of that kind it returns is taken to be made with the template
    let source = "Provider = {} function Provider:Acquire() return CreateFrame('Button') end
        local f = Provider:Acquire() local g = f:Glow()";
    assert_eq!(types_of(source, "g")[0], "number");
    // Another kind it may return stays as it is, so may not have them
    let source = "Provider = {} function Provider:Acquire(plain) if plain then return CreateFrame('Frame') end return CreateFrame('Button') end
        local f = Provider:Acquire() local g = f:Glow()";
    assert_eq!(types_of(source, "g")[0], "unknown");
    // Not for another function, nor a template that isn't known
    let source = "Other = {} function Other:Acquire() return CreateFrame('Button') end
        local f = Other:Acquire() local g = f:Glow()";
    assert_eq!(types_of(source, "g")[0], "unknown");
}

#[test]
fn iterating_contents() {
    let source = "local t = { 'a' } for i, v in ipairs(t) do end";
    assert_eq!(library_types_of(source, "i")[0], "number");
    assert_eq!(library_types_of(source, "v")[0], "string");
    let source = "local t = { n = 1, [true] = 'x' } for k, v in pairs(t) do end";
    assert_eq!(library_types_of(source, "k")[0], "string|true");
    assert_eq!(library_types_of(source, "v")[0], "number|string");
    // ipairs only visits number keys
    let source = "local t = { n = 1 } for i, v in ipairs(t) do end";
    assert_eq!(library_types_of(source, "v")[0], "unknown (not nil)");
    // Unknown contents keep the old guesses
    let source = "local function f(t) for k, v in pairs(t) do end end";
    assert_eq!(library_types_of(source, "k")[0], "unknown (not nil)");
    // Through a parameter, from what calls pass
    let source = "local function f(list) for _, b in ipairs(list) do b:Show() end end f({ CreateFrame('Button') })";
    assert_eq!(library_types_of(source, "b")[0], "Button");
}

#[test]
fn string_methods() {
    assert_eq!(library_types_of("local s = 'x' local u = s:upper()", "u")[0], "string");
    assert_eq!(library_types_of("local u = ('x'):upper():lower()", "u")[0], "string");
    assert_eq!(library_types_of("local u = string.rep('x', 2):upper()", "u")[0], "string");
    assert_eq!(library_types_of("local function f(s) return s:len() end local n = f('x')", "n")[0], "number");
    // A string that may be nil still has them
    assert_eq!(library_types_of("local s if math.random() > 0.5 then s = 'x' end local u = s:upper()", "u")[0], "string");
    // Functions an addon adds to `string`
    assert_eq!(library_types_of("string.ext = function() return 1 end local n = ('x'):ext()", "n")[0], "number");
    // A number has no methods, so calling one on what may be a number or
    // a string is a string's, if it works at all
    assert_eq!(library_types_of("local function f(v) return v:upper() end local u = f(1) f('x')", "u")[0], "string");
    // A string's methods whatever else it may be, so knowing more of
    // what a value may be never takes them away, which would leave a
    // function taking its own results cycling
    let source = "local function parse(value)
        local first, remainder = value:sub(1, 1), value:sub(2)
        if first == '^' then
            local str, rest = remainder:match('(.)(.*)')
            return parse(str), rest
        end
        return 1, remainder
    end
    local x, y = parse(('abc'):lower())";
    let analysis = analyse_with(source, &library());
    assert!(analysis.walks <= 5, "{} walks", analysis.walks);
    assert_eq!(describe_uses(&analysis, "remainder")[0], "string");
    // A table's own field isn't lost to a string's nothing known of it
    assert_eq!(library_types_of("local t = {} t.v = 1 local s = math.random() > 0.5 and t or 'x' local n = s.v", "n")[0], "number");
}

#[test]
fn iterators() {
    let source = "for a, b in ('a1'):gmatch('(%a)(%d)') do end";
    assert_eq!(library_types_of(source, "a")[0], "string");
    assert_eq!(library_types_of(source, "b")[0], "string");
    assert_eq!(library_types_of("for w in string.gmatch('a b', '%a+') do end", "w")[0], "string");
    // A function the code defines: the variables are what it returns, the
    // first never nil as the loop stops then
    let source = "local function it() return 1, 'x' end for n, s in it do end";
    assert_eq!(library_types_of(source, "n")[0], "number");
    assert_eq!(library_types_of(source, "s")[0], "string");
    let source = "local function counter() local i = 0 return function() i = i + 1 if i < 3 then return i end end end for n in counter() do end";
    assert_eq!(library_types_of(source, "n")[0], "number");
}

#[test]
fn metatables() {
    // setmetatable gives back the table, with its fields
    assert_eq!(library_types_of("local t = setmetatable({ a = 1 }, {}) local x = t.a", "x")[0], "number");
    // Missing fields come from `__index`
    assert_eq!(library_types_of("local B = { hi = 'x' } local t = setmetatable({}, { __index = B }) local x = t.hi", "x")[0], "string");
    assert_eq!(library_types_of("local t = setmetatable({}, { __index = function(t, k) return 5 end }) local x = t.any", "x")[0], "number");
    // A table's own fields win
    assert_eq!(library_types_of("local B = { v = 'x' } local t = setmetatable({ v = 1 }, { __index = B }) local x = t.v", "x")[0], "number");
    // The class pattern, two levels deep
    let source = "local A = {} A.__index = A function A:speak() return 'x' end
        local D = setmetatable({}, { __index = A }) D.__index = D function D:fetch() return 1 end
        local d = setmetatable({}, D) local s = d:speak() local n = d:fetch()";
    assert_eq!(library_types_of(source, "s")[0], "string");
    assert_eq!(library_types_of(source, "n")[0], "number");
    // A constructor setting fields on the object it returns
    let source = "local C = {} C.__index = C function C.new() local o = setmetatable({}, C) o.name = 'x' return o end
        function C:get() return 1 end local o = C.new() local nm = o.name local g = o:get()";
    assert_eq!(library_types_of(source, "nm")[0], "string");
    assert_eq!(library_types_of(source, "g")[0], "number");
    // `__call` makes the table callable, given itself first
    assert_eq!(library_types_of("local t = setmetatable({}, { __call = function(self, a) return a end }) local x = t('s')", "x")[0], "string");
    // getmetatable gives back what was set
    assert_eq!(library_types_of("local M = { k = 1 } local t = setmetatable({}, M) local x = getmetatable(t).k", "x")[0], "number");
}

#[test]
fn function_varargs() {
    let source = "local function f(fmt, ...) local a, b = ... local n = select('#', ...) local t = { ... } end f('x', 1, 's') f('y', 2)";
    assert_eq!(types_of(source, "a")[0], "number");
    assert_eq!(types_of(source, "b")[0], "string?");
    assert_eq!(library_types_of(source, "n")[0], "number");
    assert_eq!(types_of(source, "t")[0], "(number|string)[]");
    // The signature shows what's passed, without the nil of shorter calls
    assert_eq!(types_of(source, "f")[0], "function(fmt: string, ...: number|string)");
    // Passed straight on
    assert_eq!(types_of("local function g(...) return ... end local function f(...) return g(...) end local x = f(1)", "x"), ["number"]);
    // Unknown arguments are just unknown
    assert_eq!(types_of("local function f(...) end local function g(x) f(x) end local h = g", "f")[0], "function(...)");
    // A method's `...` doesn't include `self`
    assert_eq!(types_of("local o = {} function o:m(...) local a = ... end o:m('s')", "a"), ["string"]);
    // Some calls unseen, as it escapes into another variable: nothing known
    assert_eq!(types_of("local function f(...) local a = ... end local g = f g(1)", "a"), ["unknown"]);
    // A constructor's last call gives every value; others only their first
    assert_eq!(types_of("local function two() return 1, 's' end local t = { two() }", "t"), ["(number|string)[]"]);
    assert_eq!(types_of("local function two() return 1, 's' end local t = { two(), true }", "t"), ["(number|true)[]"]);
}

#[test]
fn globals_table() {
    // `_G.name` and `_G["name"]` are the global, both ways
    assert_eq!(library_types_of("_G.Saved = { n = 1 } local x = Saved", "x")[0], "table");
    assert_eq!(library_types_of("Count = 5 local x = _G.Count", "x")[0], "number");
    assert_eq!(library_types_of("Count = 5 local x = _G['Count']", "x")[0], "number");
    assert_eq!(library_types_of("_G['Name'] = 's' local x = Name", "x")[0], "string");
    assert_eq!(library_types_of("local function f() return 'r' end _G.F = f local x = F()", "x")[0], "string");
    // Hover shows the global
    let analysis = analyse_with("Count = 5 local x = _G.Count", &library());
    let (text, _) = analysis.hover(FileId(0), TextSize::from(24)).unwrap();
    assert_eq!(text, "global Count: number");
    // The global even where a local hides it
    assert_eq!(library_types_of("local Count = 's' local x = _G.Count", "x")[0], "unknown");
    // A local named `_G` isn't the globals table
    assert_eq!(library_types_of("Count = 5 local _G = { Count = 's' } local x = _G.Count", "x")[0], "string");
}

#[test]
fn standard_values() {
    assert_eq!(library_types_of("local p = math.pi", "p")[0], "number");
    assert_eq!(library_types_of("local h = math.huge", "h")[0], "number");
    assert_eq!(library_types_of("local a = math.atan2(1, 2)", "a")[0], "number");
    assert_eq!(library_types_of("local v = _VERSION", "v")[0], "string");
}

#[test]
fn narrowing_by_truth() {
    let maybe = "local v if math.random() > 0.5 then v = 1 end ";
    assert_eq!(library_types_of(&format!("{maybe} if v then local x = v end"), "x")[0], "number");
    assert_eq!(library_types_of(&format!("{maybe} if not v then local x = v end"), "x")[0], "nil");
    assert_eq!(library_types_of(&format!("{maybe} if v then else local x = v end"), "x")[0], "nil");
    // `elseif` and `else` see the earlier conditions false
    assert_eq!(library_types_of(&format!("{maybe} if v == nil then elseif true then local x = v end"), "x")[0], "number");
    // Afterwards it's as before
    assert_eq!(library_types_of(&format!("{maybe} if v then end local x = v"), "x")[0], "number?");
    // Unless every branch returns, and there's no else
    assert_eq!(library_types_of(&format!("{maybe} if not v then return end local x = v"), "x")[0], "number");
    assert_eq!(library_types_of(&format!("{maybe} if v == nil then return end local x = v"), "x")[0], "number");
    // Only the `else` goes on, where it's set
    assert_eq!(library_types_of(&format!("{maybe} if not v then return else end local x = v"), "x")[0], "number");
    // Without an `else`, going on with every condition false narrows too
    assert_eq!(library_types_of(&format!("{maybe} if v == nil then v = 's' end local x = v"), "x")[0], "number|string");
    // Assigned in a branch: that's kept as well, and otherwise it was false
    assert_eq!(library_types_of(&format!("{maybe} if v then v = 's' end local x = v"), "x")[0], "string?");
    // `and` and `or`
    assert_eq!(library_types_of(&format!("{maybe} local x = v and v + 1"), "x")[0], "number?");
    assert_eq!(library_types_of(&format!("{maybe} if v and v > 0 then local x = v end"), "x")[0], "number");
    assert_eq!(library_types_of(&format!("{maybe} if not v or v > 0 then else local x = v end"), "x")[0], "number");
    let t = "local t if math.random() > 0.5 then t = { n = 1 } end ";
    assert_eq!(library_types_of(&format!("{t} local x = t and t.n"), "x")[0], "number?");
    // `while`
    assert_eq!(library_types_of(&format!("{maybe} while v do local x = v v = nil end"), "x")[0], "number");
    // An upvalue is narrowed as it is where the closure is made when
    // nothing can assign it after
    assert_eq!(library_types_of(&format!("{maybe} if v then local f = function() local x = v end end"), "x")[0], "number");
    // Not when something may: the closure can run at any time
    assert_eq!(library_types_of(&format!("{maybe} if v then local f = function() local x = v end end v = nil"), "x")[0], "number?");
    assert_eq!(library_types_of(&format!("{maybe} local function g() v = nil end if v then local f = function() local x = v end end"), "x")[0], "number?");
    // Nor in a loop the variable is outside of, which runs its writes again
    // while an earlier closure lives on
    assert_eq!(library_types_of(&format!("{maybe} while true do v = math.random() > 0.5 and 1 or nil if v then local f = function() local x = v end end end"), "x")[0], "number?");
}

#[test]
fn narrowing_by_type() {
    let source = "local function f(v) if type(v) == 'string' then local s = v elseif type(v) == 'number' then local n = v else local o = v end end f(1) f('a') f(true)";
    assert_eq!(library_types_of(source, "s")[0], "string");
    assert_eq!(library_types_of(source, "n")[0], "number");
    assert_eq!(library_types_of(source, "o")[0], "true");
    assert_eq!(library_types_of("local function f(v) if 'table' ~= type(v) then return end local t = v end f({}) f(1)", "t")[0], "table");
    // Unknown narrowed to what's tested
    assert_eq!(library_types_of("function F(v) if type(v) == 'function' then local x = v end end", "x")[0], "function");
    // A local named `type` isn't the library's
    assert_eq!(library_types_of("local type = function() return 'x' end local function f(v) if type(v) == 'string' then local s = v end end f(1)", "s")[0], "number");
}

#[test]
fn library_returns_from_arguments() {
    let maybe = "local v if math.random() > 0.5 then v = 1 end ";
    assert_eq!(library_types_of(&format!("{maybe} local x, m = assert(v, 'msg')"), "x")[0], "number");
    assert_eq!(library_types_of(&format!("{maybe} local x, m = assert(v, 'msg')"), "m")[0], "string");
    // As a statement, what it checks holds afterwards
    assert_eq!(library_types_of(&format!("{maybe} assert(v) local x = v"), "x")[0], "number");
    assert_eq!(library_types_of(&format!("{maybe} assert(type(v) == 'number') local x = v"), "x")[0], "number");
    // pcall: success, then the returns or the error message
    let source = "local ok, r, s = pcall(function() return 1, 'x' end)";
    assert_eq!(library_types_of(source, "ok")[0], "boolean");
    assert_eq!(library_types_of(source, "r")[0], "number|string");
    assert_eq!(library_types_of(source, "s")[0], "string?");
    assert_eq!(library_types_of("local ok, r = xpcall(function() return 1 end, function() return true end)", "r")[0], "number|true");
    // From the table's array
    assert_eq!(library_types_of("local a, b = unpack({ 'x', 'y' })", "b")[0], "string?");
    assert_eq!(library_types_of("local t = { 1 } local x = table.remove(t)", "x")[0], "number?");
    assert_eq!(library_types_of("local t = { 1 } local x = tremove(t)", "x")[0], "number?");
    // rawget skips the metatable
    assert_eq!(library_types_of("local t = { a = 1 } local x = rawget(t, 'a')", "x")[0], "number");
    assert_eq!(library_types_of("local B = { b = 1 } local t = setmetatable({}, { __index = B }) local x = rawget(t, 'b')", "x")[0], "unknown");
    let source = "local k, v = next({ a = 1 })";
    assert_eq!(library_types_of(source, "k")[0], "string?");
    assert_eq!(library_types_of(source, "v")[0], "number?");
}

#[test]
fn passed_on_parameters_settle_in_one_go() {
    // Defined deepest first, so each body is walked before its caller:
    // before, each link took another walk, and a cap left the end unknown
    let depth = 60;
    let mut source = format!("local function f{depth}(x) return x end ");
    for i in (1..depth).rev() {
        source += &format!("local function f{i}(x) return f{}(x) end ", i + 1);
    }
    source += "local result = f1('s')";
    assert_eq!(types_of(&source, "result"), ["string"]);
    let analysis = analyse(&source);
    assert!(analysis.walks <= 4, "{} walks", analysis.walks);
    // The same for `...`
    let mut source = format!("local function g{depth}(...) return ... end ");
    for i in (1..depth).rev() {
        source += &format!("local function g{i}(...) return g{}(...) end ", i + 1);
    }
    source += "local result = g1(1)";
    assert_eq!(types_of(&source, "result"), ["number"]);
}

#[test]
fn passed_on_parameters() {
    // Narrowed where it's passed on: only that part goes
    let source = "local function g(s) return s end local function f(v) if type(v) == 'string' then g(v) end end f(1) f('s')";
    assert_eq!(library_types_of(source, "s")[0], "string");
    // Reassigned in a loop after being passed on: what it became goes too
    let source = "local function g(y) return y end local function f(x) for i = 1, 2 do g(x) x = true end end f(1)";
    assert_eq!(types_of(source, "y")[0], "number|true");
    // Passed back to itself: only what's really passed
    let source = "local function f(x, n) if n > 0 then return f(x, n - 1) end return x end local r = f('s', 3)";
    assert_eq!(types_of(source, "x")[0], "string");
    // To a method, with `self` separate
    let source = "local o = {} function o:m(a) return a end local function g(y) return o:m(y) end g(1)";
    assert_eq!(types_of(source, "a")[0], "number");
    let source = "local o = {} function o.m(self, a) return a end local function g(y) return o:m(y) end g(1)";
    assert_eq!(types_of(source, "a")[0], "number");
    // Before more arguments of its own
    let source = "local function g(a, b, ...) local c = ... end local function f(...) g('s', ...) end f(1, true)";
    assert_eq!(types_of(source, "a")[0], "string");
    assert_eq!(types_of(source, "b")[0], "number");
    assert_eq!(types_of(source, "c")[0], "true");
    // From an enclosing function, through a closure
    let source = "local function g(y) return y end local function f(x) local h = function() return g(x) end h() end f('s')";
    assert_eq!(types_of(source, "y")[0], "string");
}

/// Everything an analysis shows, to compare two
fn everything_shown(analysis: &Analysis) -> Vec<String> {
    let mut shown = Vec::new();
    for r in &analysis.references {
        let binding = analysis.binding(r);
        shown.push(format!("{:?} {:?} {} {} {} {}", r.file, r.range, binding.kind, binding.name, r.write, analysis.describe_callee(r.types, r.call_returns.as_ref())));
    }
    for f in &analysis.field_references {
        shown.push(format!("{:?} {:?} {} {} {}", f.file, f.range, analysis.field_path(f), f.write, analysis.describe_callee(f.types, f.call_returns.as_ref())));
    }
    for d in &analysis.diagnostics {
        shown.push(format!("{d:?}"));
    }
    shown.push(format!("{} walks", analysis.walks));
    shown
}

/// Replaying a function body whose inputs haven't changed must give
/// exactly what walking it again would, for every file here
#[test]
fn replaying_matches_walking() {
    let parse = |path: &std::path::Path, in_addon: bool| {
        let text = std::fs::read_to_string(path).unwrap();
        SourceFile { root: SyntaxNode::new_root(Generator::new(&text).process_all()), addon: in_addon.then_some(0), handler: None }
    };
    let mut programs: Vec<(String, Vec<SourceFile>)> = Vec::new();
    for entry in std::fs::read_dir("tests").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "lua") {
            programs.push((path.display().to_string(), vec![parse(&path, false)]));
        }
    }
    let toc = std::path::Path::new("tests/addon/TestAddon/TestAddon.toc");
    let files = crate::project::Addon::load(toc).unwrap().files.iter()
        .map(|file| parse(file, true))
        .collect();
    programs.push((toc.display().to_string(), files));
    assert!(programs.len() > 10);
    let mut replays = 0;
    for (name, files) in &programs {
        let replaying = Analysis::analyse_replaying(files, &library(), Arc::default(), &[], true);
        let walking = Analysis::analyse_replaying(files, &library(), Arc::default(), &[], false);
        replays += replaying.replayed;
        assert_eq!(walking.replayed, 0);
        assert_eq!(everything_shown(&replaying), everything_shown(&walking), "{name}");
    }
    // Or this would prove nothing
    assert!(replays > 100, "{replays} replays");
}

#[test]
fn constructor_fields() {
    assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.count", "a"), ["number"]);
    assert_eq!(types_of("local t = { count = 1, name = 'x' } local a = t.name", "a"), ["string"]);
    assert_eq!(types_of("local t = { f = function() return true end } local a = t.f()", "a"), ["true"]);
    // A string literal key is the field it names; positional entries aren't fields
    assert_eq!(types_of("local t = { 1, ['k'] = 2 } local a = t.k", "a"), ["number"]);
    assert_eq!(types_of("local t = { 1 } local a = t.k", "a"), ["unknown"]);
    assert_eq!(types_of("local t = {} local a = t.missing", "a"), ["unknown"]);
    // Tables never show their identity
    assert_eq!(types_of("local t = { count = 1 }", "t"), ["table"]);
}

#[test]
fn assigned_fields() {
    assert_eq!(types_of("local t = {} t.extra = true local a = t.extra", "a"), ["true"]);
    // Defaulted from itself: the default, rather than nothing for good
    assert_eq!(types_of("local t = {} t.list = t.list or {} local a = t.list", "a"), ["table"]);
    assert_eq!(types_of("local t = {} t.n = (t.n or 0) + 1 local a = t.n", "a"), ["number"]);
    // Every assignment counts, wherever it is
    assert_eq!(types_of("local t = { n = 1 } if c then t.n = 's' end local a = t.n", "a"), ["number|string"]);
    assert_eq!(types_of("local t = {} local function set() t.x = 1 end local a = t.x", "a"), ["number"]);
    // Including after the read
    assert_eq!(types_of("local t = {} local a = t.x t.x = {}", "a"), ["table"]);
    // Nested tables
    assert_eq!(types_of("local t = { inner = { x = 1 } } local a = t.inner.x", "a"), ["number"]);
    assert_eq!(types_of("local t = {} t.inner = {} t.inner.x = 's' local a = t.inner.x", "a"), ["string"]);
}

#[test]
fn fields_follow_the_table_not_the_variable() {
    assert_eq!(types_of("local t = {} local u = t u.x = 1 local a = t.x", "a"), ["number"]);
    assert_eq!(types_of("local t, u = {}, {} t.x = 1 u.x = 's' local a = t.x", "a"), ["number"]);
    // A function given the table writes to it too
    assert_eq!(types_of("local function init(tbl) tbl.x = 1 end local t = {} init(t) local a = t.x", "a"), ["number"]);
}

#[test]
fn methods() {
    let source = "local Addon = {} function Addon:Name() return 's' end function Addon.Count() return 1 end
        local a = Addon:Name() local b = Addon.Count()";
    assert_eq!(types_of(source, "a"), ["string"]);
    assert_eq!(types_of(source, "b"), ["number"]);
    assert_eq!(types_of("local Addon = {} function Addon:Name() end", "Name"), Vec::<String>::new());
    // `self` is the table, so fields set in one method are seen in another
    let source = "local Addon = {} function Addon:Init() self.count = 0 end function Addon:Get() local c = self.count end";
    assert_eq!(types_of(source, "c"), ["number"]);
    let source = "local Addon = {} function Addon:Init() self.count = 0 end local c = Addon.count";
    assert_eq!(types_of(source, "c"), ["number"]);
    // Nested owners
    let source = "local A = { B = {} } function A.B:M() return 1 end local x = A.B:M()";
    assert_eq!(types_of(source, "x"), ["number"]);
    // Globals made in the file work too
    let source = "MyAddon = {} function MyAddon:M() return {} end local function f() local x = MyAddon:M() end";
    assert_eq!(types_of(source, "x"), ["table"]);
}

#[test]
fn pseudo_identities() {
    // The usual addon namespace, which comes from outside the file
    let source = "local _, ns = ... ns.Version = '1.0' function ns:Get() return 1 end local a, b = ns.Version, ns:Get()";
    assert_eq!(types_of(source, "a"), ["string"]);
    assert_eq!(types_of(source, "b"), ["number"]);
    // But it's still unknown what the variable itself is
    assert_eq!(types_of("local _, ns = ...", "ns"), ["unknown"]);
    // Parameters too
    assert_eq!(types_of("local function f(opts) opts.size = 1 return opts.size end local a = f({})", "a"), ["number"]);
    // What it's seen given under other keys is only some of what it holds
    let source = "local s = f() s[5] = true local a = s[3] for _, v in pairs(s) do local b = v end local c = unpack(s)";
    assert_eq!(types_of(source, "a"), ["unknown"]);
    assert_eq!(types_of(source, "b"), ["unknown (not nil)"]);
    assert_eq!(types_of(source, "c"), ["unknown"]);
    let source = "local function P(s) if s[5] == nil then s[5] = s[1] == 1 end return s[5] end local function I(s, l) local W = s[3] return l / W end";
    assert!(analyse(source).diagnostics.is_empty());
}

#[test]
fn fields_of_unidentified_tables() {
    // Writing through an expression that could be either table keeps the write by name
    let source = "local u = {}; (c and {} or u).x = 1 local a = u.x";
    assert_eq!(types_of(source, "a"), ["number"]);
    // A variable holding either one gets a pseudo identity of its own
    let source = "local t = c and {} or {} t.x = 1 local a = t.x local u = {} local b = u.x";
    assert_eq!(types_of(source, "a"), ["number"]);
    assert_eq!(types_of(source, "b"), ["unknown"]);
    // WoW objects share fields by name
    assert_eq!(object_types_of("local f = CreateFrame('Frame') f.value = 5 local a = f.value", "a"), ["number"]);
    assert_eq!(object_types_of("local f = CreateFrame('Frame') function f:OnEvent() return 1 end local a = f:OnEvent()", "a"), ["number"]);
    // Reading from either of two tables gets what the one with it holds
    assert_eq!(types_of("local t = {} t.x = 1 local a = (c and {} or t).x", "a"), ["number"]);
    // Reading from something unknown stays unknown
    assert_eq!(types_of("local t = {} t.x = 1 local a = (c and f() or t).x", "a"), ["unknown"]);
}

/// Files analysed together, in order, as one addon (`in_addon`) or just
/// sharing globals
fn analyse_files(sources: &[&str], in_addon: bool) -> Analysis {
    let files: Vec<SourceFile> = sources.iter()
        .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: in_addon.then_some(0), handler: None })
        .collect();
    Analysis::analyse(&files, &Arc::new(Definitions::default()))
}

/// Type at each use of `name` in file `file`
fn types_in(analysis: &Analysis, file: u32, name: &str) -> Vec<String> {
    analysis.references.iter()
        .filter(|r| r.file == FileId(file) && analysis.binding(r).name.as_ref() == name)
        .map(|r| analysis.describe(r.types))
        .collect()
}

#[test]
fn globals_carry_across_files() {
    let analysis = analyse_files(&["Foo = 1", "local x = Foo"], false);
    assert_eq!(types_in(&analysis, 1, "x"), ["number"]);
    // Files run in order, so a global set by a later file isn't set yet
    let analysis = analyse_files(&["local x = Late", "Late = 's'"], false);
    assert_eq!(types_in(&analysis, 0, "x"), ["nil"]);
    // So making it if it isn't there makes it
    assert_eq!(types_of("T = T or {} local a = T", "a"), ["table"]);
    assert_eq!(types_of("if G == nil then G = 's' end local a = G", "a"), ["string"]);
    // One the files never set is whatever else set it
    assert_eq!(types_of("local a = Unset", "a"), ["unknown"]);
    // But a function reading it can run later
    let analysis = analyse_files(&["function F() return Late end", "Late = 's' local x = F()"], false);
    assert_eq!(types_in(&analysis, 1, "x"), ["string"]);
}

#[test]
fn functions_across_files() {
    let analysis = analyse_files(&["function Helper() return {} end", "local t = Helper()"], false);
    assert_eq!(types_in(&analysis, 1, "t"), ["table"]);
    let analysis = analyse_files(&["Lib = {} function Lib:Get() return 1 end", "local n = Lib:Get()"], false);
    assert_eq!(types_in(&analysis, 1, "n"), ["number"]);
}

#[test]
fn locals_stay_in_their_file() {
    let analysis = analyse_files(&["local x = 1", "local y = x"], false);
    assert_eq!(types_in(&analysis, 1, "y"), ["unknown"]);
    assert_eq!(analysis.binding(analysis.references.iter().find(|r| r.file == FileId(1) && !r.write).unwrap()).kind, BindingKind::Global);
}

#[test]
fn addon_namespace_is_shared() {
    let analysis = analyse_files(&[
        "local name, ns = ... ns.Version = '1.0' function ns:GetName() return name end",
        "local _, ns = ... function ns.Round(x) return 1 end",
        "local _, ns = ... local v, n, r = ns.Version, ns:GetName(), ns.Round(1)",
    ], true);
    assert_eq!(types_in(&analysis, 0, "name"), ["string", "string"]);
    assert_eq!(types_in(&analysis, 2, "v"), ["string"]);
    assert_eq!(types_in(&analysis, 2, "n"), ["string"]);
    assert_eq!(types_in(&analysis, 2, "r"), ["number"]);
    // A method defined in a later file, called by an earlier one
    let analysis = analyse_files(&[
        "local _, ns = ... function F() return ns:Later() end",
        "local _, ns = ... function ns:Later() return true end local x = F()",
    ], true);
    assert_eq!(types_in(&analysis, 1, "x"), ["true"]);
}

#[test]
fn select_from_varargs() {
    let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
        "select": {"parameters": ["index: number|string", "..."], "returns": ["unknown"], "rest": "unknown"}
    }}"#).unwrap());
    let analyse = |sources: &[&str]| {
        let files: Vec<SourceFile> = sources.iter()
            .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
            .collect();
        Analysis::analyse(&files, &definitions)
    };
    // The usual way to get the namespace without naming the addon
    let analysis = analyse(&["local ns = select(2, ...) ns.x = 1", "local ns = select(2, ...) local y = ns.x"]);
    assert_eq!(types_in(&analysis, 1, "y"), ["number"]);
    let analysis = analyse(&["local name = select(1, ...)", "local n = select('#', ...)", "local a, b = select(2, 1, 's', true)"]);
    assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
    assert_eq!(types_in(&analysis, 1, "n"), ["number"]);
    assert_eq!(types_in(&analysis, 2, "a"), ["string"]);
    assert_eq!(types_in(&analysis, 2, "b"), ["true"]);
    // An index that isn't a literal, or a shadowed `select`, isn't followed
    assert_eq!(types_in(&analyse(&["local i = 2 local ns = select(i, ...)"]), 0, "ns")[0], "unknown");
    assert_eq!(types_in(&analyse(&["local function select() return 1 end local ns = select(2, ...)"]), 0, "ns"), ["number"]);
}

#[test]
fn parameters_from_calls_in_other_files() {
    let analysis = analyse_files(&["function Id(x) return x end", "local v = Id(1)"], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
    assert_eq!(types_in(&analysis, 1, "v"), ["number"]);
    let analysis = analyse_files(&[
        "local _, ns = ... function ns.Id(x) return x end function ns:Add(n) return n end",
        "local _, ns = ... local a, b = ns.Id(1), ns:Add('s')",
    ], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
    assert_eq!(types_in(&analysis, 0, "n"), ["string", "string"]);
    assert_eq!(types_in(&analysis, 1, "a"), ["number"]);
    assert_eq!(types_in(&analysis, 1, "b"), ["string"]);
    // From calls in several files
    let analysis = analyse_files(&["local _, ns = ... function ns.F(x) end", "local _, ns = ... ns.F(1)", "local _, ns = ... ns.F('s')"], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["number|string"]);
}

#[test]
fn methods_called_either_way() {
    // `self` is passed explicitly with `.`, so the arguments line up after it
    let analysis = analyse_files(&["local _, ns = ... function ns:Add(n) end ns.Add(ns, 1)"], true);
    assert_eq!(types_in(&analysis, 0, "n"), ["number"]);
    let analysis = analyse_files(&["local _, ns = ... function ns.Add(t, n) end ns:Add(1)"], true);
    assert_eq!(types_in(&analysis, 0, "n"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "t"), ["table"]);
}

#[test]
fn shared_functions_that_escape() {
    // Handed to something else, which could call it with anything
    let analysis = analyse_files(&["local _, ns = ... function ns.OnEvent(x) end", "local _, ns = ... ns.OnEvent(1) frame:SetScript('OnEvent', ns.OnEvent)"], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
    let analysis = analyse_files(&["function Handler(x) end", "Handler(1) register(Handler)"], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
    // Copied onto something else (a mixin), whose calls aren't known to be it
    let analysis = analyse_files(&["local M = {} function M:Init(x) end M:Init(1)", "local f = Mixin(CreateFrame('Frame'), M) f:Init('s')"], true);
    assert_eq!(types_in(&analysis, 0, "x"), ["unknown"]);
}

#[test]
fn methods_only_given_as_script_handlers() {
    // Never called on `M`, only by the game on the frame
    let source = "local f = CreateFrame('Button')
        local M = {} function M:OnShow() local s = self end
        f:SetScript('OnShow', M.OnShow)";
    assert_eq!(widget_types_of(source, "s")[0], "Button");
    // Also called on `M`: either may be `self`, and `M` is the guess
    let source = "local f = CreateFrame('Button')
        local M = {} function M:OnShow() local s = self end
        f:SetScript('OnShow', M.OnShow) M:OnShow()";
    assert_eq!(widget_types_of(source, "s")[0], "table");
    // Read as a value for anything else, it's still `M`'s
    let source = "local M = { libs = {} } function M:Get() local s = self.libs end setmetatable(M, { __call = M.Get })";
    assert_eq!(types_of(source, "s"), ["table"]);
}

#[test]
fn script_handlers_set_in_lua() {
    let source = "local f = CreateFrame('Button')
        f:SetScript('OnEvent', function(self, eventName, a) local s, e, x = self, eventName, a end)
        f:SetScript('OnClick', function(self, button, down) local b, d = button, down end)
        local M = {} function M:OnUpdate(elapsed) local el = elapsed end
        f:HookScript('OnUpdate', M.OnUpdate)
        f:SetScript('OnUpdate', nil)";
    // The frame, then what that kind of handler is passed
    assert_eq!(widget_types_of(source, "s")[0], "Button");
    assert_eq!(widget_types_of(source, "e")[0], "string");
    // An event's payload isn't known
    assert_eq!(widget_types_of(source, "x")[0], "unknown");
    assert_eq!(widget_types_of(source, "b")[0], "string");
    assert_eq!(widget_types_of(source, "d")[0], "boolean");
    // A method's `self` is the frame, so its parameters start after
    assert_eq!(widget_types_of(source, "el")[0], "number");
}

#[test]
fn functions_only_some_clients_have() {
    let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
        "Engrave": {"parameters": ["slot: number"], "returns": ["boolean"], "only": ["Classic"]},
        "Both": {"parameters": [], "returns": [], "only": ["Classic", "Forever"]}
    }}"#).unwrap());
    let analysis = analyse_with("local a, b = Engrave, Both local c = Engrave(1)", &definitions);
    assert_eq!(describe_uses(&analysis, "a")[0], "function(slot: number) -> boolean (Classic only)");
    assert_eq!(describe_uses(&analysis, "b")[0], "function() (Classic and Forever only)");
    assert_eq!(describe_uses(&analysis, "c")[0], "boolean");
}

#[test]
fn callback_registry_callbacks() {
    let definitions = Arc::new(Definitions::from_json(r#"{"functions": {
        "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]}
    }, "tables": {"CallbackRegistryMixin": {"functions": {
        "RegisterCallback": {"parameters": ["event", "func", "owner", "..."], "returns": ["unknown"]},
        "TriggerEvent": {"parameters": ["event", "..."]}
    }}}}"#).unwrap());
    let source = "local R = CreateFromMixins(CallbackRegistryMixin)
        R:RegisterCallback('X', function(o, v) local a, own = v, o end)
        local M = {} function M:OnX(v) local b = v end
        R:RegisterCallback('X', M.OnX, M)
        R:RegisterCallback('X', function(o, extra, v) local c, e = v, extra end, M, 's')
        R:RegisterCallback('Never', function(o, v) local d = v end)
        R:TriggerEvent('X', 5)";
    let analysis = analyse_with(source, &definitions);
    // Each gets its owner (a number made for it without one), what was
    // registered after the owner, then what the event passes
    assert_eq!(describe_uses(&analysis, "a")[0], "number");
    assert_eq!(describe_uses(&analysis, "own")[0], "number");
    assert_eq!(describe_uses(&analysis, "b")[0], "number");
    assert_eq!(describe_uses(&analysis, "c")[0], "number");
    assert_eq!(describe_uses(&analysis, "e")[0], "string");
    // Never triggered: nothing known
    assert_eq!(describe_uses(&analysis, "d")[0], "unknown");
}

fn mixin_definitions() -> Arc<Definitions> {
    Arc::new(Definitions::from_json(r#"{"functions": {
        "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
        "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
        "CreateAndInitFromMixin": {"parameters": ["mixin", "..."], "returns": ["table"], "fresh_returns": [{"parameters": [0]}], "method_calls": [{"on": [0], "method": "Init", "arguments": ["..."]}]},
        "CreateFooter": {"parameters": ["parent"], "returns": ["table"], "fresh_returns": [{"mixins": ["FooterMixin"]}]},
        "CreateColumn": {"returns": ["table"], "fresh_returns": [{"mixins": ["FooterMixin"], "values": {"width": "number", "OnResize": "function"}}]},
        "MakePaint": {"parameters": ["r", "g", "a"], "returns": ["table"], "fresh_returns": [{"mixins": ["PaintMixin"]}],
            "field_writes": [{"on": {"returned": 0}, "field": "r", "value": [0]}, {"on": {"returned": 0}, "field": "a", "value": [{"truthy": 2}, "number"]}]}
    }, "tables": {
        "FooterMixin": {"functions": {"Finalize": {"returns": ["number"]}}},
        "PaintMixin": {"functions": {
            "GetR": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "r"}]},
            "SetR": {"parameters": ["r"], "field_writes": [{"on": "self", "field": "r", "value": [0]}]}
        }, "values": {"r": "unknown", "a": "unknown"}},
        "SOME_COLOR": {"functions": {
            "GetR": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "r"}]},
            "GetLabel": {"returns": ["unknown"], "returned_fields": [{"on": "self", "field": "label"}]}
        }, "values": {"r": "number"}}
    }}"#).unwrap())
}

#[test]
fn fields_given_after_tables_are_made() {
    // As Blizzard's code gives a column a width where it loops over them:
    // each call's has it, besides its mixins' methods
    let analysis = analyse_mixins(&["local c = CreateColumn() local w, done = c.width, c:Finalize() c:OnResize()"]);
    assert_eq!(types_in(&analysis, 0, "w"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
    // A function it's given is a method it has
    assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    assert!(!analyse_mixins(&["CreateColumn():Nope()"]).diagnostics.is_empty());
}

#[test]
fn global_tables_return_their_fields() {
    // A global table the definitions give: the field they give it, as
    // for `NORMAL_FONT_COLOR:GetRGB()`, but not what any table's field
    // of that name may be
    let analysis = analyse_mixins(&["local t = {} t.label = 's' local r, label = SOME_COLOR:GetR(), SOME_COLOR:GetLabel()"]);
    assert_eq!(types_in(&analysis, 0, "r"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "label"), ["unknown"]);
}

fn analyse_mixins(sources: &[&str]) -> Analysis {
    let files: Vec<SourceFile> = sources.iter()
        .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
        .collect();
    Analysis::analyse(&files, &mixin_definitions())
}

#[test]
fn create_from_mixins() {
    let analysis = analyse_mixins(&[
        "local _, ns = ... ns.Base = {} function ns.Base:Name() return 's' end",
        "local _, ns = ... ns.Bar = CreateFromMixins(ns.Base) function ns.Bar:Size() return 1 end",
        "local _, ns = ... local a, b, c = ns.Bar:Name(), ns.Bar:Size(), ns.Bar",
    ]);
    assert_eq!(types_in(&analysis, 2, "a"), ["string"]);
    assert_eq!(types_in(&analysis, 2, "b"), ["number"]);
    assert_eq!(types_in(&analysis, 2, "c"), ["table"]);
    // Its own fields win, and so do later mixins
    let analysis = analyse_mixins(&["local A = { x = 1 } local B = { x = 's' } local C = CreateFromMixins(A, B) local D = CreateFromMixins(A) D.x = true
        local c, d = C.x, D.x"]);
    assert_eq!(types_in(&analysis, 0, "c"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "d"), ["true"]);
    // Through an alias too
    let analysis = analyse_mixins(&["local Create = CreateFromMixins local A = { x = 1 } local y = Create(A).x"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
}

#[test]
fn methods_called_on_what_a_function_is_passed() {
    // A definition's: `CreateAndInitFromMixin` calls `Init` on its new table
    let analysis = analyse_mixins(&["local M = {} function M:Init(x, y) self.x = x end
        local o = CreateAndInitFromMixin(M, 1, 's') local v = o.x"]);
    assert_eq!(types_in(&analysis, 0, "x"), ["number", "number"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "v"), ["number"]);
    // The files' own, on a parameter or a table made with one mixed in,
    // through a function passing its parameters on, each call with its
    // own mixin: neither `Init` gets the other's arguments
    let analysis = analyse_mixins(&["local A = {} function A:Init(a) end local B = {} function B:Init(b) end
        local function Make(mixin, ...) local o = CreateFromMixins(mixin) o:Init(...) return o end
        local function Start(mixin, value) mixin:Init(value) end
        local function MakeAgain(mixin, ...) return Make(mixin, ...) end
        Make(A, 1) MakeAgain(B, 's') Start(A, 2)"]);
    assert_eq!(types_in(&analysis, 0, "a"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "b"), ["string"]);
    // Called in a function whose calls aren't all seen, as a callback
    // handed to something else is: that may call it with anything
    let analysis = analyse_mixins(&["local M = {} function M:Set(v) end M:Set(1)
        register(function(button, value) button:Set(value) end)"]);
    assert_eq!(types_in(&analysis, 0, "v"), ["unknown"]);
    // Passing it on to itself, as Blizzard's `VariadicInsert` does, settles
    let analysis = analyse_mixins(&["local A = {} function A:Insert(a) end
        local function Insert(on, ...) local first = ... if first == nil then return end on:Insert(first) Insert(on, select(2, ...)) end
        Insert(A, 1, 2)"]);
    assert!(analysis.walks < 10);
}

#[test]
fn tables_of_a_family() {
    // Any number of the tables a definition's function makes, together,
    // are of its family: its mixins' type, with what any was given
    let footers: Vec<String> = (0..12).map(|_| "CreateFooter()".to_string()).collect();
    let analysis = analyse_mixins(&[&format!("local all = {{ {} }} all[2].label = 's'
        local any = all[1] local done, label = any:Finalize(), any.label
        local one = CreateFooter() one.own = 1 local own, other = one.own, CreateFooter().own", footers.join(", "))]);
    assert_eq!(types_in(&analysis, 0, "any")[0], "FooterMixin?");
    assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "label"), ["string"]);
    // Each still has its own fields
    assert_eq!(types_in(&analysis, 0, "own"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "other"), ["unknown"]);
    // Mixing in what it's passed: each call's own, any of them all of them
    let analysis = analyse_mixins(&["local A = {} function A:Name() return 's' end local B = {} function B:Size() return 1 end
        local all = { CreateAndInitFromMixin(A), CreateAndInitFromMixin(B) } local any = all[1]
        local name, size, a = any:Name(), any:Size(), CreateAndInitFromMixin(A).Size"]);
    assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "size"), ["number"]);
}

#[test]
fn fields_given_what_functions_are_passed() {
    // As `CreateColor` and `ColorMixin` are: each call's table has what
    // that call passed, its methods give it back, and any of many has
    // what each has
    let paints: Vec<String> = (0..9).map(|_| "MakePaint(1)".to_string()).collect();
    let analysis = analyse_mixins(&[&format!("local p = MakePaint(1) local s = MakePaint('s')
        local r, a, sr, got = p.r, p.a, s.r, p:GetR()
        local t = MakePaint('t') t:SetR(true) local set = t.r
        local all = {{ {} }} local any = all[1] local anyr, anyget = any.r, any:GetR()", paints.join(", "))]);
    assert_eq!(types_in(&analysis, 0, "r"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "a"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "sr"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "got"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "set"), ["string|true"]);
    // Any of them is of the function's family: what any of its tables
    // in the file was given, the others' too
    assert_eq!(types_in(&analysis, 0, "anyr"), ["number|string|true"]);
    assert_eq!(types_in(&analysis, 0, "anyget"), ["number|string|true"]);
    // The files' own, through the methods a function calls on its new
    // table, each call's its own
    let analysis = analyse_mixins(&["local M = {}
        function M:SetRA(r, a) self.r = r self.a = a or 1 end
        function M:OnLoad(r, a) self:SetRA(r, a) end
        function M:GetR() return self.r end
        local function Make(r, a) local c = CreateFromMixins(M) c:OnLoad(r, a) return c end
        local c, s = Make(1), Make('s', 'x')
        local cr, sr, ca, sa, got = c.r, s.r, c.a, s.a, s:GetR()"]);
    assert_eq!(types_in(&analysis, 0, "cr"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "sr"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "ca"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "sa"), ["number|string"]);
    assert_eq!(types_in(&analysis, 0, "got"), ["string"]);
}

#[test]
fn cached_tables_are_not_new_each_call() {
    // Kept in an upvalue or a global, so later calls return the same one
    let analysis = analyse_mixins(&["local cached local function Get() if not cached then cached = {} end return cached end
        local function GetMade() if not Cached then Cached = CreateAndInitFromMixin({}) end return Cached end
        Get().x = 1 GetMade().y = 1 local x, y = Get().x, GetMade().y"]);
    assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
}

#[test]
fn definitions_making_tables_with_mixins() {
    // As generated from Blizzard's `CreateAndInitFromMixin`: each call a
    // new table, with what it's passed mixed in
    let analysis = analyse_mixins(&["local A = { x = 1 } function A:Name() return 's' end local B = { x = 's' }
        local a, b = CreateAndInitFromMixin(A, 1), CreateAndInitFromMixin(B)
        local ax, name, bx = a.x, a:Name(), b.x"]);
    assert_eq!(types_in(&analysis, 0, "ax"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "name"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "bx"), ["string"]);
    // As `GamepadSharedUtility.CreatePromptedBindingFooter` is: with a
    // global of the definitions mixed in, and fields of its own
    let analysis = analyse_mixins(&["local f = CreateFooter() f.x = 1 local done, x, other = f:Finalize(), f.x, CreateFooter().x"]);
    assert_eq!(types_in(&analysis, 0, "done"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
    // Another call's table doesn't have them
    assert_eq!(types_in(&analysis, 0, "other"), ["unknown"]);
    // Functions of the files returning what such a function does: new at
    // each call too, with what they pass on mixed in
    let analysis = analyse_mixins(&["local A = {} function A:Name() return 's' end local B = {} function B:Size() return 1 end
        local function Make(mixin) return CreateAndInitFromMixin(mixin) end
        local function MakeA() return CreateAndInitFromMixin(A) end
        local function MakeAgain(mixin) return Make(mixin) end
        local a, b, c = MakeA():Name(), Make(B):Size(), MakeAgain(A):Name()"]);
    assert_eq!(types_in(&analysis, 0, "a"), ["string"]);
    assert_eq!(types_in(&analysis, 0, "b"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "c"), ["string"]);
}

#[test]
fn mixin_into_objects() {
    let analysis = analyse_mixins(&["local M = {} function M:Init(x) end M:Init(1) local t = Mixin({}, M) t:Init('s')"]);
    // Both calls are known to be M.Init
    assert_eq!(types_in(&analysis, 0, "x"), ["number|string"]);
    let analysis = analyse_mixins(&["local M = { v = 1 } local t = {} Mixin(t, M) local y = t.v"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
}

fn analyse_files_with(sources: &[&str], definitions: &Arc<Definitions>) -> Analysis {
    let files: Vec<SourceFile> = sources.iter()
        .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
        .collect();
    Analysis::analyse(&files, definitions)
}

fn analyse_objects(sources: &[&str]) -> Analysis {
    let definitions = Arc::new(Definitions::from_json(r#"{
        "types": {"Frame": {"base": "table", "method_tables": ["FrameAPI"]}},
        "method_tables": {"FrameAPI": {"functions": {"Show": {}, "GetParent": {"returns": ["Frame"]}}}},
        "values": {"UIParent": "Frame"},
        "functions": {
            "CreateFrame": {"parameters": ["frameType: string", "name: string|nil", "parent: Frame|nil"], "returns": ["Frame"]},
            "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
            "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]}
        }
    }"#).unwrap());
    let files: Vec<SourceFile> = sources.iter()
        .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
        .collect();
    Analysis::analyse(&files, &definitions)
}

#[test]
fn functions_mixing_in_parameters() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // Coolinator's InitFrameWithMixin
    let analysis = analyse_objects(&[
        "local _, ns = ... function ns.Init(parent, mixin) local f = CreateFrame('Frame', nil, parent) Mixin(f, mixin) f:OnLoad() return f end",
        "local _, ns = ... ns.A = {} function ns.A:OnLoad() end function ns.A:Count() return 1 end
         ns.B = {} function ns.B:OnLoad() end function ns.B:Name() return 's' end",
        "local _, ns = ... local a, b = ns.Init(UIParent, ns.A), ns.Init(UIParent, ns.B)
         local x, y, z = a:Count(), b:Name(), a:Name()
         local p = a:GetParent() a:Show()",
    ]);
    assert_eq!(types_in(&analysis, 2, "a")[0], "Frame & ns.A");
    assert_eq!(types_in(&analysis, 2, "x"), ["number"]);
    assert_eq!(types_in(&analysis, 2, "y"), ["string"]);
    // `a` was made with A, not B
    assert_eq!(types_in(&analysis, 2, "z"), ["unknown"]);
    assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
}

#[test]
fn each_call_returns_a_new_object() {
    let analysis = analyse_objects(&["local function Make() local t = { kind = 1 } return t end
        local a, b = Make(), Make() a.v = 1 b.v = 's' local x, y, k = a.v, b.v, a.kind"]);
    assert_eq!(types_in(&analysis, 0, "x"), ["number"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["string"]);
    // Fields set where it was made still count
    assert_eq!(types_in(&analysis, 0, "k"), ["number"]);
    // Something not made by the function is the same table every call
    let analysis = analyse_objects(&["local shared = {} local function Get() return shared end
        local a, b = Get(), Get() a.v = 1 local y = b.v"]);
    assert_eq!(types_in(&analysis, 0, "y"), ["number"]);
}

#[test]
fn varargs_outside_addon_files() {
    // Not part of an addon, so nothing is known about `...`
    assert_eq!(types_in(&analyse_files(&["local name, ns = ..."], false), 0, "name"), ["unknown"]);
    // A function's own `...` is its arguments, even in an addon
    assert_eq!(types_in(&analyse_files(&["local function f(...) local a = ... end"], true), 0, "a"), ["unknown"]);
}

#[test]
fn references_are_per_file() {
    let analysis = analyse_files(&["local a = 1", "local b = 's'"], false);
    let first = analysis.reference_at(FileId(0), TextSize::from(6)).unwrap();
    let second = analysis.reference_at(FileId(1), TextSize::from(6)).unwrap();
    assert_eq!(&*analysis.binding(first).name, "a");
    assert_eq!(&*analysis.binding(second).name, "b");
    assert!(analysis.reference_at(FileId(2), TextSize::from(6)).is_none());
}

/// Hover text for each use of the field reached by `path`, in source order
fn field_hovers(analysis: &Analysis, path: &str) -> Vec<String> {
    analysis.field_references.iter()
        .filter(|f| analysis.field_path(f) == path)
        .map(|f| analysis.hover(f.file, f.range.start()).unwrap().0)
        .collect()
}

#[test]
fn field_names_have_types() {
    // The value assigned, and then the field's type wherever it's read
    let source = "local addonTable = {} local hidden = {} addonTable.hiddenTable = hidden local x = addonTable.hiddenTable";
    let analysis = analyse(source);
    assert_eq!(field_hovers(&analysis, "addonTable.hiddenTable"),
        ["field addonTable.hiddenTable: table", "field addonTable.hiddenTable: table"]);
    let analysis = analyse_with("local t = {} local hidden = CreateFrame('Frame') t.hidden = hidden local x = t.hidden", &objects());
    assert_eq!(field_hovers(&analysis, "t.hidden"), ["field t.hidden: Frame", "field t.hidden: Frame"]);
    // Through a string key too
    let analysis = analyse("local t = {} t['v'] = 1 local x = t['v']");
    assert_eq!(field_hovers(&analysis, "t['v']"), ["field t['v']: number", "field t['v']: number"]);
    // A write shows what that write assigned; reads show every assignment
    let analysis = analyse("local t = {} t.v = 1 t.v = 's' local x = t.v");
    assert_eq!(field_hovers(&analysis, "t.v"), ["field t.v: number", "field t.v: string", "field t.v: number|string"]);
}

#[test]
fn field_names_in_every_position() {
    let analysis = analyse("local t = { count = 1, a = { b = {} } } function t.a.b:Get() return 's' end local x = t:Get() local y = t.a.b:Get()");
    assert_eq!(field_hovers(&analysis, "count"), ["field count: number"]);
    assert_eq!(field_hovers(&analysis, "t.a"), ["field t.a: table", "field t.a: table"]);
    assert_eq!(field_hovers(&analysis, "t.a.b:Get"), ["field t.a.b:Get: function() -> string", "field t.a.b:Get: function() -> string"]);
    // `Get` is on `t.a.b`, not `t`
    assert_eq!(field_hovers(&analysis, "t:Get"), ["field t:Get: unknown"]);
    // Whitespace and comments aren't part of the path
    let analysis = analyse("local t = {} t . --[[c]] x = 1");
    assert_eq!(field_hovers(&analysis, "t.x"), ["field t.x: number"]);
}

#[test]
fn field_names_in_loops_are_recorded_once() {
    let analysis = analyse("local t = {} while c do t.x = 1 local y = t.x end");
    assert_eq!(field_hovers(&analysis, "t.x").len(), 2);
}

fn widgets() -> Arc<Definitions> {
    Arc::new(Definitions::from_json(r#"{
        "types": {
            "Region": {"base": "table", "method_tables": ["RegionAPI"]},
            "Frame": {"base": "table", "inherits": ["Region"], "method_tables": ["FrameAPI"]},
            "Button": {"base": "table", "inherits": ["Frame"], "method_tables": ["ButtonAPI"]},
            "Texture": {"base": "table", "inherits": ["Region"], "method_tables": []},
            "FontString": {"base": "table", "inherits": ["Region"], "method_tables": ["FontStringAPI"]},
            "ObjectPool": {"base": "table", "method_tables": ["ObjectPoolAPI"]},
            "AnimationGroup": {"base": "table", "method_tables": ["AnimationGroupAPI"]},
            "Animation": {"base": "table", "method_tables": ["AnimationAPI"]},
            "Alpha": {"base": "table", "inherits": ["Animation"], "method_tables": ["AlphaAPI"]},
            "ItemKey": {"base": "table"}
        },
        "method_tables": {
            "RegionAPI": {"functions": {"SetPoint": {}, "Show": {}, "GetParent": {"returns": ["Region"]}, "SetParent": {"parameters": ["parent: Frame|nil"]}}},
            "FrameAPI": {"functions": {
                "SetScript": {"parameters": ["scriptTypeName: string", "script: function|nil"]},
                "HookScript": {"parameters": ["scriptTypeName: string", "script: function"]},
                "CreateTexture": {"returns": ["Texture"]},
                "CreateFontString": {"parameters": ["name: string|nil", "templateName: string|nil"], "returns": ["Texture"]},
                "GetChild": {"returns": ["Frame|nil"]},
                "CreateAnimationGroup": {"returns": ["AnimationGroup"]}
            }},
            "AnimationGroupAPI": {"functions": {"CreateAnimation": {"parameters": ["animationType: string|nil", "name: string|nil", "templateName: string|nil"], "returns": ["Animation"]}}},
            "AnimationAPI": {"functions": {"Play": {}}},
            "AlphaAPI": {"functions": {"SetFromAlpha": {}}},
            "ButtonAPI": {"functions": {"Click": {}, "GetText": {"returns": ["string"]}}},
            "FontStringAPI": {"functions": {"SetText": {}}},
            "ObjectPoolAPI": {"functions": {"Acquire": {"returns": ["unknown", "boolean"]}, "Release": {"parameters": ["object"]},
                "EnumerateActive": {"returns": ["function", "table"]}, "GetNextActive": {"returns": ["unknown"]}}}
        },
        "elements": {"Frame": "Frame", "Button": "Button", "Texture": "Texture", "FontString": "FontString", "Font": "Font",
            "Odd": {"type": "Frame", "extended": true}},
        "mixins": {
            "BaseMixin": {"functions": {"Base": {}, "Value": {"returns": ["string"]}}},
            "ButtonMixin": {"functions": {"Init": {}, "Value": {"returns": ["number"]}}},
            "OpenMixin": {"open": true, "functions": {"Known": {}}}
        },
        "templates": {
            "BaseT": {"type": "Button", "mixins": ["BaseMixin"]},
            "ButtonT": {"type": "Button", "inherits": ["BaseT"], "mixins": ["ButtonMixin"], "values": {"flag": "boolean"},
                "children": {"Label": {"type": "FontString", "inherits": ["GameFont"]}, "Icon": {"type": "Texture"}},
                "arrays": {"Tabs": [{"type": "Button"}]}},
            "OpenT": {"type": "Frame", "mixins": ["OpenMixin"]},
            "MissingT": {"type": "Frame", "mixins": ["Nowhere"]},
            "TextureT": {"type": "Texture", "values": {"v": "number"}}
        },
        "frames": {
            "GameFont": {"type": "Font"},
            "Plain": {"type": "Frame", "children": {"Inner": {"type": "Button", "mixins": ["ButtonMixin"]}}},
            "Opened": {"type": "Frame", "inherits": ["OpenT"]},
            "UIParent": {"type": "Button"}
        },
        "values": {"UIParent": "Frame"},
        "tables": {
            "RegistryMixin": {"functions": {"Register": {}, "Count": {"returns": ["number"]}}},
            "OpenRegistryMixin": {"open": true, "functions": {"Register": {}}}
        },
        "functions": {
            "CreateFrame": {"parameters": ["frameType: string", "name: string|nil", "parent: Frame|nil", "template: string|nil"], "returns": ["Frame"]},
            "GetItemKey": {"returns": ["ItemKey"]},
            "GetFrame": {"returns": ["Frame"]},
            "Anchor": {"parameters": ["region: Region"]},
            "Parent": {"parameters": ["frame: Frame"]},
            "CreateFromMixins": {"parameters": ["..."], "returns": ["unknown"]},
            "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
            "CopyTable": {"parameters": ["settings", "shallow"], "returns": ["table"]},
            "CopyTableSafe": {"parameters": ["settings", "shallow"], "returns": ["table|nil"]},
            "CreateFramePool": {"parameters": ["frameType: string|nil", "parent: Frame|nil", "frameTemplate: string|nil", "resetFunc: function|nil", "forbidden: boolean|nil", "frameInitFunc: function|nil"], "returns": ["ObjectPool"]},
            "CreateFontStringPool": {"parameters": ["parent: Frame|nil", "layer: string|nil", "subLayer: number|nil", "fontStringTemplate: string|nil", "resetFunc: function|nil"], "returns": ["ObjectPool"]},
            "CreateObjectPool": {"parameters": ["createFunc: function|nil", "resetFunc: function|nil"], "returns": ["ObjectPool"]}
        }
    }"#).unwrap())
}

fn widget_types_of(source: &str, name: &str) -> Vec<String> {
    describe_uses(&analyse_with(source, &widgets()), name)
}

fn warnings(source: &str) -> Vec<String> {
    analyse_with(source, &widgets()).diagnostics.iter().map(|d| d.message.clone()).collect()
}

#[test]
fn create_frame_types() {
    assert_eq!(widget_types_of("local f = CreateFrame('Button')", "f"), ["Button"]);
    assert_eq!(widget_types_of("local f = CreateFrame(\"button\", 'Name')", "f"), ["Button"]);
    // Not a kind of frame, or not a literal
    assert_eq!(widget_types_of("local f = CreateFrame('Texture')", "f"), ["Frame"]);
    assert_eq!(widget_types_of("local f = CreateFrame(kind)", "f"), ["Frame"]);
    // Templates still give the type
    assert_eq!(widget_types_of("local f = CreateFrame('Button', nil, nil, 'Template')", "f"), ["Button"]);
}

#[test]
fn methods_of_wow_types() {
    assert_eq!(widget_types_of("local f = CreateFrame('Button') local t = f:CreateTexture()", "t"), ["Texture"]);
    assert_eq!(widget_types_of("local f = CreateFrame('Button') local t = f:GetText()", "t"), ["string"]);
    assert_eq!(widget_types_of("local f = CreateFrame('Frame') local c = f:GetChild()", "c"), ["Frame?"]);
    // Methods are fields too
    assert_eq!(widget_types_of("local f = CreateFrame('Frame') local m = f.Show", "m"), ["function()"]);
}

#[test]
fn missing_data_fields() {
    let definitions = Arc::new(Definitions::from_json(r#"{
        "types": {"Info": {"base": "table", "values": {"count": "number", "link": "string|nil"}}, "Loose": {"base": "table"}},
        "functions": {"GetInfo": {"returns": ["Info|nil"]}, "GetLoose": {"returns": ["Loose"]},
            "Mixin": {"parameters": ["object", "..."], "returns": ["unknown"]},
            "rawset": {"parameters": ["t: table", "key", "value"], "returns": ["table"]},
            "setmetatable": {"parameters": ["t: table", "mt"], "returns": ["table"]},
            "pairs": {"parameters": ["t"], "returns": ["unknown"]}}
    }"#).unwrap());
    let warnings = |source: &str| -> Vec<String> {
        analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect()
    };
    assert!(warnings("local i = GetInfo() print(i.count, i.link, i['count'])").is_empty());
    assert_eq!(warnings("local i = GetInfo() print(i.cuont)"), ["Info has no field `cuont`"]);
    assert_eq!(warnings("local i = GetInfo() print(i['cuont'])"), ["Info has no field `cuont`"]);
    // The addon may give it fields of its own, even after reading them
    assert!(warnings("local i = GetInfo() print(i.extra) i.extra = 1").is_empty());
    assert!(warnings("local function tag(t) t.extra = true end local i = GetInfo() tag(i) print(i.extra)").is_empty());
    // A type without `values` may have any field
    assert!(warnings("local l = GetLoose() print(l.anything)").is_empty());
    // Or fields that can't be named: under a computed key, by `rawset`, a
    // mixin or a metatable, wherever that is
    assert!(warnings("local i = GetInfo() local k = 'extra' i[k] = 1 print(i.extra)").is_empty());
    assert!(warnings("local i = GetInfo() print(i.extra) local function f() for k, v in pairs({}) do i[k] = v end end f()").is_empty());
    assert!(warnings("local i = GetInfo() rawset(i, 'extra', 1) print(i.extra)").is_empty());
    assert!(warnings("local i = GetInfo() Mixin(i, {extra = 1}) print(i.extra)").is_empty());
    assert!(warnings("local i = GetInfo() setmetatable(i, {__index = {extra = 1}}) print(i.extra)").is_empty());
    // Numbered entries don't give it names
    assert_eq!(warnings("local i = GetInfo() i[1] = true print(i.extra)"), ["Info has no field `extra`"]);
    // Nor methods, unless the addon gives it some
    assert_eq!(warnings("local i = GetInfo() i:Describe()"), ["Info has no method `Describe`"]);
    assert!(warnings("local i = GetInfo() i:Describe() function i:Describe() end").is_empty());
    assert!(warnings("local i = GetInfo() i:Describe() Mixin(i, {Describe = function() end})").is_empty());
    assert!(warnings("local l = GetLoose() l:Anything()").is_empty());
    // Calling a field that isn't a function
    assert_eq!(warnings("local i = GetInfo() i.count()"), ["`count` is number, not a function"]);
    assert_eq!(warnings("local i = GetInfo() i:link()"), ["`link` is string|nil, not a function"]);
    assert_eq!(warnings("local t = {n = 1} t.n()"), ["`n` is number, not a function"]);
    assert_eq!(warnings("local t = {} t.flag = true t:flag()"), ["`flag` is true, not a function"]);
    // Not when it may be one, or what it is is a guess
    assert!(warnings("local t = {n = 1} t.n = function() end t.n()").is_empty());
    assert!(warnings("local t = {c = setmetatable({}, {__call = function() end})} t.c()").is_empty());
    assert!(warnings("local function f(x) x.n() end local a = {} a.n = 1 f(a) local b = {} b.n = print f(b)").is_empty());
    assert_eq!(warnings("local function f(x) x.n() end local a = {} a.n = 1 f(a)"), ["`n` is number, not a function"]);
    assert!(warnings("local t = {} t.n()").is_empty());
    // Through a string key
    assert_eq!(warnings("local i = GetInfo() i['count']()"), ["`count` is number, not a function"]);
    assert_eq!(warnings("local t = {n = 's'} t['n']()"), ["`n` is string, not a function"]);
    assert!(warnings("local t = {n = print} t['n']() local k = 'n' t[k]()").is_empty());
    // A local variable
    assert_eq!(warnings("local n = 1 n()"), ["`n` is number, not a function"]);
    assert_eq!(warnings("local f = x and 'a' or 2 f()"), ["`f` is number|string, not a function"]);
    assert_eq!(warnings("local n = 1 local function g() n() end g()"), ["`n` is number, not a function"]);
    assert!(warnings("local n = 1 n = print n()").is_empty());
    assert!(warnings("local f = x and 1 or print f()").is_empty());
    assert!(warnings("local f if x then f = print end f()").is_empty());
    assert!(warnings("local f = 1 local function g() f() end f = print g()").is_empty());
    assert!(warnings("local t = setmetatable({}, {__call = print}) t()").is_empty());
    assert!(warnings("local f f()").is_empty());
    // A parameter, when every call is seen passing one
    assert_eq!(warnings("local function g(cb) cb() end g(1) g('a')"), ["`cb` is number|string, not a function"]);
    assert!(warnings("local function g(cb) cb() end g(1) g(print)").is_empty());
    // Not once the function escapes, so other code may call it
    assert!(warnings("local function g(cb) cb() end g(1) Register(g)").is_empty());
    assert!(warnings("T = {} function T.g(cb) cb() end T.g(1) local h = T.g").is_empty());
    // A global, by every value the files give it
    assert_eq!(warnings("G = 1 G()"), ["`G` is number, not a function"]);
    assert_eq!(warnings("G = 1 _G.G()"), ["`G` is number, not a function"]);
    assert!(warnings("G = 1 local function f() G() end G = print f()").is_empty());
    assert!(warnings("Unset()").is_empty());
}

#[test]
fn missing_methods() {
    assert_eq!(warnings("local f = CreateFrame('Frame') f:SetPiont()"), ["Frame has no method `SetPiont`"]);
    assert_eq!(warnings("local f = CreateFrame('Frame') f:Click()"), ["Frame has no method `Click`"]);
    assert_eq!(warnings("local f = CreateFrame('Frame') local c = f:GetChild() c:Nope()"), ["Frame has no method `Nope`"]);
    assert_eq!(warnings("local t = CreateFrame('Frame'):CreateTexture() t:Nope()"), ["Texture has no method `Nope`"]);
    // Known, inherited, or added by the addon
    assert!(warnings("local f = CreateFrame('Button') f:SetPoint() f:Click() f:CreateTexture()").is_empty());
    assert!(warnings("local f = CreateFrame('Frame') function f:OnEvent() end f:OnEvent()").is_empty());
    assert!(warnings("local M = {} function M:Extra() end local f = CreateFrame('Frame') Mixin(f, M) f:Extra()").is_empty());
    assert!(warnings("local f = CreateFrame('Frame') f:Later() function SomeTable:Later() end").is_empty());
}

#[test]
fn requirements_include_subtypes() {
    // Required to be a Region, which a Frame is, so it could have Frame methods
    let source = "local function Row(frame) Anchor(frame) frame:CreateTexture() frame:Anything() end";
    assert!(warnings(source).is_empty());
    assert_eq!(widget_types_of(source, "frame")[0], "Region");
    // But a documented return is exactly what it says
    assert_eq!(warnings("local p = GetFrame() p:Click()"), ["Frame has no method `Click`"]);
    assert_eq!(warnings("local t = GetFrame():CreateTexture() t:Nope()"), ["Texture has no method `Nope`"]);
    // Two requirements that agree give the narrower
    let source = "local function Row(frame) Anchor(frame) Parent(frame) end";
    assert_eq!(widget_types_of(source, "frame")[0], "Frame");
}

#[test]
fn parents_from_creation() {
    assert_eq!(widget_types_of("local p = CreateFrame('Frame', nil, UIParent):GetParent()", "p"), ["Frame"]);
    let source = "local b = CreateFrame('Button') local t = b:CreateTexture() local p = t:GetParent()";
    assert_eq!(widget_types_of(source, "p"), ["Button"]);
    // So methods of the real parent are known
    assert!(warnings("local b = CreateFrame('Button') local t = b:CreateTexture() t:GetParent():Click()").is_empty());
    assert_eq!(warnings("local f = CreateFrame('Frame') local t = f:CreateTexture() t:GetParent():Click()"), ["Frame has no method `Click`"]);
    // Made without one
    assert_eq!(widget_types_of("local p = CreateFrame('Frame'):GetParent()", "p"), ["nil"]);
    assert_eq!(widget_types_of("local p = CreateFrame('Frame', nil, nil):GetParent()", "p"), ["nil"]);
}

#[test]
fn parents_from_set_parent() {
    // Anywhere in the files, as it could run before GetParent does
    let source = "local f = CreateFrame('Frame') local p = f:GetParent() f:SetParent(UIParent)";
    assert_eq!(widget_types_of(source, "p"), ["Frame?"]);
    // A Button or UIParent: either, with the methods both have
    let source = "local b = CreateFrame('Button', nil, UIParent) b:SetParent(CreateFrame('Button')) local p = b:GetParent()";
    assert_eq!(widget_types_of(source, "p"), ["Frame|Button"]);
    assert!(warnings(&format!("{source} p:Show()")).is_empty());
    assert_eq!(warnings(&format!("{source} p:Click()")), ["This may be a Frame, which has no method `Click`"]);
}

#[test]
fn several_objects_kept_apart() {
    let made = "local holder = CreateFrame('Frame') local other = CreateFrame('Button')
        local a = CreateFrame('Button', nil, holder) local b = CreateFrame('Button', nil, other, 'ButtonT') local x = c and a or b ";
    // Each with its own parent
    assert_eq!(widget_types_of(&format!("{made} local p = x:GetParent()"), "p"), ["Button|Frame"]);
    // A field set on it may be on either; one only one has isn't known
    assert_eq!(widget_types_of(&format!("{made} x.label = 's' local l = a.label"), "l"), ["string"]);
    assert_eq!(widget_types_of(&format!("{made} a.only = 1 local o = x.only"), "o")[0], "unknown");
    assert_eq!(widget_types_of(&format!("{made} a.n = 1 b.n = 2 local n = x.n"), "n"), ["number"]);
    // Mixed into whichever it is
    let mixed = format!("{made} local M = {{}} function M:Go() return 1 end Mixin(x, M) local g = a:Go()");
    assert_eq!(widget_types_of(&mixed, "g"), ["number"]);
    // A parent set on whichever it is
    assert_eq!(widget_types_of(&format!("{made} x:SetParent(UIParent) local p = a:GetParent()"), "p"), ["Frame"]);
    // Plain objects of one type are still kept apart, but shown once
    assert_eq!(widget_types_of("local f = c and CreateFrame('Frame') or CreateFrame('Frame')", "f"), ["Frame"]);
    // Past the most kept apart: the type they all are, which may have more
    let many: String = (0..9).map(|i| format!("if c{i} then f = CreateFrame('Button', nil, nil, 'T{i}') end ")).collect();
    let source = format!("local f = CreateFrame('Button') {many} local g = f g:Nope()");
    assert_eq!(widget_types_of(&source, "g")[0], "Button");
    assert!(warnings(&source).is_empty());
}

#[test]
fn animations_of_the_kind_named() {
    let group = "local g = CreateFrame('Frame'):CreateAnimationGroup() ";
    assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation('alpha')"), "a"), ["Alpha"]);
    assert!(warnings(&format!("{group} local a = g:CreateAnimation('Alpha') a:SetFromAlpha() a:Play()")).is_empty());
    // Not a kind of animation, not a literal, or none: an Animation
    assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation('Button')"), "a"), ["Animation"]);
    assert_eq!(widget_types_of(&format!("{group} local a = g:CreateAnimation(kind)"), "a"), ["Animation"]);
    assert_eq!(warnings(&format!("{group} g:CreateAnimation():SetFromAlpha()")), ["Animation has no method `SetFromAlpha`"]);
}

#[test]
fn copied_tables() {
    let made = "local t = { n = 1, sub = { s = 'x' }, list = { 1, 2 } } local M = {} function M:Go() return true end Mixin(t, M)
        local c = CopyTable(t) ";
    // What it holds, however deep, and its mixins'
    assert_eq!(widget_types_of(&format!("{made} local n, s, l, g = c.n, c.sub.s, c.list, c:Go()"), "n"), ["number"]);
    assert_eq!(widget_types_of(&format!("{made} local n, s = c.n, c.sub.s"), "s"), ["string"]);
    assert_eq!(widget_types_of(&format!("{made} local l = c.list"), "l"), ["number[]"]);
    assert_eq!(widget_types_of(&format!("{made} local g = c:Go()"), "g"), ["true"]);
    // A table of its own, its tables copied too: writes to it don't reach the original
    assert_eq!(widget_types_of(&format!("{made} c.added = 1 c.sub.extra = 1 local a, e = t.added, t.sub.extra"), "a"), ["unknown"]);
    assert_eq!(widget_types_of(&format!("{made} c.sub.extra = 1 local e = t.sub.extra"), "e"), ["unknown"]);
    assert_eq!(widget_types_of(&format!("{made} c.sub.extra = 1 local e = c.sub.extra"), "e"), ["number"]);
    // A shallow copy shares the tables it holds
    assert_eq!(widget_types_of(&format!("{made} local h = CopyTable(t, true) h.sub.extra = 1 local e = t.sub.extra"), "e"), ["number"]);
    // A table holding itself is shared rather than copied forever
    assert_eq!(widget_types_of("local t = { n = 1 } t.me = t local c = CopyTable(t) local n = c.me.n", "n"), ["number"]);
    // `CopyTableSafe` gives nil for nil
    assert_eq!(widget_types_of("local t = c and { n = 1 } or nil local s = CopyTableSafe(t)", "s"), ["table?"]);
    // Not a table made in the files: as the definitions say
    assert_eq!(widget_types_of("local c = CopyTable(unknown)", "c"), ["table"]);
    // Returned from a method, so each call's is new: still what it holds
    let returned = "local M = {} function M:Init(data) self.data = data end
        function M:Items() return CopyTable(self.data.items) end
        local o = CreateAndInitFromMixin(M, { items = { 'a' } }) local y = o:Items() local h = y[1]";
    assert_eq!(library_types_of(returned, "h"), ["string?"]);
    // Any of several tables: what any of them holds
    assert_eq!(widget_types_of("local t = x and { n = 1 } or { s = 'a' } local c = CopyTable(t) local n, s = c.n, c.s", "n"), ["number"]);
    assert_eq!(widget_types_of("local t = x and { n = 1 } or { s = 'a' } local c = CopyTable(t) local s = c.s", "s"), ["string"]);
}

/// Types at each use of `name` in `source`, with `saved` the globals the
/// game saves between sessions
fn saved_types_of(source: &str, saved: &[&str], name: &str) -> Vec<String> {
    let root = SyntaxNode::new_root(Generator::new(source).process_all());
    let saved: Vec<String> = saved.iter().map(|s| s.to_string()).collect();
    let files = [SourceFile { root, addon: None, handler: None }];
    describe_uses(&Analysis::analyse_over(&files, &library(), Arc::default(), &saved), name)
}

#[test]
fn saved_variables_hold_what_the_addon_writes() {
    // A profile filled with defaults, which an imported table of any shape
    // may replace
    let source = "local Defaults = { a = 1, b = 'x' }
        function Init()
          if SV == nil then SV = { Profiles = { DEFAULT = {} } } end
          local profile = SV.Profiles[Current] or SV.Profiles.DEFAULT
          for k, v in pairs(Defaults) do
            if profile[k] == nil then profile[k] = v end
          end
          Profile = profile
        end
        function Import(data, name) if type(data) == 'table' then SV.Profiles[name] = data end end
        function Get(name) return Profile[name] end
        local got = Get('a')";
    assert_eq!(saved_types_of(source, &["SV"], "got"), ["number|string|nil"]);
    // Not saved: the import may hold anything, so what's read from it may be
    assert_eq!(saved_types_of(source, &[], "got"), ["unknown"]);
    // Of a value that may also be such a table, the rest is kept
    let source = "SV = { list = {} }
        function Add(t) if type(t) == 'table' then SV.list[#SV.list + 1] = x and t or 1 end end
        local got = SV.list[1]";
    assert_eq!(saved_types_of(source, &["SV"], "got"), ["number?"]);
    assert_eq!(saved_types_of(source, &[], "got"), ["number|table|nil"]);
    // A copy of a saved table into itself doesn't keep what was left out
    let source = "SV = { p = { a = 1 } }
        function Copy() SV.p = CopyTable(SV.p) end
        function Import(data, k) if type(data) == 'table' then SV.p[k] = data end end
        local got = SV.p['a']";
    assert_eq!(saved_types_of(source, &["SV"], "got"), ["number"]);
}

#[test]
fn mixins_the_definitions_give() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // Blizzard's Lua mixins, as tables in the definitions
    let made = "local f = CreateFrame('Frame') Mixin(f, RegistryMixin) local n = f:Count() ";
    assert_eq!(widget_types_of(made, "f")[0], "Frame & RegistryMixin");
    assert_eq!(widget_types_of(made, "n"), ["number"]);
    assert!(warnings(&format!("{made} f:Register() f:Show()")).is_empty());
    assert_eq!(warnings(&format!("{made} f:Nope()")), ["Frame has no method `Nope`"]);
    let table = "local t = CreateFromMixins(RegistryMixin) ";
    assert!(warnings(&format!("{table} t:Register()")).is_empty());
    assert_eq!(warnings(&format!("{table} t:Nope()")), ["No function `Nope` is defined for this table"]);
    // One that may have more, under computed keys
    assert!(warnings("local f = CreateFrame('Frame') Mixin(f, OpenRegistryMixin) f:Nope()").is_empty());
    assert!(warnings("local t = CreateFromMixins(OpenRegistryMixin) t:Nope()").is_empty());
}

#[test]
fn objects_not_made_in_the_files() {
    // The documented parent type
    assert_eq!(widget_types_of("local p = UIParent:GetParent()", "p"), ["Region"]);
    assert_eq!(widget_types_of("local f = GetFrame() local p = f:GetParent()", "p"), ["Region"]);
}

#[test]
fn objects_have_their_own_fields() {
    let source = "local a = CreateFrame('Frame') local b = CreateFrame('Frame') a.x = 1 b.x = 's' local y = a.x";
    assert_eq!(widget_types_of(source, "y"), ["number"]);
    let source = "local f = CreateFrame('Frame') function f:OnEvent() return true end local y = f:OnEvent()";
    assert_eq!(widget_types_of(source, "y"), ["true"]);
    // Still shown as just their type
    assert_eq!(widget_types_of("local f = CreateFrame('Frame')", "f"), ["Frame"]);
}

#[test]
fn mixins_are_what_they_are_mixed_into() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // `self` in a mixin's methods is the frame it's mixed into, with the mixin's fields
    let source = "local M = {} function M:OnLoad() local s, t, n = self, self:GetText(), self:Count() end function M:Count() return 1 end
        local b = CreateFrame('Button') Mixin(b, M)";
    assert_eq!(widget_types_of(source, "s"), ["Button & M"]);
    assert_eq!(widget_types_of(source, "t"), ["string"]);
    assert_eq!(widget_types_of(source, "n"), ["number"]);
    assert!(warnings(source).is_empty(), "{:?}", warnings(source));
    // A missing method is still reported
    assert_eq!(warnings("local M = {} function M:OnLoad() self:Nope() end Mixin(CreateFrame('Button'), M)"), ["Button has no method `Nope`"]);
    // Mixed into a Button and a Frame, so it's only known to be a Frame
    assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end Mixin(CreateFrame('Button'), M) Mixin(CreateFrame('Frame'), M)", "s"), ["Frame & M"]);
    // Through a table made from it
    assert_eq!(widget_types_of("local Base = {} function Base:OnLoad() local s = self end local M = CreateFromMixins(Base)
        Mixin(CreateFrame('Button'), M)", "s"), ["Button & Base"]);
    // Mixed into a plain table too, so it isn't only frames
    assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end Mixin(CreateFrame('Button'), M) Mixin({}, M)", "s"), ["table"]);
    // Never mixed into anything: still a plain table
    assert_eq!(widget_types_of("local M = {} function M:OnLoad() local s = self end", "s"), ["table"]);
    // A template can add methods, so they aren't reported
    assert!(warnings("local M = {} function M:OnLoad() self:Nope() end Mixin(CreateFrame('Button', nil, nil, 'T'), M)").is_empty());
}

#[test]
fn mixins_shown_on_frames() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    let source = "local A, B = {}, {} local f = CreateFrame('Button') Mixin(f, A, B) local g = f
        local h = Mixin(CreateFrame('Frame'), CreateFromMixins(A, B)) local plain = CreateFrame('Frame')
        local maybe = c and f or nil";
    assert_eq!(widget_types_of(source, "g"), ["Button & A & B"]);
    // An inline table of mixins shows what it's made from
    assert_eq!(widget_types_of(source, "h"), ["Frame & A & B"]);
    assert_eq!(widget_types_of(source, "plain"), ["Frame"]);
    assert_eq!(widget_types_of(source, "maybe"), ["(Button & A & B)?"]);
}

#[test]
fn pools_give_their_objects() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    let source = "local M = {} function M:Count() return 1 end
        local pool = CreateFramePool('Button', UIParent, nil, function(p, frame) local r = frame end, false, function(frame) Mixin(frame, M) frame.label = 's' end)
        local got, new = pool:Acquire()
        local n, label, t = got:Count(), got.label, got:GetText()
        for active in pool:EnumerateActive() do local e = active end
        local next = pool:GetNextActive()
        local p = pool";
    assert_eq!(widget_types_of(source, "got")[0], "Button & M");
    assert_eq!(widget_types_of(source, "new"), ["boolean"]);
    // The init function's mixins and fields, and the type's own methods
    assert_eq!(widget_types_of(source, "n"), ["number"]);
    assert_eq!(widget_types_of(source, "label"), ["string"]);
    assert_eq!(widget_types_of(source, "t"), ["string"]);
    // The reset function is given the pool, then the object
    assert_eq!(widget_types_of(source, "r"), ["Button & M"]);
    assert_eq!(widget_types_of(source, "e"), ["Button & M"]);
    assert_eq!(widget_types_of(source, "next"), ["(Button & M)?"]);
    // The reset function's pool parameter, then the pool itself
    assert_eq!(widget_types_of(source, "p"), ["ObjectPool<Button & M>", "ObjectPool<Button & M>"]);
    // `self` in the mixin is the pool's frames
    assert_eq!(widget_types_of("local M = {} function M:Init() local s = self end
        local pool = CreateFramePool('Button', nil, nil, nil, false, function(f) Mixin(f, M) end)", "s"), ["Button & M"]);
    assert!(warnings(source).is_empty(), "{:?}", warnings(source));
}

#[test]
fn pool_callbacks_held_in_variables() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // Named init and reset functions count as called by the pool too
    let source = "local function Reset(pool, fs) local r = fs end
        local pool = CreateFontStringPool(UIParent, 'OVERLAY', 0, nil, Reset) local s = pool:Acquire()";
    assert_eq!(widget_types_of(source, "r"), ["FontString"]);
    assert_eq!(widget_types_of(source, "s"), ["FontString"]);
    // Object pools hold what their create function returns
    let source = "local M = { v = 1 } local pool = CreateObjectPool(function(p) local f = CreateFrame('Frame') Mixin(f, M) return f end)
        local o = pool:Acquire() local v = o.v";
    assert_eq!(widget_types_of(source, "o")[0], "Frame & M");
    assert_eq!(widget_types_of(source, "v"), ["number"]);
    // A template can add members
    assert!(warnings("local pool = CreateFramePool('Button', nil, 'T') pool:Acquire():Anything()").is_empty());
    assert_eq!(warnings("local pool = CreateFramePool('Button') pool:Acquire():Anything()"), ["Button has no method `Anything`"]);
}

#[test]
fn pools_made_by_functions() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // Coolinator's GeneratePool: the init function mixes in a parameter of
    // the function making the pool, so each call's pool has its own mixin
    let analysis = analyse_files_with(&[
        "local _, ns = ... function ns.GeneratePool(mixin, template)
            return CreateFramePool('Frame', UIParent, template or 'T', function(_, frame) frame:Hide() end, false, function(frame)
                Mixin(frame, mixin) frame:OnLoad() end)
        end",
        "local _, ns = ... ns.A = {} function ns.A:OnLoad() local sa = self end function ns.A:Count() return 1 end
         ns.B = {} function ns.B:OnLoad() end function ns.B:Name() return 's' end",
        "local _, ns = ... local pools = { a = ns.GeneratePool(ns.A), b = ns.GeneratePool(ns.B) }
         local pa, pb = pools.a, pools.b
         local a, b = pools.a:Acquire(), pools.b:Acquire()
         local x, y = a:Count(), b:Name()",
    ], &widgets());
    assert_eq!(types_in(&analysis, 2, "pa"), ["ObjectPool<Frame & ns.A>"]);
    assert_eq!(types_in(&analysis, 2, "pb"), ["ObjectPool<Frame & ns.B>"]);
    assert_eq!(types_in(&analysis, 2, "a"), ["Frame & ns.A", "Frame & ns.A"]);
    assert_eq!(types_in(&analysis, 2, "b"), ["Frame & ns.B", "Frame & ns.B"]);
    assert_eq!(types_in(&analysis, 2, "x"), ["number"]);
    assert_eq!(types_in(&analysis, 2, "y"), ["string"]);
    assert_eq!(types_in(&analysis, 1, "sa"), ["Frame & ns.A"]);
}

#[test]
fn hovering_calls_shows_what_they_return() {
    let source = "local M = {} local pool = CreateFramePool('Button', nil, nil, nil, false, function(f) Mixin(f, M) end)
        local got = pool:Acquire() local b = CreateFrame('Button')";
    let analysis = analyse_with(source, &widgets());
    let hover = |text: &str| analysis.hover(FileId(0), TextSize::new(source.find(text).unwrap() as u32)).unwrap().0;
    // Hover shows the native type, not the mixins it's made with
    assert_eq!(hover("Acquire"), "field pool:Acquire: function() -> Button, boolean");
    assert_eq!(hover("CreateFrame('Button')"), "global CreateFrame: function(frameType: string, name: string?, parent: Frame?, template: string?) -> Button");
}

#[test]
fn own_fields_win_over_unknown_tables() {
    // Another table of unknown identity using the same field name doesn't
    // blur what this table's field holds
    let source = "local t = { cooldown = CreateFramePool('Frame') }
        local function f(entry) entry.texts.cooldown = { scale = 1 } end
        local p = t.cooldown local o = {} local v = o.cooldown";
    assert_eq!(widget_types_of(source, "p"), ["ObjectPool<Frame>"]);
    // With no field of its own, it could be the unknown table
    assert_eq!(widget_types_of(source, "v"), ["table"]);
}

#[test]
fn mixins_given_to_functions_making_frames() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    // Coolinator's InitFrameWithMixin, with the frame made in another file
    let analysis = analyse_objects(&[
        "local _, ns = ... function ns.Init(parent, mixin) local f = CreateFrame('Frame', nil, parent) Mixin(f, mixin) return f end",
        "local _, ns = ... ns.A = {} function ns.A:OnLoad() local s = self self:Show() end",
        "local _, ns = ... local a = ns.Init(UIParent, ns.A)",
    ]);
    assert_eq!(types_in(&analysis, 1, "s"), ["Frame & ns.A"]);
    assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
}

#[test]
fn templates_on_created_children() {
    assert!(warnings("local t = CreateFrame('Frame'):CreateFontString(nil, 'Template') t:Anything()").is_empty());
    assert_eq!(warnings("local t = CreateFrame('Frame'):CreateFontString(nil, nil) t:Anything()"), ["Texture has no method `Anything`"]);
}

#[test]
fn frames_made_from_templates() {
    let made = "local f = CreateFrame('Button', nil, nil, 'ButtonT') ";
    // Hover shows the native type, not the template
    assert_eq!(widget_types_of(made, "f"), ["Button"]);
    let either = "local f = c and CreateFrame('Button', nil, nil, 'ButtonT') or CreateFrame('Button') ";
    assert_eq!(widget_types_of(either, "f"), ["Button"]);
    // What the objects are made with
    let _made_with = MadeWith::shown();
    assert_eq!(widget_types_of(made, "f"), ["ButtonT"]);
    // Its own methods, its mixins', those of what it inherits, and its children
    assert!(warnings(&format!("{made} f:Init() f:Base() f:Click() f.Label:SetText() f.Icon:Show()")).is_empty());
    assert_eq!(warnings(&format!("{made} f:Nope()")), ["ButtonT has no method `Nope`"]);
    // Its own mixin's method wins over what it inherits
    assert_eq!(widget_types_of(&format!("{made} local v = f:Value()"), "v"), ["number"]);
    assert_eq!(widget_types_of(&format!("{made} local v = f.flag"), "v"), ["boolean"]);
    assert_eq!(widget_types_of(&format!("{made} local v = f.Tabs"), "v"), ["table"]);
    // A font string inheriting a font only takes its settings
    assert_eq!(widget_types_of(&format!("{made} local l = f.Label"), "l"), ["FontString"]);
    assert_eq!(warnings(&format!("{made} f.Label:Nope()")), ["FontString has no method `Nope`"]);
    // From a pool, or made by a frame
    assert_eq!(widget_types_of("local p = CreateFramePool('Button', nil, 'ButtonT') local f = p:Acquire()", "f"), ["ButtonT"]);
    assert_eq!(widget_types_of("local t = UIParent:CreateFontString(nil, 'TextureT') local v = t.v", "v"), ["number"]);
    // Either a frame made from it or a plain one: the methods both have
    let either = "local f = c and CreateFrame('Button', nil, nil, 'ButtonT') or CreateFrame('Button') ";
    assert_eq!(widget_types_of(either, "f"), ["Button|ButtonT"]);
    assert!(warnings(&format!("{either} f:Click() f:Show()")).is_empty());
    assert_eq!(warnings(&format!("{either} f:Init()")), ["This may be a Button, which has no method `Init`"]);
    assert_eq!(warnings(&format!("{either} f:Nope()")), ["Neither Button nor ButtonT has a method `Nope`"]);
}

#[test]
fn templates_that_cant_be_used() {
    // For another kind of frame, several at once, or not known: the kind
    // of frame made, which the template can add methods to
    assert_eq!(widget_types_of("local f = CreateFrame('Frame', nil, nil, 'ButtonT')", "f"), ["Frame"]);
    assert!(warnings("CreateFrame('Frame', nil, nil, 'ButtonT'):Nope()").is_empty());
    assert!(warnings("CreateFrame('Button', nil, nil, 'ButtonT, BaseT'):Nope()").is_empty());
    assert!(warnings("CreateFrame('Button', nil, nil, name):Nope()").is_empty());
    // With mixins whose methods aren't all known
    assert!(warnings("CreateFrame('Frame', nil, nil, 'OpenT'):Nope()").is_empty());
    assert!(warnings("CreateFrame('Frame', nil, nil, 'MissingT'):Nope()").is_empty());
}

#[test]
fn named_frames() {
    assert_eq!(widget_types_of("local f = Plain", "f"), ["Frame"]);
    assert_eq!(warnings("Plain.Inner:Init() Plain.Inner:Click() Plain.Inner:Nope()"), ["Button has no method `Nope`"]);
    assert!(warnings("Opened:Nope()").is_empty());
    // One the definitions give otherwise keeps that type
    assert!(warnings("UIParent:Click()").len() == 1);
}

/// The widgets' definitions with the templates and frames of XML file `xml`
fn with_xml(xml: &str) -> Arc<Definitions> {
    let definitions = widgets();
    let document = crate::xml::parse(xml).unwrap();
    let objects = crate::xml::objects(&document, &|tag: &str| definitions.element(tag).is_some());
    Arc::new(definitions.with_xml(&objects, crate::xml::sites(&objects, "/addon/Frames.xml", xml)))
}

const ADDON_XML: &str = r#"<Ui>
    <Button name="AddonT" virtual="true" inherits="ButtonT" mixin="AddonMixin">
        <KeyValues><KeyValue key="size" value="2" type="number"/></KeyValues>
        <Layers><Layer><Texture parentKey="Glow"/></Layer></Layers>
    </Button>
    <Frame name="AddonFrame" mixin="FrameMixin">
        <Frames><Button name="$parentClose" parentKey="Close" inherits="AddonT"/></Frames>
    </Frame>
    <Frame name="UnknownT" virtual="true" mixin="NowhereMixin"/>
    <Frame name="LoneT" virtual="true" mixin="LoneMixin"/>
    <Frame name="Placed" parent="UIParent"><Frames><Button parentKey="Inner" mixin="InnerMixin"/></Frames></Frame>
    <Odd name="OddT" virtual="true"/>
</Ui>"#;

#[test]
fn addon_templates() {
    // What the objects are made with, which hover doesn't show
    let _made_with = MadeWith::shown();
    let definitions = with_xml(ADDON_XML);
    let warnings = |source: &str| -> Vec<String> { analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect() };
    let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
    let mixin = "AddonMixin = {count = 1} function AddonMixin:Extra() end ";
    // Made from one: its XML, what it inherits, and its Lua mixin's fields
    let made = format!("{mixin} local f = CreateFrame('Button', nil, nil, 'AddonT') local n, s = f.count, f.size ");
    assert_eq!(types_of(&made, "f")[0], "AddonT & AddonMixin");
    assert_eq!(types_of(&made, "n")[0], "number");
    assert_eq!(types_of(&made, "s")[0], "number");
    assert!(warnings(&format!("{made} f:Extra() f:Init() f:Click() f.Glow:Show()")).is_empty());
    assert_eq!(warnings(&format!("{made} f:Nope()")), ["AddonT has no method `Nope`"]);
    // In the mixin's methods, `self` is what XML mixes it into
    let own = format!("{mixin} function AddonMixin:OnLoad() local s = self self:Init() self:Extra() self.Glow:Show() end");
    assert_eq!(types_of(&own, "s")[0], "AddonT & AddonMixin");
    assert!(warnings(&own).is_empty());
    assert_eq!(warnings(&format!("{mixin} function AddonMixin:OnLoad() self:Nope() end")), ["AddonT has no method `Nope`"]);
    // A named frame, and its children by key and name
    let frame = format!("{mixin} FrameMixin = {{}} function FrameMixin:OnLoad() self.Close:Extra() end local a, c = AddonFrame, AddonFrameClose");
    assert_eq!(types_of(&frame, "a")[0], "Frame");
    // Of its own type, as it's an object of its own, shown as its kind
    assert_eq!(types_of(&frame, "c")[0], "Button");
    assert!(warnings(&format!("{frame} AddonFrame.Close:Extra() c:Click()")).is_empty());
    // Without its mixin's table, its methods aren't known
    assert!(warnings("local f = CreateFrame('Button', nil, nil, 'AddonT') f:Nope()").is_empty());
    assert!(warnings("CreateFrame('Frame', nil, nil, 'UnknownT'):Nope()").is_empty());
    // A kind of frame wow_ls doesn't know may have more methods
    assert!(warnings("CreateFrame('Frame', nil, nil, 'OddT'):Nope()").is_empty());
}

#[test]
fn definitions_in_addon_xml() {
    let definitions = with_xml(ADDON_XML);
    let line_of = |text: &str| ADDON_XML[..ADDON_XML.find(text).unwrap()].matches('\n').count() as u32 + 1;
    let source = "AddonMixin = {}\nfunction AddonMixin:Extra() end\nlocal f = CreateFrame('Button', nil, nil, 'AddonT')\nprint(f.Glow, AddonFrame.Close)\nf:Extra()";
    let analysis = analyse_with(source, &definitions);
    let at = |text: &str| TextSize::from((source.find(text).unwrap() + 1) as u32);
    // A template's child, and a named frame's, in the XML
    assert_eq!(analysis.external_definition(FileId(0), at("Glow")), Some(("/addon/Frames.xml".into(), line_of("<Texture parentKey=\"Glow\""))));
    assert_eq!(analysis.external_definition(FileId(0), at("Close")), Some(("/addon/Frames.xml".into(), line_of("<Button name=\"$parentClose\""))));
    assert_eq!(analysis.external_definition(FileId(0), at("AddonFrame")), Some(("/addon/Frames.xml".into(), line_of("<Frame name=\"AddonFrame\""))));
    // A method from the template's Lua mixin, in the files
    let extra = source.rfind("Extra").unwrap() + 1;
    let sites = analysis.definition(FileId(0), TextSize::from(extra as u32));
    assert_eq!(sites.iter().map(|s| &source[usize::from(s.range.start())..][..5]).collect::<Vec<_>>(), ["Extra"]);
    assert!(usize::from(sites[0].range.start()) < source.find("local f").unwrap());
    // Defined in the files: not outside them
    assert_eq!(analysis.external_definition(FileId(0), TextSize::from(extra as u32)), None);
}

#[test]
fn parents_of_addon_mixins() {
    let definitions = with_xml(ADDON_XML);
    let warnings = |source: &str| -> Vec<String> { analyse_with(source, &definitions).diagnostics.iter().map(|d| d.message.clone()).collect() };
    let types_of = |source: &str, name: &str| describe_uses(&analyse_with(source, &definitions), name);
    // What its frames are made in: by `CreateFrame` with its template, and
    // inside other objects in XML
    let lone = "LoneMixin = {} function LoneMixin:OnLoad() local p = self:GetParent() end ";
    assert_eq!(types_of(&format!("{lone} CreateFrame('Frame', nil, CreateFrame('Button'), 'LoneT')"), "p")[0], "Button");
    // That Button, and `AddonFrame`, whose `Close` inherits the template
    let made = "AddonMixin = {} function AddonMixin:OnLoad() local p = self:GetParent() end ";
    assert_eq!(types_of(&format!("{made} CreateFrame('Button', nil, CreateFrame('Button'), 'AddonT')"), "p")[0], "Frame|Button");
    let inner = "InnerMixin = {} function InnerMixin:OnLoad() local p = self:GetParent() end";
    assert_eq!(types_of(inner, "p")[0], "Frame");
    assert!(warnings(&format!("{inner} function InnerMixin:Go() self:GetParent():Show() end")).is_empty());
    // Its frames may be made where the analysis can't see, so the parent
    // isn't one to report missing methods by
    assert!(warnings(&format!("{made} CreateFrame('Button', nil, CreateFrame('Button'), 'AddonT') function AddonMixin:Go() self:GetParent():Nope() end")).is_empty());
    // Made nowhere it can see: the documented type
    let lone = "LoneMixin = {} function LoneMixin:OnLoad() local p = self:GetParent() p:Click() end";
    assert_eq!(types_of(lone, "p")[0], "Region");
    assert!(warnings(lone).is_empty());
}

#[test]
fn missing_methods_only_when_sure() {
    // Made from a template, which can add methods
    assert!(warnings("local f = CreateFrame('Frame', nil, nil, 'BackdropTemplate') f:SetBackdrop()").is_empty());
    // Not one known WoW type
    assert!(warnings("local f = c and CreateFrame('Frame') or 's' f:Nope()").is_empty());
    assert!(warnings("local f = c and CreateFrame('Frame') or {} f:Nope()").is_empty());
    assert!(warnings("unknown:Nope()").is_empty());
    // A type without known methods, like a data structure
    assert!(warnings("local k = GetItemKey() k:Nope()").is_empty());
    // Recorded once, even in loops
    assert_eq!(warnings("local f = CreateFrame('Frame') while c do f:Nope() end").len(), 1);
}

#[test]
fn undefined_table_methods() {
    let warnings = |source: &str| -> Vec<String> {
        analyse_with(source, &library()).diagnostics.iter().map(|d| d.message.clone()).collect()
    };
    let missing = |name: &str| vec![format!("No function `{name}` is defined for this table")];
    assert_eq!(warnings("local t = {} t:Tree()"), missing("Tree"));
    assert_eq!(warnings("local t = {} function t:Grow() self:Tree() end t:Grow()"), missing("Tree"));
    assert_eq!(warnings("local t = {} while c do t:Tree() end"), missing("Tree"));
    // A parameter given that table at every call
    assert_eq!(warnings("local function f(t) t:Tree() end f({})"), missing("Tree"));
    // Through `__index` to a table that doesn't have it either
    assert_eq!(warnings("local C = {} C.__index = C function C:Grow() end local o = setmetatable({}, C) o:Grow() o:Tree()"), missing("Tree"));
    // Defined on this table, later on, or on any other table
    assert!(warnings("local t = {} t:Tree() function t:Tree() end").is_empty());
    assert!(warnings("local t = {} t:Tree() local u = {} u.Tree = function() end").is_empty());
    assert!(warnings("local t = {} t:Tree() function SomeGlobal:Tree() end").is_empty());
    // A field that isn't a function is another problem, reported as that
    assert_eq!(warnings("local t = { Tree = 1 } t:Tree()"), ["`Tree` is number, not a function"]);
    // A WoW object's method, so it may be a mixin for one
    assert!(warnings("local M = {} function M:OnLoad() self:SetScript() end").is_empty());
    // Somewhere the analysis can't see into
    assert!(warnings("local t = {} Mixin(t, UnknownMixin) t:Tree()").is_empty());
    assert!(warnings("local t = setmetatable({}, { __index = function() end }) t:Tree()").is_empty());
    assert!(warnings("local t = setmetatable({}, { __index = Unknown }) t:Tree()").is_empty());
    assert!(warnings("local t = setmetatable({}, Unknown) t:Tree()").is_empty());
    assert!(warnings("local t = {} t[name] = function() end t:Tree()").is_empty());
    // Tables the analysis can't pin down: parameters, and what calls it can't see into give
    assert!(warnings("function F(t) t:Tree() end").is_empty());
    assert!(warnings("local t = c and {} or {} t:Tree()").is_empty());
    assert!(warnings("local _, ns = ... local t = ns.Make() t:Tree()").is_empty());
}

#[test]
fn reference_lookup() {
    let source = "local x = 5\nlocal y = x";
    let file = analyse(source);
    let r = file.reference_at(FileId(0), TextSize::from(22)).expect("reference at x");
    assert_eq!(&*file.binding(r).name, "x");
    assert!(!r.write);
    assert!(file.reference_at(FileId(0), TextSize::from(3)).is_none());
}

/// Go to definition from the `@` in one of `sources` (analysed together
/// as an addon), as the file and the text from each site to the end of
/// its line
fn definitions_at(sources: &[&str]) -> Vec<(u32, String)> {
    definitions_with(sources, &library())
}

fn definitions_with(sources: &[&str], definitions: &Arc<Definitions>) -> Vec<(u32, String)> {
    let (file, offset) = sources.iter().enumerate()
        .find_map(|(i, s)| s.find('@').map(|at| (i, at)))
        .expect("a cursor");
    let sources: Vec<String> = sources.iter().map(|s| s.replace('@', "")).collect();
    let files: Vec<SourceFile> = sources.iter()
        .map(|source| SourceFile { root: SyntaxNode::new_root(Generator::new(source).process_all()), addon: Some(0), handler: None })
        .collect();
    let analysis = Analysis::analyse(&files, definitions);
    analysis.definition(FileId(file as u32), TextSize::from(offset as u32)).iter().map(|site| {
        let rest = &sources[site.file.0 as usize][usize::from(site.range.start())..];
        (site.file.0, rest.lines().next().unwrap_or_default().to_string())
    }).collect()
}

fn at(file: u32, text: &str) -> (u32, String) {
    (file, text.to_string())
}

#[test]
fn definitions_of_variables() {
    // A local's or parameter's declaration, not its later assignments
    assert_eq!(definitions_at(&["local x = 1\nx = 2\nprint(@x)"]), [at(0, "x = 1")]);
    assert_eq!(definitions_at(&["local function f(a, b)\nreturn @b end"]), [at(0, "b)")]);
    assert_eq!(definitions_at(&["local function f()\nend\n@f()"]), [at(0, "f()")]);
    // The innermost of shadowed ones
    assert_eq!(definitions_at(&["local x = 1\ndo local x = 2\nprint(@x) end"]), [at(0, "x = 2")]);
    // Every assignment to a global, in any file
    assert_eq!(definitions_at(&["Count = 0", "function Count()\nend\nprint(@Count)"]), [at(0, "Count = 0"), at(1, "Count()")]);
    // Nothing for one the files never assign
    assert!(definitions_at(&["print(@Unknown)"]).is_empty());
    // `self` is declared by the method's name
    assert_eq!(definitions_at(&["local M = {}\nfunction M.Inner:Go()\nprint(@self) end"]), [at(0, "Go()")]);
    assert!(definitions_at(&["local M = {}\nfunction M.Go()\nprint(@self) end"]).is_empty());
}

#[test]
fn definitions_of_copied_fields() {
    // Where the original's field is set, however deep
    let source = "local t = { n = 1, sub = { s = 'x' } }\nlocal c = CopyTable(t)\nprint(c.@n)";
    assert_eq!(definitions_with(&[source], &widgets()), [at(0, "n = 1, sub = { s = 'x' } }")]);
    let source = "local t = { n = 1, sub = { s = 'x' } }\nlocal c = CopyTable(t)\nprint(c.sub.@s)";
    assert_eq!(definitions_with(&[source], &widgets()), [at(0, "s = 'x' } }")]);
    // Set on the copy itself: there
    let source = "local t = { n = 1 }\nlocal c = CopyTable(t)\nc.n = 2\nprint(c.@n)";
    assert_eq!(definitions_with(&[source], &widgets()), [at(0, "n = 2")]);
}

#[test]
fn definitions_of_fields() {
    // Every assignment to the field, on the table it's read from
    assert_eq!(definitions_at(&["local t = { v = 1 }\nt.v = 2\nlocal y = t.@v"]), [at(0, "v = 1 }"), at(0, "v = 2")]);
    // Not another table's field of the same name
    assert_eq!(definitions_at(&["local a, b = {}, {}\na.v = 1\nb.v = 2\nlocal y = b.@v"]), [at(0, "v = 2")]);
    // Methods, across files through the addon's namespace
    assert_eq!(definitions_at(&["local _, ns = ...\nfunction ns:Init()\nend", "local _, ns = ...\nns:@Init()"]), [at(0, "Init()")]);
    // Dotted names along the way
    assert_eq!(definitions_at(&["local A = { B = {} }\nfunction A.B.Go()\nend\nA.@B.Go()"]), [at(0, "B = {} }")]);
    // From a mixin, or from `__index`
    assert_eq!(definitions_at(&["local M = {}\nfunction M:Init()\nend\nlocal t = Mixin({}, M)\nt:@Init()"]), [at(0, "Init()")]);
    assert_eq!(definitions_at(&["local M = {}\nfunction M:Init()\nend\nlocal f = Mixin(CreateFrame('Frame'), M)\nf:@Init()"]), [at(0, "Init()")]);
    assert_eq!(definitions_at(&["local B = {}\nfunction B:Go()\nend\nlocal t = setmetatable({}, { __index = B })\nt:@Go()"]), [at(0, "Go()")]);
    // `self` in a method is the table it's defined on
    assert_eq!(definitions_at(&["local M = {}\nfunction M:A()\nend\nfunction M:B() self:@A() end"]), [at(0, "A()")]);
    // Fields of tables whose identity isn't known (like globals no file
    // assigns), by name, as reading them falls back on those
    assert_eq!(definitions_at(&["SomeFrame.label = 1\nlocal y = OtherFrame.@label"]), [at(0, "label = 1")]);
    // Each frame made in the files is its own table
    assert!(definitions_at(&["local f = CreateFrame('Frame')\nf.label = 1\nlocal g = CreateFrame('Frame')\nlocal y = g.@label"]).is_empty());
    // String keys are the field of that name, both ways round
    assert_eq!(definitions_at(&["local t = {}\nt['v'] = 1\nlocal y = t.@v"]), [at(0, "'v'] = 1")]);
    assert_eq!(definitions_at(&["local t = { ['v'] = 1 }\nt.v = 2\nlocal y = t[@'v']"]), [at(0, "'v'] = 1 }"), at(0, "v = 2")]);
    // Nothing for a WoW object's own methods, which the files don't define
    assert!(definitions_at(&["local f = CreateFrame('Frame')\nf:@SetPoint('TOP')"]).is_empty());
}
