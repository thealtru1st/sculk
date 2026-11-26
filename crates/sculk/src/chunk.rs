//! Encoder for the body of clientbound `level_chunk_with_light`.
//!
//! The chunk's occupied rows are generated once into a per-thread kind buffer
//! (one byte per block), edits are applied on top, and each section is then
//! scanned for its palette and packed straight into `out`.

use crate::protocol::write_var_i32;
use crate::world::{
    kind_of_state, state_is_air, state_is_fluid, ChunkPlan, World, KIND_STATE, MIN_Y, SECTION_COUNT,
};
use std::cell::RefCell;

/// Typical payload is 20-40 KB; this covers sections plus all 26 light arrays.
pub const LEVEL_CHUNK_SIZE_HINT: usize = 36 * 1024;

const LIGHT_SECTIONS: usize = SECTION_COUNT + 2;
const SECTION_VOLUME: usize = 4096;
static FULL_LIGHT: [u8; 2048] = [0xff; 2048];

/// Kinds at or above this index stand for edit states outside `KIND_STATE`.
const EXTRA_BASE: usize = 64;
/// Marks a voxel whose edit state did not fit in the extra table.
const OVERFLOW: u8 = 255;
const MAX_EXTRAS: usize = OVERFLOW as usize - EXTRA_BASE;
/// Bits per entry of the global block-state palette.
const DIRECT_BITS: u32 = 15;

struct Scratch {
    plan: Box<ChunkPlan>,
    kinds: Vec<u8>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch {
        plan: Box::default(),
        // Grows to the tallest section range encoded so far.
        kinds: Vec::new(),
    });
}

pub fn write_level_chunk(world: &World, chunk_x: i32, chunk_z: i32, out: &mut Vec<u8>) {
    SCRATCH.with(|scratch| {
        let scratch = &mut *scratch.borrow_mut();
        encode(
            world,
            chunk_x,
            chunk_z,
            &mut scratch.plan,
            &mut scratch.kinds,
            out,
        );
    });
}

fn encode(
    world: &World,
    chunk_x: i32,
    chunk_z: i32,
    plan: &mut ChunkPlan,
    kinds: &mut Vec<u8>,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&chunk_x.to_be_bytes());
    out.extend_from_slice(&chunk_z.to_be_bytes());
    write_var_i32(out, 0); // heightmaps

    let terrain = world.terrain();
    terrain.plan_chunk(chunk_x, chunk_z, plan);
    let edits = world.chunk_edits(chunk_x, chunk_z);

    // Sections from the bedrock floor to the highest generated block, widened
    // to cover every edit.
    let mut s_lo = section_of(0);
    let mut s_hi = section_of(plan.top_y);
    if let (Some(first), Some(last)) = (edits.first(), edits.last()) {
        s_lo = s_lo.min((first.0 >> 12) as usize);
        s_hi = s_hi.max((last.0 >> 12) as usize);
    }
    let y_lo = MIN_Y + s_lo as i32 * 16;
    let y_hi = MIN_Y + s_hi as i32 * 16 + 15;
    let needed = (s_hi - s_lo + 1) * SECTION_VOLUME;
    if kinds.len() < needed {
        kinds.resize(needed, 0);
    }
    terrain.fill_chunk(plan, chunk_x, chunk_z, y_lo, y_hi, kinds);

    let mut extras = [0i32; MAX_EXTRAS];
    let mut extra_count = 0;
    let mut overflow = 0u32;
    let base_key = ((y_lo - MIN_Y) as u32) << 8;
    for &(key, state) in edits {
        let k = match kind_of_state(state) {
            Some(k) => k,
            None => match extras[..extra_count].iter().position(|&s| s == state) {
                Some(index) => (EXTRA_BASE + index) as u8,
                None if extra_count < MAX_EXTRAS => {
                    extras[extra_count] = state;
                    extra_count += 1;
                    (EXTRA_BASE + extra_count - 1) as u8
                }
                None => {
                    overflow |= 1 << (key >> 12);
                    OVERFLOW
                }
            },
        };
        kinds[(key - base_key) as usize] = k;
    }

    let mut tables = Tables {
        state: [0; 256],
        counts: [0; 256],
    };
    for (k, &state) in KIND_STATE.iter().enumerate() {
        tables.set(k, state);
    }
    for (index, &state) in extras[..extra_count].iter().enumerate() {
        tables.set(EXTRA_BASE + index, state);
    }

    let mut biomes = [0u8; 64];
    let biomes = biome_container(&plan.cell_biomes, &mut biomes);

    let data_start = out.len();
    let mut highest = None;
    for s in 0..SECTION_COUNT {
        if s < s_lo || s > s_hi {
            out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(biomes);
            continue;
        }
        let at = (s - s_lo) * SECTION_VOLUME;
        let section = &kinds[at..at + SECTION_VOLUME];
        let non_air = if overflow & (1 << s) != 0 {
            let key_lo = (s as u32) << 12;
            let section_edits = edits
                .iter()
                .filter(|&&(key, _)| key >> 12 == s as u32)
                .map(|&(key, state)| ((key - key_lo) as usize, state));
            write_direct_section(section, section_edits, &tables, out)
        } else {
            write_section(section, &tables, out)
        };
        out.extend_from_slice(biomes);
        if non_air > 0 {
            highest = Some(s);
        }
    }

    // Prefix the section data with its length.
    let data_len = out.len() - data_start;
    let mut prefix_buf = [0u8; 5];
    let prefix_len = var_i32_into(data_len as i32, &mut prefix_buf);
    out.extend_from_slice(&prefix_buf[..prefix_len]);
    out.copy_within(data_start..data_start + data_len, data_start + prefix_len);
    out[data_start..data_start + prefix_len].copy_from_slice(&prefix_buf[..prefix_len]);

    write_var_i32(out, 0); // block entities

    // Light: full sky light from below the world to one section above the
    // highest non-air section, no block light.
    let top_light = highest.map_or(1, |s| (s + 2).min(LIGHT_SECTIONS - 1));
    let sky_mask = (1u64 << (top_light + 1)) - 1;
    write_var_i32(out, 1);
    out.extend_from_slice(&sky_mask.to_be_bytes());
    write_var_i32(out, 0);
    write_var_i32(out, 0);
    write_var_i32(out, 1);
    out.extend_from_slice(&((1u64 << LIGHT_SECTIONS) - 1).to_be_bytes());
    write_var_i32(out, top_light as i32 + 1);
    for _ in 0..=top_light {
        write_var_i32(out, FULL_LIGHT.len() as i32);
        out.extend_from_slice(&FULL_LIGHT);
    }
    write_var_i32(out, 0);
}

