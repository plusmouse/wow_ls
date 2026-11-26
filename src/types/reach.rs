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

//! How far an edit can reach, by what each body's last record says each key
//! it influenced depends on (`BodyInfluences::get`). A measurement for
//! the incremental analysis design: a reset from a key only needs to cover
//! the keys in a cycle with it, as the rest is rebuilt from the writes

use std::path::PathBuf;

use super::*;

/// The links between keys the records of the first `files` files' bodies
/// give: from each key read to each key influenced with it
struct Links<'a> {
    forward: HashMap<Read, Vec<Read>>,
    backward: HashMap<Read, Vec<Read>>,
    /// The bodies reading each key
    readers: HashMap<Read, Vec<FunctionId>>,
    records: &'a HashMap<FunctionId, Arc<Record>>,
    /// How many bodies there are
    bodies: usize,
}

impl<'a> Links<'a> {
    fn of(analysis: &'a Analysis, files: usize) -> Links<'a> {
        let mut links = Links { forward: HashMap::default(), backward: HashMap::default(), readers: HashMap::default(), records: &analysis.records, bodies: 0 };
        for (id, record) in analysis.records.iter().filter(|(id, _)| (id.file.0 as usize) < files) {
            links.bodies += 1;
            for read in record.own_influences.own_reads().iter() {
                links.readers.entry(read.clone()).or_default().push(*id);
            }
            for (key, deps) in record.own_influences.get().iter() {
                for read in deps.iter() {
                    links.forward.entry(read.clone()).or_default().push(key.clone());
                    links.backward.entry(key.clone()).or_default().push(read.clone());
                }
            }
        }
        links
    }

    /// The keys reached from `seeds` following the links, or following
    /// them back
    fn closure<'b>(&self, seeds: impl IntoIterator<Item = &'b Read>, forward: bool) -> HashSet<Read> {
        let next = if forward { &self.forward } else { &self.backward };
        let mut seen: HashSet<Read> = HashSet::default();
        let mut pending: Vec<Read> = Vec::new();
        for seed in seeds {
            if seen.insert(seed.clone()) {
                pending.push(seed.clone());
            }
        }
        while let Some(key) = pending.pop() {
            for to in next.get(&key).into_iter().flatten() {
                if seen.insert(to.clone()) {
                    pending.push(to.clone());
                }
            }
        }
        seen
    }

    /// The bodies that read any of `keys`, with the functions they're in,
    /// which walking them walks too
    fn bodies_reading(&self, keys: &HashSet<Read>) -> usize {
        let mut all: HashSet<FunctionId> = HashSet::default();
        for id in keys.iter().flat_map(|key| self.readers.get(key).into_iter().flatten()) {
            let mut at = Some(*id);
            while let Some(id) = at {
                if !all.insert(id) {
                    break
                }
                at = self.records.get(&id).and_then(|record| record.parent);
            }
        }
        all.len()
    }

    /// The keys in a cycle with any of `seeds`: reached from it, and
    /// reaching back to it
    fn cycles(&self, seeds: &[Read]) -> HashSet<Read> {
        let mut cycle = HashSet::default();
        for seed in seeds {
            let forward = self.closure([seed], true);
            let backward = self.closure([seed], false);
            cycle.extend(forward.intersection(&backward).cloned());
        }
        cycle
    }
}

/// How many of the first `files` files' bodies reading what `seeds`
/// influence would be walked again, and of how many
pub(super) fn reach_from(analysis: &Analysis, seeds: &[Read], files: usize) -> (usize, usize) {
    let links = Links::of(analysis, files);
    let reached = links.closure(seeds, true);
    (links.bodies_reading(&reached), links.bodies)
}

/// How many keys are in a cycle with any of `seeds`, and how many of the
/// first `files` files' bodies read them
pub(super) fn cycle_reach(analysis: &Analysis, seeds: &[Read], files: usize) -> (usize, usize) {
    let links = Links::of(analysis, files);
    let cycle = links.cycles(seeds);
    (cycle.len(), links.bodies_reading(&cycle))
}

/// Prints, over the function bodies of the analysis's own files, how many
/// bodies an edit to each could reach, and how many a reset limited to the
/// cycles it's in would walk again
pub fn print_reach(analysis: &Analysis, paths: &[PathBuf]) {
    if analysis.records.is_empty() {
        println!("no records kept");
        return
    }
    let links = Links::of(analysis, paths.len());
    let total = links.bodies;
    let mut ids: Vec<&FunctionId> = analysis.records.keys().filter(|id| (id.file.0 as usize) < paths.len()).collect();
    ids.sort();
    let mut reaches: Vec<usize> = Vec::new();
    let mut cycles: Vec<usize> = Vec::new();
    for id in ids {
        let seeds: Vec<Read> = analysis.records[id].own_influences.get().iter().map(|(key, _)| key.clone()).collect();
        reaches.push(links.bodies_reading(&links.closure(&seeds, true)));
        cycles.push(links.bodies_reading(&links.cycles(&seeds)));
    }
    let percent = |n: usize| n as f64 * 100.0 / total.max(1) as f64;
    for (label, mut counts) in [("reach", reaches), ("cycles", cycles)] {
        counts.sort();
        let at = |q: f64| counts[((counts.len() - 1) as f64 * q) as usize];
        let small = counts.iter().filter(|&&n| percent(n) <= 5.0).count();
        println!("{label}: median {:.1}%, 90th percentile {:.1}%, most {:.1}% of {total} bodies; {:.0}% of edits at most 5%",
            percent(at(0.5)), percent(at(0.9)), percent(at(1.0)), small as f64 * 100.0 / counts.len().max(1) as f64);
    }
}
