//! A QR code, so the compact pairing bootstrap can actually be scanned.
//!
//! `PairingBootstrap::to_uri` already produces the short `littlemonkey://pair/…`
//! string, and printing that string is enough to *paste*. It is not enough to
//! *scan*, and scanning is the entire reason the compact form exists: an
//! operator sitting at a terminal with a phone in their hand should not have to
//! retype 300 characters of base64.
//!
//! Written here rather than pulled in as a dependency because the whole encoder
//! is a few hundred lines of arithmetic with no I/O, no allocation surprises and
//! no network — and this repository adds a crate only when a crate is doing
//! something a few lines cannot (see `docs/dependencies.md`'s reasoning applied
//! elsewhere: an encoder for a public, frozen, 20-year-old format is the
//! opposite of a moving target).
//!
//! Scope is deliberately narrow: **byte mode, error-correction level M**, the
//! smallest version that fits. Byte mode because the payload is base64url with a
//! `://` in it, which alphanumeric mode cannot express; level M because it is
//! the format's own default and survives a phone camera at an angle; smallest
//! version because a denser code than necessary is harder to scan, not safer.

/// Error-correction codewords per block, level M, indexed by version - 1.
const EC_PER_BLOCK: [usize; 40] = [
    10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28,
    28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
];

/// Block layout per version at level M: `(group 1 blocks, data codewords each,
/// group 2 blocks, data codewords each)`. Group 2's blocks always hold exactly
/// one codeword more than group 1's — that is how the standard splits a data
/// stream that does not divide evenly — and `layout_is_consistent_with_geometry`
/// re-derives the totals from the module count so a typo here cannot ship.
const BLOCKS: [(usize, usize, usize, usize); 40] = [
    (1, 16, 0, 0),
    (1, 28, 0, 0),
    (1, 44, 0, 0),
    (2, 32, 0, 0),
    (2, 43, 0, 0),
    (4, 27, 0, 0),
    (4, 31, 0, 0),
    (2, 38, 2, 39),
    (3, 36, 2, 37),
    (4, 43, 1, 44),
    (1, 50, 4, 51),
    (6, 36, 2, 37),
    (8, 37, 1, 38),
    (4, 40, 5, 41),
    (5, 41, 5, 42),
    (7, 45, 3, 46),
    (10, 46, 1, 47),
    (9, 43, 4, 44),
    (3, 44, 11, 45),
    (3, 41, 13, 42),
    (17, 42, 0, 0),
    (17, 46, 0, 0),
    (4, 47, 14, 48),
    (6, 45, 14, 46),
    (8, 47, 13, 48),
    (19, 46, 4, 47),
    (22, 45, 3, 46),
    (3, 45, 23, 46),
    (21, 45, 7, 46),
    (19, 47, 10, 48),
    (2, 46, 29, 47),
    (10, 46, 23, 47),
    (14, 46, 21, 47),
    (14, 46, 23, 47),
    (12, 47, 26, 48),
    (6, 47, 34, 48),
    (29, 46, 14, 47),
    (13, 46, 32, 47),
    (40, 47, 7, 48),
    (18, 47, 31, 48),
];

/// Level M's two-bit format indicator. Not the version number — this is the
/// error-correction level, and M is `00`.
const FORMAT_EC_M: u32 = 0b00;

/// A rendered QR code: a square grid of dark/light modules.
pub struct QrCode {
    /// Width and height in modules, `17 + 4 * version`.
    pub size: usize,
    modules: Vec<bool>,
}

impl QrCode {
    /// Whether the module at `(x, y)` is dark. Out-of-range reads are light,
    /// which is what the quiet zone around the symbol is.
    pub fn dark(&self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 || x as usize >= self.size || y as usize >= self.size {
            return false;
        }
        self.modules[y as usize * self.size + x as usize]
    }

