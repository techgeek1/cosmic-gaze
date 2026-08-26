//! A 5x7 bitmap font, enough to draw a debug label.
//!
//! tiny-skia rasterises paths and nothing else, so there is no text support to lean on and
//! pulling in a shaping stack for a few words of debug output is not worth it. The overlay
//! only ever prints short ASCII labels (an element kind, a score, a coordinate), so a
//! fixed 5x7 cell font covering printable ASCII 32 to 95 is enough. Lowercase is folded to
//! uppercase and anything outside the range renders as `?`.

/// Width of a glyph cell in font pixels.
pub const GLYPH_W: u32 = 5;

/// Height of a glyph cell in font pixels.
pub const GLYPH_H: u32 = 7;

/// Horizontal step from one glyph origin to the next, in font pixels. One column wider
/// than a cell so adjacent glyphs do not touch.
pub const ADVANCE: u32 = 6;

/// First character the table covers. Everything below it is unprintable.
const FIRST: u8 = 32;

/// Character substituted for anything the table does not cover.
const FALLBACK: u8 = b'?';

// --- Api ---

/// Returns the seven row bitmaps for `c`. Bit 4 of each row is the leftmost pixel, bit 0
/// the rightmost, row 0 the top.
pub fn glyph(c: char) -> [u8; 7] {
    let byte = {
        if c.is_ascii() {
            c.to_ascii_uppercase() as u8
        }
        else {
            FALLBACK
        }
    };

    let index = byte.wrapping_sub(FIRST) as usize;

    GLYPHS.get(index).copied().unwrap_or(GLYPHS[(FALLBACK - FIRST) as usize])
}

/// Size of `text` in font pixels, before any scale is applied. The trailing inter-glyph
/// column is not counted, so a one character string is exactly `GLYPH_W` wide.
pub fn text_size(text: &str) -> (u32, u32) {
    let n = text.chars().count() as u32;

    if n == 0 {
        return (0, 0);
    }

    (n * ADVANCE - (ADVANCE - GLYPH_W), GLYPH_H)
}

/// Calls `f(col, row)` for every inked font pixel of `text`, with the origin at the top
/// left of the first glyph cell. Coordinates are in font pixels; the caller scales them.
pub fn for_each_pixel(text: &str, mut f: impl FnMut(u32, u32)) {
    for (i, c) in text.chars().enumerate() {
        let rows = glyph(c);
        let x0   = i as u32 * ADVANCE;

        for (row, bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_W {
                if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                    f(x0 + col, row as u32);
                }
            }
        }
    }
}

// --- Table ---

/// Packs a glyph written as seven five-character rows, `#` for ink, into row bitmaps.
const fn glyph_from(rows: [&str; 7]) -> [u8; 7] {
    let mut out = [0u8; 7];
    let mut r   = 0;

    while r < 7 {
        let bytes = rows[r].as_bytes();
        let mut c = 0;
        let mut bits: u8 = 0;

        while c < 5 {
            if bytes[c] == b'#' {
                bits |= 1 << (4 - c);
            }

            c += 1;
        }

        out[r] = bits;
        r += 1;
    }

    out
}

