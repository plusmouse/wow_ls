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

//! The fast hash the analysis's maps and sets use

/// The analysis's maps and sets. Its keys are small (positions, names,
/// type ids), and nothing they hold comes from someone trying to make keys
/// collide, so a fast hash beats the standard one, built to resist that. It
/// also makes iteration order the same from run to run
pub type HashMap<K, V> = std::collections::HashMap<K, V, FastHash>;
pub type HashSet<K> = std::collections::HashSet<K, FastHash>;
pub type FastHash = std::hash::BuildHasherDefault<FastHasher>;

/// The hash `rustc` uses for its own maps: each word is mixed in with a
/// rotate, an xor and a multiply
#[derive(Default, Clone, Copy)]
pub struct FastHasher {
    pub(super) hash: u64,
}

impl FastHasher {
    pub(super) fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl std::hash::Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn write_u8(&mut self, i: u8) { self.add(i.into()) }
    fn write_u16(&mut self, i: u16) { self.add(i.into()) }
    fn write_u32(&mut self, i: u32) { self.add(i.into()) }
    fn write_u64(&mut self, i: u64) { self.add(i) }
    fn write_usize(&mut self, i: usize) { self.add(i as u64) }
    fn finish(&self) -> u64 { self.hash }
}