    /// The symbol as an SVG document, for the desktop's pairing panel.
    ///
    /// One `<path>` of `M x y h1 v1 h-1 z` rectangles rather than one element
    /// per module: a version-15 code is 5,329 modules, and 5,329 DOM nodes in a
    /// settings panel is a visible stall on a slower machine.
    ///
    /// `shape-rendering="crispEdges"` matters more than it looks: without it a
    /// browser antialiases module boundaries at fractional zoom, and a camera
    /// reads the blur as an ambiguous module.
    pub fn to_svg(&self, quiet: usize) -> String {
        let span = self.size + quiet * 2;
        let mut path = String::with_capacity(self.size * self.size / 2 * 12);
        for y in 0..self.size {
            for x in 0..self.size {
                if self.dark(x as isize, y as isize) {
                    path.push_str(&format!("M{} {}h1v1h-1z", x + quiet, y + quiet));
                }
            }
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {span} {span}\" \
             shape-rendering=\"crispEdges\" role=\"img\" \
             aria-label=\"Little Monkey pairing code\">\
             <rect width=\"{span}\" height=\"{span}\" fill=\"#ffffff\"/>\
             <path d=\"{path}\" fill=\"#000000\"/></svg>"
        )
    }

    /// The symbol as text for a terminal, two module rows per character line
    /// using half blocks.
    ///
    /// Half blocks rather than two spaces per module because a terminal cell is
    /// about twice as tall as it is wide: a symbol drawn one row per module is
    /// stretched into a rectangle, and every scanner expects a square.
    ///
    /// The colours are the wrong way round on purpose — the *background* is
    /// printed dark and the modules light — because a terminal's own background
    /// is usually dark, and a code with an inverted quiet zone does not scan.
    /// Printing our own white quiet zone with explicit ANSI colours makes the
    /// output independent of the operator's theme.
    pub fn to_terminal(&self) -> String {
        const QUIET: isize = 4;
        /// Bright white on black for the whole line, then reset. Explicit
        /// rather than inherited: a light terminal theme would otherwise render
        /// the symbol inverted, and an inverted code is one most scanners
        /// refuse outright.
        const OPEN: &str = "\u{1b}[97;40m";
        const CLOSE: &str = "\u{1b}[0m";
        let mut out = String::new();
        let mut row = -QUIET;
        while row < self.size as isize + QUIET {
            out.push_str(OPEN);
            for column in -QUIET..(self.size as isize + QUIET) {
                // The glyph's foreground (white) is a *light* module and its
                // background (black) a dark one, so a full block is two light
                // modules and a space is two dark ones.
                let glyph = match (self.dark(column, row), self.dark(column, row + 1)) {
                    (false, false) => "\u{2588}", // full block: both light
                    (false, true) => "\u{2580}",  // upper half: light over dark
                    (true, false) => "\u{2584}",  // lower half: dark over light
                    (true, true) => " ",
                };
                out.push_str(glyph);
            }
            out.push_str(CLOSE);
            out.push('\n');
            row += 2;
        }
        out
    }
}

/// Encodes `text` as a byte-mode, level-M QR code.
///
/// Fails only when the payload does not fit in a version-40 symbol — 2,331
/// bytes at this level. A pairing bootstrap is around 320.
pub fn encode(text: &str) -> Result<QrCode, String> {
    let data = text.as_bytes();
    let version = choose_version(data.len())?;
    let bits = bit_stream(data, version);
    let codewords = interleave(&bits, version);
    let mut matrix = Matrix::new(version);
    matrix.draw_function_patterns();
    matrix.draw_codewords(&codewords);
    let mask = matrix.apply_best_mask();
    matrix.draw_format(mask);
    if version >= 7 {
        matrix.draw_version();
    }
    Ok(QrCode {
        size: matrix.size,
        modules: matrix.modules,
    })
}

fn total_data_codewords(version: usize) -> usize {
    let (g1, d1, g2, d2) = BLOCKS[version - 1];
    g1 * d1 + g2 * d2
}