/// Printable ASCII 32 to 95, in code point order.
static GLYPHS: [[u8; 7]; 64] = [
    glyph_from([".....", ".....", ".....", ".....", ".....", ".....", "....."]), // space
    glyph_from(["..#..", "..#..", "..#..", "..#..", "..#..", ".....", "..#.."]), // !
    glyph_from([".#.#.", ".#.#.", ".....", ".....", ".....", ".....", "....."]), // "
    glyph_from([".#.#.", ".#.#.", "#####", ".#.#.", "#####", ".#.#.", ".#.#."]), // #
    glyph_from(["..#..", ".####", "#.#..", ".###.", "..#.#", "####.", "..#.."]), // $
    glyph_from(["##...", "##..#", "...#.", "..#..", ".#...", "#..##", "...##"]), // %
    glyph_from([".##..", "#..#.", "#.#..", ".#...", "#.#.#", "#..#.", ".##.#"]), // &
    glyph_from(["..#..", "..#..", ".....", ".....", ".....", ".....", "....."]), // '
    glyph_from(["...#.", "..#..", ".#...", ".#...", ".#...", "..#..", "...#."]), // (
    glyph_from([".#...", "..#..", "...#.", "...#.", "...#.", "..#..", ".#..."]), // )
    glyph_from([".....", "#.#.#", ".###.", "#####", ".###.", "#.#.#", "....."]), // *
    glyph_from([".....", "..#..", "..#..", "#####", "..#..", "..#..", "....."]), // +
    glyph_from([".....", ".....", ".....", ".....", "..##.", "..#..", ".#..."]), // ,
    glyph_from([".....", ".....", ".....", "#####", ".....", ".....", "....."]), // -
    glyph_from([".....", ".....", ".....", ".....", ".....", ".##..", ".##.."]), // .
    glyph_from(["....#", "...#.", "..#..", "..#..", ".#...", "#....", "#...."]), // /
    glyph_from([".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###."]), // 0
    glyph_from(["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."]), // 1
    glyph_from([".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"]), // 2
    glyph_from(["#####", "...#.", "..#..", "...#.", "....#", "#...#", ".###."]), // 3
    glyph_from(["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#."]), // 4
    glyph_from(["#####", "#....", "####.", "....#", "....#", "#...#", ".###."]), // 5
    glyph_from(["..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###."]), // 6
    glyph_from(["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#..."]), // 7
    glyph_from([".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."]), // 8
    glyph_from([".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##.."]), // 9
    glyph_from([".....", ".##..", ".##..", ".....", ".##..", ".##..", "....."]), // :
    glyph_from([".....", ".##..", ".##..", ".....", ".##..", "..#..", ".#..."]), // ;
    glyph_from(["...#.", "..#..", ".#...", "#....", ".#...", "..#..", "...#."]), // <
    glyph_from([".....", ".....", "#####", ".....", "#####", ".....", "....."]), // =
    glyph_from([".#...", "..#..", "...#.", "....#", "...#.", "..#..", ".#..."]), // >
    glyph_from([".###.", "#...#", "....#", "...#.", "..#..", ".....", "..#.."]), // ?
    glyph_from([".###.", "#...#", "#.###", "#.#.#", "#.###", "#....", ".####"]), // @
    glyph_from([".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"]), // A
    glyph_from(["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."]), // B
    glyph_from([".###.", "#...#", "#....", "#....", "#....", "#...#", ".###."]), // C
    glyph_from(["###..", "#..#.", "#...#", "#...#", "#...#", "#..#.", "###.."]), // D
    glyph_from(["#####", "#....", "#....", "####.", "#....", "#....", "#####"]), // E
    glyph_from(["#####", "#....", "#....", "####.", "#....", "#....", "#...."]), // F
    glyph_from([".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".####"]), // G
    glyph_from(["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"]), // H
    glyph_from([".###.", "..#..", "..#..", "..#..", "..#..", "..#..", ".###."]), // I
    glyph_from(["....#", "....#", "....#", "....#", "#...#", "#...#", ".###."]), // J
    glyph_from(["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"]), // K
    glyph_from(["#....", "#....", "#....", "#....", "#....", "#....", "#####"]), // L
    glyph_from(["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"]), // M
    glyph_from(["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"]), // N
    glyph_from([".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."]), // O
    glyph_from(["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."]), // P
    glyph_from([".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#"]), // Q
    glyph_from(["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"]), // R
    glyph_from([".####", "#....", "#....", ".###.", "....#", "....#", "####."]), // S
    glyph_from(["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."]), // T
    glyph_from(["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."]), // U
    glyph_from(["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."]), // V
    glyph_from(["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"]), // W
    glyph_from(["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"]), // X
    glyph_from(["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."]), // Y
    glyph_from(["#####", "....#", "...#.", "..#..", ".#...", "#....", "#####"]), // Z
    glyph_from([".###.", ".#...", ".#...", ".#...", ".#...", ".#...", ".###."]), // [
    glyph_from(["#....", "#....", ".#...", "..#..", "..#..", "...#.", "....#"]), // \
    glyph_from([".###.", "...#.", "...#.", "...#.", "...#.", "...#.", ".###."]), // ]
    glyph_from(["..#..", ".#.#.", "#...#", ".....", ".....", ".....", "....."]), // ^
    glyph_from([".....", ".....", ".....", ".....", ".....", ".....", "#####"]), // _
];

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_is_blank_and_underscore_is_a_full_bottom_row() {
        assert_eq!(glyph(' '), [0; 7]);
        assert_eq!(glyph('_')[6], 0b11111);
        assert_eq!(glyph('_')[0], 0);
    }

    #[test]
    fn lowercase_folds_to_uppercase() {
        assert_eq!(glyph('a'), glyph('A'));
    }

    #[test]
    fn unknown_characters_fall_back_to_question_mark() {
        assert_eq!(glyph('\u{263a}'), glyph('?'));
        assert_eq!(glyph('\u{7f}')  , glyph('?'));
    }

    #[test]
    fn text_size_counts_gaps_between_glyphs_but_not_after_the_last() {
        assert_eq!(text_size("")   , (0, 0));
        assert_eq!(text_size("A")  , (GLYPH_W, GLYPH_H));
        assert_eq!(text_size("AB") , (GLYPH_W + ADVANCE, GLYPH_H));
    }

    #[test]
    fn pixel_walk_stays_inside_the_reported_size() {
        let text     = "GAZE 42";
        let (w, h)   = text_size(text);
        let mut seen = 0;

        for_each_pixel(text, |x, y| {
            assert!(x < w, "column {x} past width {w}");
            assert!(y < h, "row {y} past height {h}");
            seen += 1;
        });

        assert!(seen > 0);
    }
}