fn section_of(y: i32) -> usize {
    ((y - MIN_Y) >> 4).clamp(0, SECTION_COUNT as i32 - 1) as usize
}

fn var_i32_into(value: i32, buf: &mut [u8; 5]) -> usize {
    let mut v = value as u32;
    let mut n = 0;
    loop {
        if v < 0x80 {
            buf[n] = v as u8;
            return n + 1;
        }
        buf[n] = (v as u8 & 0x7f) | 0x80;
        v >>= 7;
        n += 1;
    }
}

struct Tables {
    state: [i32; 256],
    /// Non-air flag in the low half, fluid flag in the high half, so one
    /// addition per block keeps both section counts (each at most 4096).
    counts: [u32; 256],
}

impl Tables {
    fn set(&mut self, k: usize, state: i32) {
        self.state[k] = state;
        self.counts[k] = !state_is_air(state) as u32 | ((state_is_fluid(state) as u32) << 16);
    }
}

#[inline(always)]
fn row(section: &[u8], r: usize) -> u128 {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&section[r * 16..r * 16 + 16]);
    u128::from_ne_bytes(bytes)
}

/// Writes counts and the block container of one section; returns the
/// non-air count.
fn write_section(section: &[u8], tables: &Tables, out: &mut Vec<u8>) -> u32 {
    // Presence and counts, skipping rows identical to the previous one.
    let mut seen = [0u8; 256];
    let mut counts = 0u32;
    let mut row_counts = 0u32;
    let mut previous = !row(section, 0);
    for r in 0..256 {
        let current = row(section, r);
        if current != previous {
            previous = current;
            row_counts = 0;
            for &k in &section[r * 16..r * 16 + 16] {
                seen[k as usize] = 1;
                row_counts += tables.counts[k as usize];
            }
        }
        counts += row_counts;
    }
    let (non_air, fluid) = (counts & 0xffff, counts >> 16);
    out.extend_from_slice(&(non_air as i16).to_be_bytes());
    out.extend_from_slice(&(fluid as i16).to_be_bytes());

    let mut remap = [0u8; 256];
    let mut palette = [0u8; 256];
    let mut len = 0;
    for k in 0..256 {
        if seen[k] != 0 {
            remap[k] = len as u8;
            palette[len] = k as u8;
            len += 1;
        }
    }
    if len == 1 {
        out.push(0);
        write_var_i32(out, tables.state[palette[0] as usize]);
        return non_air;
    }
    let bits = (usize::BITS - (len - 1).leading_zeros()).max(4);
    out.push(bits as u8);
    write_var_i32(out, len as i32);
    for &k in &palette[..len] {
        write_var_i32(out, tables.state[k as usize]);
    }
    let per_long = 64 / bits as usize;
    let longs = SECTION_VOLUME.div_ceil(per_long);
    let start = out.len();
    out.resize(start + longs * 8, 0);
    let dst = &mut out[start..];
    if bits == 4 {
        // One 16-block row per long.
        let mut previous = !row(section, 0);
        let mut packed = 0u64;
        for r in 0..256 {
            let current = row(section, r);
            if current != previous {
                previous = current;
                packed = 0;
                for (j, &k) in section[r * 16..r * 16 + 16].iter().enumerate() {
                    packed |= (remap[k as usize] as u64) << (j * 4);
                }
            }
            dst[r * 8..r * 8 + 8].copy_from_slice(&packed.to_be_bytes());
        }
    } else {
        for (l, chunk) in section.chunks(per_long).enumerate() {
            let mut packed = 0u64;
            for (j, &k) in chunk.iter().enumerate() {
                packed |= (remap[k as usize] as u64) << (j * bits as usize);
            }
            dst[l * 8..l * 8 + 8].copy_from_slice(&packed.to_be_bytes());
        }
    }
    non_air
}