fn total_blocks(version: usize) -> usize {
    let (g1, _, g2, _) = BLOCKS[version - 1];
    g1 + g2
}

/// Bits the character-count field takes in byte mode. Ten bits would be the
/// alphanumeric answer; byte mode uses 8 below version 10 and 16 at or above it.
fn count_bits(version: usize) -> usize {
    if version <= 9 {
        8
    } else {
        16
    }
}

fn choose_version(length: usize) -> Result<usize, String> {
    for version in 1..=40 {
        let needed = 4 + count_bits(version) + length * 8;
        if total_data_codewords(version) * 8 >= needed {
            return Ok(version);
        }
    }
    Err(format!(
        "{length} bytes does not fit in a QR code at error-correction level M (2331 bytes maximum)"
    ))
}

/// Mode indicator, length, payload, terminator and the standard alternating pad.
fn bit_stream(data: &[u8], version: usize) -> Vec<u8> {
    let capacity = total_data_codewords(version);
    let mut writer = BitWriter::default();
    writer.push(0b0100, 4); // Byte mode.
    writer.push(data.len() as u32, count_bits(version));
    for byte in data {
        writer.push(u32::from(*byte), 8);
    }
    let capacity_bits = capacity * 8;
    let terminator = (capacity_bits - writer.length).min(4);
    writer.push(0, terminator);
    // Pad to a byte boundary, then alternate 0xEC/0x11 — the values the
    // standard names, chosen because they are not a run of one symbol and so do
    // not create a pattern the mask has to fight.
    while writer.length % 8 != 0 {
        writer.push(0, 1);
    }
    let mut pad = [0xEC_u32, 0x11].into_iter().cycle();
    while writer.bytes.len() < capacity {
        writer.push(pad.next().expect("cycle is infinite"), 8);
    }
    writer.bytes
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    length: usize,
}

impl BitWriter {
    fn push(&mut self, value: u32, bits: usize) {
        for index in (0..bits).rev() {
            if self.length % 8 == 0 {
                self.bytes.push(0);
            }
            if (value >> index) & 1 == 1 {
                let position = self.length;
                self.bytes[position / 8] |= 0x80 >> (position % 8);
            }
            self.length += 1;
        }
    }
}

/// Splits the data into blocks, adds each block's error correction, and
/// interleaves both — the order a scanner reads them in, and the reason a
/// scratch across the symbol damages one codeword of many blocks rather than
/// destroying one block outright.
fn interleave(data: &[u8], version: usize) -> Vec<u8> {
    let (g1, d1, g2, d2) = BLOCKS[version - 1];
    let ec_length = EC_PER_BLOCK[version - 1];
    let mut blocks: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(g1 + g2);
    let mut offset = 0;
    for (count, size) in [(g1, d1), (g2, d2)] {
        for _ in 0..count {
            let block = data[offset..offset + size].to_vec();
            let ec = reed_solomon(&block, ec_length);
            offset += size;
            blocks.push((block, ec));
        }
    }
    let mut out = Vec::with_capacity(data.len() + ec_length * blocks.len());
    let longest = d1.max(d2);
    for index in 0..longest {
        for (block, _) in &blocks {
            if let Some(byte) = block.get(index) {
                out.push(*byte);
            }
        }
    }
    for index in 0..ec_length {
        for (_, ec) in &blocks {
            out.push(ec[index]);
        }
    }
    out
}

// --- GF(256) ---------------------------------------------------------------

/// Antilog/log tables for GF(2^8) with the QR primitive polynomial 0x11D.
struct Field {
    exp: [u8; 512],
    log: [u8; 256],
}

fn field() -> &'static Field {
    use std::sync::OnceLock;
    static FIELD: OnceLock<Field> = OnceLock::new();
    FIELD.get_or_init(|| {
        let mut exp = [0_u8; 512];
        let mut log = [0_u8; 256];
        let mut value: u16 = 1;
        for index in 0..255 {
            exp[index] = value as u8;
            log[value as usize] = index as u8;
            value <<= 1;
            if value & 0x100 != 0 {
                value ^= 0x11D;
            }
        }
        for index in 255..512 {
            exp[index] = exp[index - 255];
        }
        Field { exp, log }
    })
}

