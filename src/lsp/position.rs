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

//! Converting between byte offsets in a document and LSP positions, which
//! count a line's characters in UTF-16 units

use lsp_types::{Position, Range};
use rowan::{TextRange, TextSize};

/// Where `position` is in `text`. A column past the end of its line is the
/// line's end, and a line past the end of the text is the text's end
pub fn offset_of(text: &str, position: Position) -> TextSize {
    let mut start = 0;
    for _ in 0..position.line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return TextSize::from(text.len() as u32),
        }
    }
    let line = &text[start..];
    let line = &line[..line.find(['\r', '\n']).unwrap_or(line.len())];
    let mut units = 0;
    for (i, c) in line.char_indices() {
        if units >= position.character {
            return TextSize::from((start + i) as u32)
        }
        units += c.len_utf16() as u32;
    }
    TextSize::from((start + line.len()) as u32)
}

/// The position of byte `offset` in `text`
pub fn position_of(text: &str, offset: TextSize) -> Position {
    let offset = usize::from(offset).min(text.len());
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position {
        line: before.matches('\n').count() as u32,
        character: before[line_start..].encode_utf16().count() as u32,
    }
}

pub fn range_of(text: &str, range: TextRange) -> Range {
    Range { start: position_of(text, range.start()), end: position_of(text, range.end()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn offsets() {
        // `é` is one UTF-16 unit but two bytes
        assert_eq!(offset_of("é = 1\nab", at(0, 1)), TextSize::from(2));
        assert_eq!(offset_of("x\r\nyz", at(1, 1)), TextSize::from(4));
        assert_eq!(offset_of("x\nyz", at(1, 9)), TextSize::from(4));
        assert_eq!(offset_of("x", at(5, 0)), TextSize::from(1));
        // `😀` is two UTF-16 units and four bytes
        assert_eq!(offset_of("😀a", at(0, 2)), TextSize::from(4));
    }

    #[test]
    fn positions() {
        assert_eq!(position_of("é = 1\nab", TextSize::from(2)), at(0, 1));
        assert_eq!(position_of("x\r\nyz", TextSize::from(4)), at(1, 1));
        assert_eq!(position_of("😀a", TextSize::from(4)), at(0, 2));
        assert_eq!(position_of("", TextSize::from(0)), at(0, 0));
    }

    #[test]
    fn round_trip() {
        let text = "local café = 1\r\nprint(café, \"😀\")\n";
        for (offset, _) in text.char_indices() {
            let offset = TextSize::from(offset as u32);
            // Inside a line ending isn't a position an editor can give
            if text[usize::from(offset)..].starts_with('\n') && text[..usize::from(offset)].ends_with('\r') {
                continue
            }
            assert_eq!(offset_of(text, position_of(text, offset)), offset);
        }
    }
}