/// Fallback for a section holding more distinct edit states than the kind
/// table can name: the global palette, with every edit re-applied exactly.
fn write_direct_section(
    section: &[u8],
    edits: impl Iterator<Item = (usize, i32)>,
    tables: &Tables,
    out: &mut Vec<u8>,
) -> u32 {
    let mut states = [0i32; SECTION_VOLUME];
    for (state, &k) in states.iter_mut().zip(section) {
        *state = tables.state[k as usize];
    }
    for (index, state) in edits {
        states[index] = state;
    }
    let non_air = states.iter().filter(|&&s| !state_is_air(s)).count() as u32;
    let fluid = states.iter().filter(|&&s| state_is_fluid(s)).count();
    out.extend_from_slice(&(non_air as i16).to_be_bytes());
    out.extend_from_slice(&(fluid as i16).to_be_bytes());
    out.push(DIRECT_BITS as u8);
    let per_long = (64 / DIRECT_BITS) as usize;
    for chunk in states.chunks(per_long) {
        let mut packed = 0u64;
        for (j, &state) in chunk.iter().enumerate() {
            packed |= (state as u64) << (j * DIRECT_BITS as usize);
        }
        out.extend_from_slice(&packed.to_be_bytes());
    }
    non_air
}

/// Biome container shared by every section: one biome per 4x4 column cell.
fn biome_container<'a>(cells: &[u8; 16], buf: &'a mut [u8; 64]) -> &'a [u8] {
    let mut palette = [0u8; 8];
    let mut len = 0;
    let mut index = [0u8; 16];
    for (cell, &b) in cells.iter().enumerate() {
        index[cell] = match palette[..len].iter().position(|&p| p == b) {
            Some(i) => i as u8,
            None if len < palette.len() => {
                palette[len] = b;
                len += 1;
                (len - 1) as u8
            }
            None => 0,
        };
    }
    let mut n = 0;
    let mut push = |byte: u8, n: &mut usize| {
        buf[*n] = byte;
        *n += 1;
    };
    if len == 1 {
        push(0, &mut n);
        push(palette[0], &mut n);
        return &buf[..n];
    }
    let bits = (usize::BITS - (len - 1).leading_zeros()) as usize;
    push(bits as u8, &mut n);
    push(len as u8, &mut n);
    for &b in &palette[..len] {
        push(b, &mut n);
    }
    let per_long = 64 / bits;
    let mut entry = 0;
    while entry < 64 {
        let mut packed = 0u64;
        for j in 0..per_long.min(64 - entry) {
            packed |= (index[(entry + j) & 15] as u64) << (j * bits);
        }
        for byte in packed.to_be_bytes() {
            push(byte, &mut n);
        }
        entry += per_long;
    }
    &buf[..n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::read_var_i32_from_slice;
    use crate::vanilla::block;
    use crate::world::kind;

    struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Reader<'_> {
        fn bytes(&mut self, n: usize) -> &[u8] {
            let slice = &self.data[self.pos..self.pos + n];
            self.pos += n;
            slice
        }
        fn u8(&mut self) -> u8 {
            self.bytes(1)[0]
        }
        fn i16(&mut self) -> i16 {
            i16::from_be_bytes(self.bytes(2).try_into().unwrap())
        }
        fn i32(&mut self) -> i32 {
            i32::from_be_bytes(self.bytes(4).try_into().unwrap())
        }
        fn u64(&mut self) -> u64 {
            u64::from_be_bytes(self.bytes(8).try_into().unwrap())
        }
        fn var(&mut self) -> i32 {
            read_var_i32_from_slice(self.data, &mut self.pos).unwrap()
        }
        fn bitset(&mut self) -> u64 {
            let longs = self.var();
            assert!(longs <= 1);
            if longs == 1 {
                self.u64()
            } else {
                0
            }
        }

        /// Paletted container of `entries` values with `max_indirect` bits.
        fn container(&mut self, entries: usize, max_indirect: u8) -> Vec<i32> {
            let bits = self.u8();
            if bits == 0 {
                return vec![self.var(); entries];
            }
            let palette: Option<Vec<i32>> = (bits <= max_indirect).then(|| {
                let len = self.var() as usize;
                assert!(len <= 1 << bits);
                (0..len).map(|_| self.var()).collect()
            });
            let per_long = 64 / bits as usize;
            let longs = entries.div_ceil(per_long);
            let mut values = Vec::with_capacity(entries);
            for _ in 0..longs {
                let long = self.u64();
                for j in 0..per_long {
                    if values.len() == entries {
                        break;
                    }
                    let raw = ((long >> (j * bits as usize)) & ((1 << bits) - 1)) as usize;
                    values.push(match &palette {
                        Some(palette) => palette[raw],
                        None => raw as i32,
                    });
                }
            }
            values
        }
    }

    struct Decoded {
        sections: Vec<Vec<i32>>,
        biomes: Vec<Vec<i32>>,
    }

    fn decode(world: &World, chunk_x: i32, chunk_z: i32) -> Decoded {
        let mut payload = Vec::new();
        write_level_chunk(world, chunk_x, chunk_z, &mut payload);
        let mut r = Reader {
            data: &payload,
            pos: 0,
        };
        assert_eq!(r.i32(), chunk_x);
        assert_eq!(r.i32(), chunk_z);
        assert_eq!(r.var(), 0, "heightmaps");
        let len = r.var() as usize;
        let start = r.pos;
        let mut sections = Vec::new();
        let mut biomes = Vec::new();
        let mut highest = None;
        for s in 0..SECTION_COUNT {
            let non_air = r.i16() as usize;
            let fluid = r.i16() as usize;
            let blocks = r.container(4096, 8);
            biomes.push(r.container(64, 3));
            assert_eq!(
                non_air,
                blocks.iter().filter(|&&b| !state_is_air(b)).count(),
                "section {s}"
            );
            assert_eq!(
                fluid,
                blocks.iter().filter(|&&b| state_is_fluid(b)).count(),
                "section {s}"
            );
            if non_air > 0 {
                highest = Some(s);
            }
            sections.push(blocks);
        }
        assert_eq!(r.pos - start, len, "section data length");
        assert_eq!(r.var(), 0, "block entities");
        let sky = r.bitset();
        assert_eq!(r.bitset(), 0, "block light mask");
        assert_eq!(r.bitset(), 0, "empty sky mask");
        assert_eq!(r.bitset(), (1 << LIGHT_SECTIONS) - 1, "empty block mask");
        let arrays = r.var() as u32;
        assert_eq!(arrays, sky.count_ones());
        assert_eq!(sky, (1 << arrays) - 1, "sky arrays are contiguous from 0");
        let highest = highest.expect("chunk has blocks");
        assert!(
            arrays as usize > highest + 2,
            "sky light reaches above the terrain"
        );
        for _ in 0..arrays {
            assert_eq!(r.var(), 2048);
            assert!(r.bytes(2048).iter().all(|&b| b == 0xff));
        }
        assert_eq!(r.var(), 0, "block light arrays");
        assert_eq!(r.pos, payload.len());
        Decoded { sections, biomes }
    }

    fn assert_matches_world(world: &World, chunk_x: i32, chunk_z: i32) -> Decoded {
        let decoded = decode(world, chunk_x, chunk_z);
        for (s, blocks) in decoded.sections.iter().enumerate() {
            for (i, &state) in blocks.iter().enumerate() {
                let x = chunk_x * 16 + (i & 15) as i32;
                let z = chunk_z * 16 + ((i >> 4) & 15) as i32;
                let y = MIN_Y + s as i32 * 16 + (i >> 8) as i32;
                assert_eq!(
                    state,
                    world.block(x, y, z),
                    "chunk ({chunk_x}, {chunk_z}) block ({x}, {y}, {z})"
                );
            }
        }
        decoded
    }

    #[test]
    fn payload_matches_world_blocks() {
        let world = World::new(0);
        let mut kinds_seen = [false; kind::COUNT];
        let chunks = [
            (0, 0),
            (-1, -1),
            (5, -3),
            (-40, 17),
            (73, 91),
            (-128, -64),
            (200, -150),
            (-310, 260),
        ];
        for (cx, cz) in chunks {
            let decoded = assert_matches_world(&world, cx, cz);
            for blocks in &decoded.sections {
                for &state in blocks {
                    if let Some(k) = kind_of_state(state) {
                        kinds_seen[k as usize] = true;
                    }
                }
            }
        }
        assert!(kinds_seen[kind::BEDROCK as usize]);
        assert!(kinds_seen[kind::STONE as usize]);
        assert!(kinds_seen[kind::WATER as usize] || kinds_seen[kind::GRASS as usize]);
    }

    #[test]
    fn payload_includes_edits() {
        let mut world = World::new(3);
        let (cx, cz) = (2, -5);
        let (x0, z0) = (cx * 16, cz * 16);
        let surface = world.terrain().column_at(x0 + 4, z0 + 4).h;
        let edits = [
            (x0, MIN_Y, z0, block::GLASS),
            (x0 + 15, 300, z0 + 15, block::GOLD_BLOCK),
            (x0 + 4, surface, z0 + 4, block::AIR),
            (x0 + 4, surface + 1, z0 + 4, block::WATER),
            (x0 + 7, 20, z0 + 9, block::STONE),
            (x0 + 8, 20, z0 + 9, block::OAK_PLANKS),
            (x0 + 9, 5, z0 + 1, block::CAVE_AIR),
        ];
        for (x, y, z, state) in edits {
            world.set_block(x, y, z, state);
        }
        assert!(world.chunk_has_edits(cx, cz));
        assert_matches_world(&world, cx, cz);
        assert_matches_world(&world, cx + 1, cz);
    }

    #[test]
    fn many_distinct_edits_use_the_global_palette() {
        let mut world = World::new(9);
        let mut placed = 0;
        for state in 1000..1300 {
            let i = state - 1000;
            if world.set_block(i & 15, 100 + (i >> 8), (i >> 4) & 15, state) {
                placed += 1;
            }
        }
        assert!(placed > 256);
        assert_matches_world(&world, 0, 0);
    }

    #[test]
    fn same_seed_same_bytes() {
        let (a, b, c) = (World::new(11), World::new(11), World::new(12));
        for (cx, cz) in [(0, 0), (-3, 8), (40, -40)] {
            let mut pa = Vec::new();
            let mut pb = Vec::new();
            let mut pc = Vec::new();
            write_level_chunk(&a, cx, cz, &mut pa);
            write_level_chunk(&b, cx, cz, &mut pb);
            write_level_chunk(&c, cx, cz, &mut pc);
            assert_eq!(pa, pb);
            assert_ne!(pa, pc);
        }
    }

    #[test]
    fn trees_agree_across_chunk_borders() {
        let world = World::new(0);
        let terrain = world.terrain();
        let mut plan = Box::<ChunkPlan>::default();
        let mut checked = 0;
        'search: for cz in -24..24 {
            for cx in -24..24 {
                terrain.plan_chunk(cx, cz, &mut plan);
                for tree in &plan.trees[..plan.tree_count] {
                    let (lx, lz) = (tree.x - cx * 16, tree.z - cz * 16);
                    if lx != 15 || !(0..16).contains(&lz) {
                        continue;
                    }
                    // Trunk in this chunk at its east edge; canopy spills east.
                    let left = assert_matches_world(&world, cx, cz);
                    let right = assert_matches_world(&world, cx + 1, cz);
                    let leaves = KIND_STATE[tree.leaves() as usize];
                    let (lo, hi) = tree.leaf_rows();
                    let mut spilled = 0;
                    for y in lo..=hi {
                        let s = ((y - MIN_Y) >> 4) as usize;
                        let i = (((y - MIN_Y) & 15) as usize) << 8 | (lz as usize) << 4;
                        if right.sections[s][i] == leaves {
                            spilled += 1;
                        }
                    }
                    assert!(spilled > 0, "canopy reaches the neighbour chunk");
                    let trunk_y = tree.ground + 1;
                    let s = ((trunk_y - MIN_Y) >> 4) as usize;
                    let i = (((trunk_y - MIN_Y) & 15) as usize) << 8 | (lz as usize) << 4 | 15;
                    assert_eq!(left.sections[s][i], KIND_STATE[tree.log() as usize]);
                    checked += 1;
                    if checked == 3 {
                        break 'search;
                    }
                }
            }
        }
        assert_eq!(checked, 3);
    }

    #[test]
    fn biomes_follow_columns() {
        let world = World::new(0);
        let mut plan = Box::<ChunkPlan>::default();
        world.terrain().plan_chunk(5, -3, &mut plan);
        let decoded = decode(&world, 5, -3);
        for biomes in &decoded.biomes {
            for (i, &b) in biomes.iter().enumerate() {
                assert_eq!(b, plan.cell_biomes[i & 15] as i32);
            }
        }
    }

    /// Every rare branch of the column profile goes through the voxel-exact
    /// comparison at least once.
    #[test]
    fn rare_features_match_world_blocks() {
        const ALL: u32 = (1 << 9) - 1;
        let world = World::new(0);
        let mut plan = Box::<ChunkPlan>::default();
        let mut covered = 0u32;
        'scan: for j in -40..40 {
            for i in -40..40 {
                let (cx, cz) = (i * 7 + j, j * 7 - i);
                world.terrain().plan_chunk(cx, cz, &mut plan);
                let found = plan.cols.iter().fold(0, |m, c| m | c.features());
                if found & !covered != 0 {
                    assert_matches_world(&world, cx, cz);
                    covered |= found;
                    if covered == ALL {
                        break 'scan;
                    }
                }
            }
        }
        assert_eq!(covered, ALL, "missing features {:09b}", ALL & !covered);
    }

    /// Run in debug too (`cargo test -p sculk extreme`) so overflow checks
    /// are active.
    #[test]
    fn extreme_coordinates_are_safe() {
        const EDGE: i32 = 1 << 25;
        let mut world = World::new(5);
        for y in [i32::MIN, i32::MAX, MIN_Y - 1, MIN_Y + 384] {
            assert_eq!(world.block(0, y, 0), block::AIR);
            assert!(!world.set_block(EDGE - 1, y, -EDGE, block::STONE));
        }
        for (x, z) in [
            (-EDGE, -EDGE),
            (EDGE - 1, EDGE - 1),
            (EDGE, -EDGE),
            (-EDGE, EDGE),
        ] {
            for y in [MIN_Y, 0, 62, 100, 319] {
                world.block(x, y, z);
            }
            assert!(world.set_block(x, 300, z, block::GLASS));
        }
        let edge_chunk = 1 << 21;
        for (cx, cz) in [
            (-edge_chunk, -edge_chunk),
            (edge_chunk - 1, edge_chunk - 1),
            (edge_chunk, -edge_chunk),
            (-edge_chunk, edge_chunk),
        ] {
            let decoded = decode(&world, cx, cz);
            for (s, blocks) in decoded.sections.iter().enumerate() {
                for (i, &state) in blocks.iter().enumerate().step_by(61) {
                    let x = cx * 16 + (i & 15) as i32;
                    let z = cz * 16 + ((i >> 4) & 15) as i32;
                    let y = MIN_Y + s as i32 * 16 + (i >> 8) as i32;
                    assert_eq!(state, world.block(x, y, z), "({x}, {y}, {z})");
                }
            }
        }
    }
}