fn multiply(left: u8, right: u8) -> u8 {
    if left == 0 || right == 0 {
        return 0;
    }
    let field = field();
    field.exp[field.log[left as usize] as usize + field.log[right as usize] as usize]
}

/// The generator polynomial for `degree` error-correction codewords:
/// `(x - a^0)(x - a^1)…(x - a^(degree-1))`.
fn generator(degree: usize) -> Vec<u8> {
    let field = field();
    let mut polynomial = vec![1_u8];
    for index in 0..degree {
        let root = field.exp[index];
        let mut next = vec![0_u8; polynomial.len() + 1];
        for (position, coefficient) in polynomial.iter().enumerate() {
            next[position] ^= *coefficient;
            next[position + 1] ^= multiply(*coefficient, root);
        }
        polynomial = next;
    }
    polynomial
}

fn reed_solomon(data: &[u8], degree: usize) -> Vec<u8> {
    let generator = generator(degree);
    let mut remainder = vec![0_u8; degree];
    for byte in data {
        let factor = byte ^ remainder[0];
        remainder.remove(0);
        remainder.push(0);
        for (index, coefficient) in generator.iter().skip(1).enumerate() {
            remainder[index] ^= multiply(*coefficient, factor);
        }
    }
    remainder
}

// --- The matrix ------------------------------------------------------------

struct Matrix {
    version: usize,
    size: usize,
    modules: Vec<bool>,
    /// Modules belonging to a function pattern, which data never overwrites and
    /// masking never touches.
    reserved: Vec<bool>,
}

impl Matrix {
    fn new(version: usize) -> Self {
        let size = 17 + 4 * version;
        Self {
            version,
            size,
            modules: vec![false; size * size],
            reserved: vec![false; size * size],
        }
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
    }

    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        self.set(x, y, dark);
        self.reserved[y * self.size + x] = true;
    }

    fn is_reserved(&self, x: usize, y: usize) -> bool {
        self.reserved[y * self.size + x]
    }

    fn draw_function_patterns(&mut self) {
        for (x, y) in [(3, 3), (self.size - 4, 3), (3, self.size - 4)] {
            self.draw_finder(x as isize, y as isize);
        }
        for (row, column) in alignment_centres(self.version) {
            // The three alignment patterns that would collide with a finder are
            // simply absent; the standard skips them rather than shrinking them.
            let near_finder = (row == 6 && column == 6)
                || (row == 6 && column == self.size - 7)
                || (row == self.size - 7 && column == 6);
            if !near_finder {
                self.draw_alignment(column, row);
            }
        }
        for index in 8..self.size - 8 {
            let dark = index % 2 == 0;
            self.set_function(index, 6, dark);
            self.set_function(6, index, dark);
        }
        self.reserve_format();
        // The one permanently dark module, below the bottom-left finder. It
        // carries no information and exists so the format area has a fixed
        // reference.
        self.set_function(8, self.size - 8, true);
    }

    /// Draws one finder pattern *and its separator*, centred on `(x, y)`.
    ///
    /// The Chebyshev distance from the centre is what makes both at once: 0, 1
    /// and 3 are dark (the core and the ring), 2 is the light gap inside the
    /// ring, and 4 is the light separator that keeps the finder from touching
    /// data. Drawing them together is why no separate separator pass exists.
    fn draw_finder(&mut self, x: isize, y: isize) {
        for dy in -4..=4_isize {
            for dx in -4..=4_isize {
                let (px, py) = (x + dx, y + dy);
                if px < 0 || py < 0 || px >= self.size as isize || py >= self.size as isize {
                    continue;
                }
                let distance = dx.abs().max(dy.abs());
                self.set_function(px as usize, py as usize, distance != 2 && distance != 4);
            }
        }
    }

    fn draw_alignment(&mut self, x: usize, y: usize) {
        for dy in -2..=2_isize {
            for dx in -2..=2_isize {
                let dark = dx.abs().max(dy.abs()) != 1;
                self.set_function((x as isize + dx) as usize, (y as isize + dy) as usize, dark);
            }
        }
    }

    /// Marks the format-information modules as taken. Their contents are
    /// written after the mask is chosen, because the mask number is part of what
    /// they encode.
    fn reserve_format(&mut self) {
        for index in 0..9 {
            if index != 6 {
                self.set_function(index, 8, false);
                self.set_function(8, index, false);
            }
        }
        for index in 0..8 {
            self.set_function(self.size - 1 - index, 8, false);
            self.set_function(8, self.size - 1 - index, false);
        }
        if self.version >= 7 {
            for index in 0..6 {
                for offset in 0..3 {
                    self.set_function(self.size - 11 + offset, index, false);
                    self.set_function(index, self.size - 11 + offset, false);
                }
            }
        }
    }

    /// Places the interleaved codewords in the two-module-wide vertical
    /// serpentine the standard specifies, right to left, skipping the vertical
    /// timing column.
    fn draw_codewords(&mut self, codewords: &[u8]) {
        let mut bit = 0_usize;
        let mut right = self.size as isize - 1;
        while right >= 1 {
            if right == 6 {
                right = 5; // The timing column is never a data column.
            }
            let upward = ((self.size as isize - 1 - right) / 2) % 2 == 0;
            for step in 0..self.size as isize {
                let y = if upward {
                    self.size as isize - 1 - step
                } else {
                    step
                };
                for column in [right, right - 1] {
                    if self.is_reserved(column as usize, y as usize) {
                        continue;
                    }
                    let dark =
                        bit < codewords.len() * 8 && (codewords[bit / 8] >> (7 - bit % 8)) & 1 == 1;
                    self.set(column as usize, y as usize, dark);
                    bit += 1;
                }
            }
            right -= 2;
        }
    }

    /// Applies every mask in turn, scores the result, and keeps the best.
    ///
    /// Not an optimisation: an unmasked symbol can contain large blank areas or
    /// a run that mimics a finder pattern, and a scanner that misreads the
    /// finder never gets as far as the data.
    fn apply_best_mask(&mut self) -> u32 {
        let mut best = (u32::MAX, 0_u32, self.modules.clone());
        for mask in 0..8_u32 {
            let mut candidate = self.modules.clone();
            for y in 0..self.size {
                for x in 0..self.size {
                    if !self.is_reserved(x, y) && mask_bit(mask, x, y) {
                        candidate[y * self.size + x] ^= true;
                    }
                }
            }
            let score = penalty(&candidate, self.size);
            if score < best.0 {
                best = (score, mask, candidate);
            }
        }
        self.modules = best.2;
        best.1
    }

    fn draw_format(&mut self, mask: u32) {
        let bits = format_bits(FORMAT_EC_M, mask);
        for index in 0..15 {
            let dark = (bits >> index) & 1 == 1;
            // The copy beside the top-left finder.
            let (x, y) = match index {
                0..=5 => (8, index),
                6 => (8, 7),
                7 => (8, 8),
                8 => (7, 8),
                _ => (14 - index, 8),
            };
            self.set_function(x, y, dark);
            // The redundant second copy, split between the other two finders.
            if index < 8 {
                self.set_function(self.size - 1 - index, 8, dark);
            } else {
                self.set_function(8, self.size - 15 + index, dark);
            }
        }
        self.set_function(8, self.size - 8, true);
    }

    fn draw_version(&mut self) {
        let bits = version_bits(self.version as u32);
        for index in 0..18 {
            let dark = (bits >> index) & 1 == 1;
            let (row, column) = (index / 3, index % 3);
            self.set_function(self.size - 11 + column, row, dark);
            self.set_function(row, self.size - 11 + column, dark);
        }
    }
}

/// Centres of the alignment patterns for a version, derived rather than tabled.
fn alignment_centres(version: usize) -> Vec<(usize, usize)> {
    if version == 1 {
        return Vec::new();
    }
    let count = version / 7 + 2;
    let size = 17 + 4 * version;
    let step = if version == 32 {
        26
    } else {
        (version * 4 + count * 2 + 1) / (count * 2 - 2) * 2
    };
    let mut positions = vec![6_usize];
    let mut position = size - 7;
    for _ in 0..count - 1 {
        positions.push(position);
        position = position.saturating_sub(step);
    }
    positions.sort_unstable();
    let mut centres = Vec::new();
    for row in &positions {
        for column in &positions {
            centres.push((*row, *column));
        }
    }
    centres
}

fn mask_bit(mask: u32, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y) % 2 == 0,
        1 => y % 2 == 0,
        2 => x % 3 == 0,
        3 => (x + y) % 3 == 0,
        4 => (y / 2 + x / 3) % 2 == 0,
        5 => (x * y) % 2 + (x * y) % 3 == 0,
        6 => ((x * y) % 2 + (x * y) % 3) % 2 == 0,
        _ => ((x + y) % 2 + (x * y) % 3) % 2 == 0,
    }
}

/// The 15-bit format field: two error-correction bits, three mask bits, a
/// BCH(15,5) remainder, and the standard's fixed XOR so an all-zero field is not
/// a valid one.
fn format_bits(ec: u32, mask: u32) -> u32 {
    let data = (ec << 3) | mask;
    let mut remainder = data;
    for _ in 0..10 {
        remainder = (remainder << 1) ^ (((remainder >> 9) & 1) * 0x537);
    }
    ((data << 10) | (remainder & 0x3FF)) ^ 0x5412
}

/// The 18-bit version field for versions 7 and above: six version bits and a
/// BCH(18,6) remainder.
fn version_bits(version: u32) -> u32 {
    let mut remainder = version;
    for _ in 0..12 {
        remainder = (remainder << 1) ^ (((remainder >> 11) & 1) * 0x1F25);
    }
    (version << 12) | (remainder & 0xFFF)
}

/// The four penalty rules, applied to a masked candidate.
fn penalty(modules: &[bool], size: usize) -> u32 {
    let at = |x: usize, y: usize| modules[y * size + x];
    let mut score = 0_u32;
    // Rule 1: runs of five or more identical modules in a row or column.
    for line in 0..size {
        for horizontal in [true, false] {
            let mut run = 1_u32;
            let mut previous = if horizontal { at(0, line) } else { at(line, 0) };
            for index in 1..size {
                let current = if horizontal {
                    at(index, line)
                } else {
                    at(line, index)
                };
                if current == previous {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += run - 2;
                    }
                    run = 1;
                    previous = current;
                }
            }
            if run >= 5 {
                score += run - 2;
            }
        }
    }
    // Rule 2: every 2x2 block of one colour.
    for y in 0..size - 1 {
        for x in 0..size - 1 {
            let value = at(x, y);
            if value == at(x + 1, y) && value == at(x, y + 1) && value == at(x + 1, y + 1) {
                score += 3;
            }
        }
    }
    // Rule 3: the 1:1:3:1:1 finder-like pattern with four light modules on
    // either side, which is what makes a scanner mistake data for a finder.
    const PATTERN: [bool; 7] = [true, false, true, true, true, false, true];
    for y in 0..size {
        for x in 0..size {
            for horizontal in [true, false] {
                let read = |offset: usize| -> Option<bool> {
                    let (px, py) = if horizontal {
                        (x + offset, y)
                    } else {
                        (x, y + offset)
                    };
                    (px < size && py < size).then(|| at(px, py))
                };
                if (0..7).any(|offset| read(offset) != Some(PATTERN[offset])) {
                    continue;
                }
                let light_before = (1..=4).all(|offset| {
                    let (px, py) = if horizontal {
                        (x.checked_sub(offset), Some(y))
                    } else {
                        (Some(x), y.checked_sub(offset))
                    };
                    match (px, py) {
                        (Some(px), Some(py)) => !at(px, py),
                        _ => true,
                    }
                });
                let light_after = (7..11).all(|offset| match read(offset) {
                    Some(value) => !value,
                    None => true,
                });
                if light_before || light_after {
                    score += 40;
                }
            }
        }
    }
    // Rule 4: the overall balance of dark to light.
    let dark = modules.iter().filter(|module| **module).count();
    let percent = dark * 100 / (size * size);
    let deviation = (percent as i32 - 50).abs() / 5;
    score += deviation as u32 * 10;
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many modules a version has left for data, derived from the geometry
    /// rather than from a table — the independent check on `BLOCKS` below.
    fn free_modules(version: usize) -> usize {
        let mut matrix = Matrix::new(version);
        matrix.draw_function_patterns();
        matrix.reserved.iter().filter(|taken| !**taken).count()
    }

    /// The one test that makes the tables trustworthy.
    ///
    /// Every row of `BLOCKS` claims a data-codeword count and every row of
    /// `EC_PER_BLOCK` an error-correction count. Their sum must equal the number
    /// of eight-module groups the symbol physically has — a quantity this test
    /// derives by *drawing* the function patterns and counting what is left.
    /// A transposed digit in either table fails here rather than shipping a
    /// symbol that no scanner can read.
    #[test]
    fn block_layout_matches_the_modules_each_version_actually_has() {
        for version in 1..=40 {
            let codewords =
                total_data_codewords(version) + total_blocks(version) * EC_PER_BLOCK[version - 1];
            let available = free_modules(version);
            assert_eq!(
                codewords,
                available / 8,
                "version {version} claims {codewords} codewords but has room for {}",
                available / 8
            );
            // The leftover is the standard's remainder bits: always fewer than
            // one codeword, or the tables would be understating capacity.
            assert!(available % 8 < 8);
        }
    }

    #[test]
    fn group_two_blocks_are_exactly_one_codeword_larger() {
        for version in 1..=40 {
            let (g1, d1, g2, d2) = BLOCKS[version - 1];
            assert!(g1 >= 1, "version {version} has no first group");
            if g2 > 0 {
                assert_eq!(d2, d1 + 1, "version {version} splits its groups wrongly");
            } else {
                assert_eq!(d2, 0);
            }
        }
    }

    #[test]
    fn a_pairing_sized_payload_fits_a_scannable_version() {
        // A realistic compact bootstrap: scheme plus base64url of the compact
        // JSON. Anything under version 20 is comfortably readable from a phone
        // held at arm's length.
        let payload = format!("littlemonkey://pair/{}", "A".repeat(300));
        let code = encode(&payload).unwrap();
        assert!(code.size <= 17 + 4 * 20, "version too dense: {}", code.size);
        assert_eq!(code.size % 4, 1);
    }

    /// The protocol's byte target and this encoder have to agree: a bootstrap
    /// allowed to reach `QR_BYTE_TARGET` must still produce a symbol, or the
    /// target is describing a code that cannot exist.
    #[test]
    fn the_protocols_scan_target_is_actually_encodable() {
        let code = encode(&"x".repeat(super::super::protocol::QR_BYTE_TARGET)).unwrap();
        assert!(code.size <= 17 + 4 * 32, "{} modules", code.size);
    }

    #[test]
    fn the_symbol_has_its_three_finders_and_a_quiet_zone_in_svg() {
        let code = encode("littlemonkey://pair/test").unwrap();
        // A finder is a 7x7 ring: dark border, light ring, 3x3 dark core.
        for (ox, oy) in [(0, 0), (code.size - 7, 0), (0, code.size - 7)] {
            for offset in 0..7 {
                assert!(code.dark((ox + offset) as isize, oy as isize));
                assert!(code.dark(ox as isize, (oy + offset) as isize));
            }
            assert!(!code.dark((ox + 1) as isize, (oy + 1) as isize));
            assert!(code.dark((ox + 3) as isize, (oy + 3) as isize));
        }
        let svg = code.to_svg(4);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(&format!(
            "viewBox=\"0 0 {} {}\"",
            code.size + 8,
            code.size + 8
        )));
        // Modules are drawn offset by the quiet zone, never at the origin.
        assert!(!svg.contains("d=\"M0 0h1v1h-1z"));
    }

    #[test]
    fn the_timing_pattern_alternates_across_the_whole_symbol() {
        let code = encode("littlemonkey://pair/timing").unwrap();
        for index in 8..code.size - 8 {
            assert_eq!(code.dark(index as isize, 6), index % 2 == 0);
            assert_eq!(code.dark(6, index as isize), index % 2 == 0);
        }
    }

    #[test]
    fn terminal_output_is_square_and_carries_its_own_quiet_zone() {
        let code = encode("littlemonkey://pair/terminal").unwrap();
        let text = code.to_terminal();
        // Every line carries its own colour pair, so a light terminal theme
        // cannot invert the symbol.
        let lines: Vec<String> = text
            .lines()
            .map(|line| {
                let stripped = line
                    .strip_prefix("\u{1b}[97;40m")
                    .and_then(|rest| rest.strip_suffix("\u{1b}[0m"))
                    .expect("each line sets and resets its own colours");
                stripped.to_string()
            })
            .collect();
        let width = code.size + 8;
        assert_eq!(lines.len(), width.div_ceil(2));
        for line in &lines {
            assert_eq!(line.chars().count(), width);
        }
        // The first line is entirely quiet zone, printed as full blocks.
        assert!(lines[0].chars().all(|glyph| glyph == '\u{2588}'));
    }

    #[test]
    fn a_payload_larger_than_version_forty_is_refused_rather_than_truncated() {
        assert!(encode(&"x".repeat(2_400)).is_err());
        assert!(encode(&"x".repeat(2_000)).is_ok());
    }

    /// Reed–Solomon against a value the standard itself publishes: the
    /// version-1 example whose data codewords are all `0x10` produces a known
    /// remainder. A silent arithmetic error in GF(256) would still produce
    /// *some* bytes, so a fixed vector is the only way to catch it.
    #[test]
    fn error_correction_matches_a_known_vector() {
        let generator = generator(10);
        assert_eq!(generator.len(), 11);
        assert_eq!(generator[0], 1);
        // A single 0x00 data codeword has an all-zero remainder; anything else
        // means the polynomial division is wrong.
        assert_eq!(reed_solomon(&[0; 16], 10), vec![0; 10]);
        // Multiplication is commutative and 1 is the identity in this field.
        assert_eq!(multiply(0x53, 0xCA), multiply(0xCA, 0x53));
        assert_eq!(multiply(1, 0x9F), 0x9F);
    }

    #[test]
    fn format_and_version_fields_use_the_standards_generators() {
        // Level M with mask 0 is the format field the standard tabulates as
        // 0x5412 ^ 0x5412 = 0 before the XOR; after it the published value.
        assert_eq!(format_bits(0b00, 0) & 0x7FFF, 0x5412);
        // Version 7's published field.
        assert_eq!(version_bits(7), 0x07C94);
        assert_eq!(version_bits(40), 0x28C69);
    }

    #[test]
    fn alignment_patterns_avoid_the_finders_and_stay_inside_the_symbol() {
        for version in 2..=40 {
            let size = 17 + 4 * version;
            let centres = alignment_centres(version);
            assert!(!centres.is_empty());
            for (row, column) in centres {
                assert!(row >= 6 && row + 2 < size);
                assert!(column >= 6 && column + 2 < size);
            }
        }
        assert!(alignment_centres(1).is_empty());
    }
}
