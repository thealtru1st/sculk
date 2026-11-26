//! Sculk's terrain model.
//!
//! The world is described column by column. Every noise field is sampled on a
//! lattice of "nodes" spaced four blocks apart in world coordinates and then
//! bilinearly interpolated, so a single column needs exactly four nodes and a
//! whole chunk needs a 7x7 patch of them (the extra ring feeds trees that
//! overhang the chunk edge). On top of the column profile sit three
//! hash-driven layers: ore blobs in 4x4x4 cells, trees on a 5x5 jittered grid
//! and the player's edits.
//!
//! Both the chunk encoder and [`World::block`] go through the same `Terrain`
//! functions, which is what keeps them bit-identical.

use crate::vanilla::{biome, block};
use std::collections::HashMap;

pub const VIEW_DISTANCE: i32 = 2;
pub const MIN_Y: i32 = -64;
pub const SECTION_COUNT: usize = 24;
pub(crate) const MAX_Y: i32 = MIN_Y + SECTION_COUNT as i32 * 16;

/// Highest y filled with water in oceans, lakes and rivers.
pub(crate) const SEA_LEVEL: i32 = 62;
/// Carved cave space at or below this height floods with lava.
const LAVA_LEVEL: i32 = 9;
/// No ore cell starts above this height.
const MAX_ORE_Y: i32 = 111;
/// Peaks above this height lose their soil.
const PEAK_Y: i32 = 112;
/// Trees are placed on a jittered grid of this pitch.
const TREE_CELL: i32 = 5;
/// Largest horizontal distance between a trunk and one of its leaves.
const TREE_REACH: i32 = 2;
/// Lowest y a log or leaf can occupy (trunks need ground at SEA_LEVEL + 1).
const TREE_MIN_Y: i32 = SEA_LEVEL + 2;
pub(crate) const MAX_TREES: usize = 36;

const RIVER_WIDTH: f32 = 0.04;
const RIVER_BED: f32 = 56.5;
const TUNNEL_WIDTH: f32 = 0.075;
const RAVINE_WIDTH: f32 = 0.022;
const RAVINE_MASK: f32 = 0.30;
const COLD: f32 = -0.30;
const COOL: f32 = -0.12;

/// Compact block identifiers used while building chunks. `KIND_STATE` maps
/// them to protocol block states.
pub(crate) mod kind {
    pub const AIR: u8 = 0;
    pub const STONE: u8 = 1;
    pub const DEEPSLATE: u8 = 2;
    pub const BEDROCK: u8 = 3;
    pub const DIRT: u8 = 4;
    pub const GRASS: u8 = 5;
    pub const SNOWY_GRASS: u8 = 6;
    pub const SAND: u8 = 7;
    pub const SANDSTONE: u8 = 8;
    pub const GRAVEL: u8 = 9;
    pub const WATER: u8 = 10;
    pub const ICE: u8 = 11;
    pub const SNOW: u8 = 12;
    pub const LAVA: u8 = 13;
    pub const SNOW_BLOCK: u8 = 14;
    /// Seven stone ores (coal, iron, copper, gold, redstone, lapis, diamond).
    pub const ORE: u8 = 15;
    /// The same seven ores in deepslate.
    pub const DEEP_ORE: u8 = 22;
    /// Oak, birch and spruce logs (29..=31), then their leaves (32..=34).
    pub const OAK_LOG: u8 = 29;
    pub const OAK_LEAVES: u8 = 32;
    pub const SHORT_GRASS: u8 = 35;
    pub const FERN: u8 = 36;
    /// Five flowers (dandelion, poppy, cornflower, azure bluet, oxeye daisy).
    pub const FLOWER: u8 = 37;
    pub const DEAD_BUSH: u8 = 42;
    pub const CACTUS: u8 = 43;
    pub const COUNT: usize = 44;
}

pub(crate) const KIND_STATE: [i32; kind::COUNT] = [
    block::AIR,
    block::STONE,
    block::DEEPSLATE,
    block::BEDROCK,
    block::DIRT,
    block::GRASS_BLOCK,
    block::GRASS_BLOCK_SNOWY,
    block::SAND,
    block::SANDSTONE,
    block::GRAVEL,
    block::WATER,
    block::ICE,
    block::SNOW,
    block::LAVA,
    block::SNOW_BLOCK,
    block::COAL_ORE,
    block::IRON_ORE,
    block::COPPER_ORE,
    block::GOLD_ORE,
    block::REDSTONE_ORE,
    block::LAPIS_ORE,
    block::DIAMOND_ORE,
    block::DEEPSLATE_COAL_ORE,
    block::DEEPSLATE_IRON_ORE,
    block::DEEPSLATE_COPPER_ORE,
    block::DEEPSLATE_GOLD_ORE,
    block::DEEPSLATE_REDSTONE_ORE,
    block::DEEPSLATE_LAPIS_ORE,
    block::DEEPSLATE_DIAMOND_ORE,
    block::OAK_LOG,
    block::BIRCH_LOG,
    block::SPRUCE_LOG,
    block::OAK_LEAVES_PERSISTENT,
    block::BIRCH_LEAVES_PERSISTENT,
    block::SPRUCE_LEAVES_PERSISTENT,
    block::SHORT_GRASS,
    block::FERN,
    block::DANDELION,
    block::POPPY,
    block::CORNFLOWER,
    block::AZURE_BLUET,
    block::OXEYE_DAISY,
    block::DEAD_BUSH,
    block::CACTUS,
];

/// Generator kind for a protocol state, if the generator ever emits it.
pub(crate) fn kind_of_state(state: i32) -> Option<u8> {
    KIND_STATE
        .iter()
        .position(|&s| s == state)
        .map(|index| index as u8)
}

pub(crate) fn state_is_air(state: i32) -> bool {
    state == block::AIR || state == block::CAVE_AIR || state == block::VOID_AIR
}

/// Water and lava, all sixteen levels each.
pub(crate) fn state_is_fluid(state: i32) -> bool {
    (block::WATER..block::WATER + 32).contains(&state)
}

// ---------------------------------------------------------------------------
// Hashing and noise
// ---------------------------------------------------------------------------

const MIX_A: u64 = 0x9e24_2997_68ca_f6bf;
const MIX_B: u64 = 0xd509_463e_f035_1615;
const SPREAD: u64 = 0x94b9_e354_3db9_d05d;
const STEP: u64 = 0x7e37_1ccc_846d_9b21;

/// Bijective 64-bit scrambler: two odd multiplies separated by xor-shifts.
#[inline(always)]
fn scramble(mut v: u64) -> u64 {
    v ^= v >> 31;
    v = v.wrapping_mul(MIX_A);
    v ^= v >> 29;
    v = v.wrapping_mul(MIX_B);
    v ^ (v >> 32)
}

#[inline(always)]
fn hash2(key: u64, x: i32, z: i32) -> u64 {
    let packed = (x as u32 as u64) | ((z as u32 as u64) << 32);
    scramble((packed ^ key).wrapping_mul(SPREAD))
}

#[inline(always)]
fn hash3(key: u64, x: i32, y: i32, z: i32) -> u64 {
    let packed = (x as u32 as u64 & 0x00ff_ffff)
        | ((z as u32 as u64 & 0x00ff_ffff) << 24)
        | ((y as u32 as u64 & 0xffff) << 48);
    scramble((packed ^ key).wrapping_mul(SPREAD))
}

/// Contribution of one lattice corner: a hashed gradient dotted with the offset.
#[inline(always)]
fn corner(key: u64, ix: i32, iz: i32, dx: f32, dz: f32) -> f32 {
    let h = hash2(key, ix, iz);
    let gx = (h as i32) as f32;
    let gz = ((h >> 32) as i32) as f32;
    gx * dx + gz * dz
}

#[inline(always)]
fn ease(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Gradient noise on an integer lattice of pitch `1 << shift` blocks.
/// Output is roughly in [-1, 1].
#[inline(always)]
fn gradient(key: u64, x: i32, z: i32, shift: u32) -> f32 {
    let ix = x >> shift;
    let iz = z >> shift;
    let mask = (1i32 << shift) - 1;
    let scale = 1.0 / (1u32 << shift) as f32;
    let fx = (x & mask) as f32 * scale;
    let fz = (z & mask) as f32 * scale;
    let a = corner(key, ix, iz, fx, fz);
    let b = corner(key, ix + 1, iz, fx - 1.0, fz);
    let c = corner(key, ix, iz + 1, fx, fz - 1.0);
    let d = corner(key, ix + 1, iz + 1, fx - 1.0, fz - 1.0);
    let u = ease(fx);
    let v = ease(fz);
    let near = a + (b - a) * u;
    let far = c + (d - c) * u;
    (near + (far - near) * v) * (NOISE_GAIN / 2_147_483_648.0)
}

/// Normalises raw gradient output to a spread of roughly +-1.
const NOISE_GAIN: f32 = 2.0;

/// Fractal sum of `octaves` gradient layers, the first with pitch `1 << shift`.
#[inline(always)]
fn fractal(key: u64, x: i32, z: i32, shift: u32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut weight = 1.0;
    let mut total = 0.0;
    let mut k = key;
    let mut s = shift;
    for _ in 0..octaves {
        let ox = (k >> 40) as i32 & 0xfff;
        let oz = (k >> 52) as i32;
        sum += gradient(k, x + ox, z + oz, s) * weight;
        total += weight;
        weight *= 0.5;
        s -= 1;
        k = scramble(k ^ STEP);
    }
    sum / total
}

#[inline(always)]
fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

#[inline(always)]
fn bilerp(a: f32, b: f32, c: f32, d: f32, fx: f32, fz: f32) -> f32 {
    let near = a + (b - a) * fx;
    let far = c + (d - c) * fx;
    near + (far - near) * fz
}

// ---------------------------------------------------------------------------
// Terrain model
// ---------------------------------------------------------------------------

struct Keys {
    continent: u64,
    hills: u64,
    mountain: u64,
    river: u64,
    temperature: u64,
    moisture: u64,
    tunnel_a: u64,
    depth_a: u64,
    tunnel_b: u64,
    depth_b: u64,
    ravine: u64,
    ravine_mask: u64,
    column: u64,
    tree: u64,
    ore: u64,
    flower: u64,
}

impl Keys {
    fn new(seed: u64) -> Self {
        let mut n = 0u64;
        let mut next = || {
            n += 1;
            scramble(seed ^ scramble(n.wrapping_mul(STEP)))
        };
        Self {
            continent: next(),
            hills: next(),
            mountain: next(),
            river: next(),
            temperature: next(),
            moisture: next(),
            tunnel_a: next(),
            depth_a: next(),
            tunnel_b: next(),
            depth_b: next(),
            ravine: next(),
            ravine_mask: next(),
            column: next(),
            tree: next(),
            ore: next(),
            flower: next(),
        }
    }
}

/// Noise fields sampled at one lattice node (world x, z multiples of 4).
#[derive(Clone, Copy, Default)]
pub(crate) struct Node {
    height: f32,
    temperature: f32,
    moisture: f32,
    river: f32,
    tunnel_a: f32,
    depth_a: f32,
    tunnel_b: f32,
    depth_b: f32,
    ravine: f32,
    ravine_mask: f32,
}

/// Everything about one column except ores and trees from other columns.
#[derive(Clone, Copy)]
pub(crate) struct Column {
    pub h: i32,
    pub biome: i32,
    top: u8,
    filler: u8,
    under: u8,
    water_top: u8,
    deco: u8,
    tree: u8,
    bedrock_top: i32,
    deep_top: i32,
    under_lo: i32,
    filler_lo: i32,
    deco_top: i32,
    trunk_top: i32,
    shape: u32,
    carves: [(i32, i32); 3],
}

impl Default for Column {
    fn default() -> Self {
        Self {
            h: 0,
            biome: 0,
            top: kind::AIR,
            filler: kind::AIR,
            under: kind::AIR,
            water_top: kind::WATER,
            deco: kind::AIR,
            tree: 0,
            bedrock_top: 0,
            deep_top: 0,
            under_lo: 0,
            filler_lo: 0,
            deco_top: 0,
            trunk_top: 0,
            shape: 0,
            carves: [(1, 0); 3],
        }
    }
}

impl Column {
    #[inline(always)]
    fn carved(&self, y: i32) -> bool {
        let c = &self.carves;
        (y >= c[0].0 && y <= c[0].1) || (y >= c[1].0 && y <= c[1].1) || (y >= c[2].0 && y <= c[2].1)
    }

    /// Kind at height `y` from the column profile alone.
    #[inline]
    pub(crate) fn base_kind(&self, y: i32) -> u8 {
        if y < 0 {
            return kind::AIR;
        }
        if y <= self.bedrock_top {
            return kind::BEDROCK;
        }
        if y <= self.h {
            if self.carved(y) {
                return if y <= LAVA_LEVEL {
                    kind::LAVA
                } else {
                    kind::AIR
                };
            }
            if y == self.h {
                return self.top;
            }
            if y >= self.filler_lo {
                return self.filler;
            }
            if y >= self.under_lo {
                return self.under;
            }
            return if y < self.deep_top {
                kind::DEEPSLATE
            } else {
                kind::STONE
            };
        }
        if self.h < SEA_LEVEL && y <= SEA_LEVEL {
            return if y == SEA_LEVEL {
                self.water_top
            } else {
                kind::WATER
            };
        }
        if y <= self.deco_top {
            return self.deco;
        }
        kind::AIR
    }

    /// Highest non-air y produced by the profile.
    pub(crate) fn top_y(&self) -> i32 {
        self.h
            .max(self.deco_top)
            .max(if self.h < SEA_LEVEL { SEA_LEVEL } else { 0 })
    }

    /// Bitmask of rare profile branches, so tests can target them:
    /// 0 ice, 1 tall cactus, 2 lava carve, 3 carved surface, 4 bare peak,
    /// 5 desert sandstone, 6 tree in a snowy biome, 7 snow layer, 8 river.
    #[cfg(test)]
    pub(crate) fn features(&self) -> u32 {
        let lava = self
            .carves
            .iter()
            .any(|&(lo, hi)| lo <= hi && lo <= LAVA_LEVEL);
        let snowy = matches!(self.biome, biome::SNOWY_PLAINS | biome::SNOWY_TAIGA);
        [
            self.h < SEA_LEVEL && self.water_top == kind::ICE,
            self.deco == kind::CACTUS && self.deco_top > self.h + 1,
            lava,
            self.carved(self.h),
            matches!(self.biome, biome::STONY_PEAKS | biome::SNOWY_SLOPES),
            self.biome == biome::DESERT && self.under == kind::SANDSTONE,
            self.tree != 0 && snowy,
            self.deco == kind::SNOW,
            self.biome == biome::RIVER,
        ]
        .iter()
        .enumerate()
        .fold(0, |mask, (bit, &on)| mask | (on as u32) << bit)
    }
}

const TREE_OAK: u8 = 1;
const TREE_BIRCH: u8 = 2;
const TREE_SPRUCE: u8 = 3;

#[derive(Clone, Copy, Default)]
pub(crate) struct Tree {
    pub x: i32,
    pub z: i32,
    pub ground: i32,
    pub top: i32,
    species: u8,
    shape: u32,
}

impl Tree {
    fn of(col: &Column, x: i32, z: i32) -> Option<Self> {
        (col.tree != 0).then_some(Self {
            x,
            z,
            ground: col.h,
            top: col.trunk_top,
            species: col.tree,
            shape: col.shape,
        })
    }

    pub(crate) fn log(&self) -> u8 {
        kind::OAK_LOG + self.species - 1
    }

    pub(crate) fn leaves(&self) -> u8 {
        kind::OAK_LEAVES + self.species - 1
    }

    pub(crate) fn leaf_rows(&self) -> (i32, i32) {
        if self.species == TREE_SPRUCE {
            (self.ground + 3, self.top + 1)
        } else {
            (self.top - 2, self.top + 1)
        }
    }

    fn is_trunk(&self, x: i32, y: i32, z: i32) -> bool {
        x == self.x && z == self.z && y > self.ground && y <= self.top
    }

    /// Canopy shape relative to the trunk; never includes trunk blocks.
    fn leaf_at(&self, dx: i32, y: i32, dz: i32) -> bool {
        let (ax, az) = (dx.abs(), dz.abs());
        if ax > TREE_REACH || az > TREE_REACH {
            return false;
        }
        if dx == 0 && dz == 0 && y <= self.top {
            return false;
        }
        let corner = (dx > 0) as u32 | (((dz > 0) as u32) << 1);
        if self.species == TREE_SPRUCE {
            if y < self.ground + 3 {
                return false;
            }
            return match self.top + 1 - y {
                0 => ax == 0 && az == 0,
                1 => ax + az <= 1,
                d if d < 0 => false,
                d if d % 2 == 0 => ax <= 1 && az <= 1,
                _ => !(ax == 2 && az == 2),
            };
        }
        match y - self.top {
            -2 | -1 => {
                let layer = (y - self.top + 2) as u32 * 4;
                !(ax == 2 && az == 2) || (self.shape >> (layer + corner)) & 1 == 1
            }
            0 => ax <= 1 && az <= 1 && (ax + az < 2 || (self.shape >> (8 + corner)) & 1 == 1),
            1 => ax + az <= 1,
            _ => false,
        }
    }
}

struct OreCell {
    ore: u8,
    mask: u8,
    x: i32,
    y: i32,
    z: i32,
}

pub(crate) struct Terrain {
    keys: Keys,
}

impl Terrain {
    fn new(seed: u64) -> Self {
        Self {
            keys: Keys::new(seed),
        }
    }

    fn node(&self, gx: i32, gz: i32) -> Node {
        let k = &self.keys;
        let x = gx * 4;
        let z = gz * 4;
        let continent = fractal(k.continent, x, z, 10, 3) + 0.2;
        let hills = fractal(k.hills, x, z, 7, 3);
        let mountain = fractal(k.mountain, x, z, 9, 2);
        let river = fractal(k.river, x, z, 10, 2);

        let mut h = if continent >= 0.0 {
            64.0 + continent * 26.0
        } else {
            63.0 + continent * 48.0
        };
        let land = ((h - 58.0) * 0.1).clamp(0.0, 1.0);
        let m = smooth(((mountain - 0.05) * 2.2).clamp(0.0, 1.0));
        h += (hills * (8.0 + 30.0 * m) + m * m * 34.0) * land;
        let r = river.abs();
        if r < RIVER_WIDTH && h > RIVER_BED {
            let t = smooth(r / RIVER_WIDTH);
            h = RIVER_BED + (h - RIVER_BED) * t;
        }
        Node {
            height: h,
            temperature: fractal(k.temperature, x, z, 10, 2),
            moisture: fractal(k.moisture, x, z, 9, 2),
            river,
            tunnel_a: fractal(k.tunnel_a, x, z, 7, 2),
            depth_a: gradient(k.depth_a, x, z, 8),
            tunnel_b: fractal(k.tunnel_b, x, z, 7, 2),
            depth_b: gradient(k.depth_b, x, z, 8),
            ravine: gradient(k.ravine, x, z, 8),
            ravine_mask: gradient(k.ravine_mask, x, z, 9),
        }
    }

    fn quad(&self, x: i32, z: i32) -> [Node; 4] {
        let gx = x >> 2;
        let gz = z >> 2;
        [
            self.node(gx, gz),
            self.node(gx + 1, gz),
            self.node(gx, gz + 1),
            self.node(gx + 1, gz + 1),
        ]
    }

    /// Column at (x, z) computed from scratch.
    pub(crate) fn column_at(&self, x: i32, z: i32) -> Column {
        self.column(&self.quad(x, z), x, z)
    }

    /// Tree candidate of grid cell (i, j): its origin and hash.
    #[inline]
    fn tree_candidate(&self, i: i32, j: i32) -> (i32, i32, u64) {
        let h = hash2(self.keys.tree, i, j);
        (
            i * TREE_CELL + (h & 3) as i32,
            j * TREE_CELL + ((h >> 2) & 3) as i32,
            h,
        )
    }

    /// Column at (x, z) from its four surrounding nodes, in the order
    /// (gx, gz), (gx + 1, gz), (gx, gz + 1), (gx + 1, gz + 1).
    pub(crate) fn column(&self, n: &[Node; 4], x: i32, z: i32) -> Column {
        let fx = (x & 3) as f32 * 0.25;
        let fz = (z & 3) as f32 * 0.25;
        macro_rules! field {
            ($f:ident) => {
                bilerp(n[0].$f, n[1].$f, n[2].$f, n[3].$f, fx, fz)
            };
        }
        let h = (field!(height).floor() as i32).clamp(16, 250);
        let river = field!(river).abs();
        let temperature = field!(temperature) - (h - 90).max(0) as f32 * 0.012;
        let moisture = field!(moisture);
        let hash = hash2(self.keys.column, x, z);
        let cold = temperature < COLD;

        let mut c = Column {
            h,
            bedrock_top: ((hash >> 40) & 1) as i32 + ((hash >> 40) & (hash >> 41) & 1) as i32,
            deep_top: (7.0 + field!(depth_a) * 3.0) as i32,
            deco_top: h,
            ..Column::default()
        };
        let (mut filler_depth, mut under_depth) = (3, 0);
        let wet = h < SEA_LEVEL;
        if wet {
            if river < RIVER_WIDTH * 1.3 && h >= SEA_LEVEL - 9 {
                c.biome = if cold {
                    biome::FROZEN_RIVER
                } else {
                    biome::RIVER
                };
                c.top = kind::SAND;
                c.filler = kind::SAND;
                filler_depth = 2;
                c.under = kind::DIRT;
                under_depth = 2;
            } else {
                c.biome = if cold {
                    biome::FROZEN_OCEAN
                } else if h < 44 {
                    biome::DEEP_OCEAN
                } else if temperature > 0.4 {
                    biome::WARM_OCEAN
                } else {
                    biome::OCEAN
                };
                c.top = if h >= SEA_LEVEL - 7 {
                    kind::SAND
                } else {
                    kind::GRAVEL
                };
                c.filler = c.top;
            }
            if cold {
                c.water_top = kind::ICE;
            }
        } else if h >= PEAK_Y {
            if cold {
                c.biome = biome::SNOWY_SLOPES;
                c.top = kind::SNOW_BLOCK;
            } else {
                c.biome = biome::STONY_PEAKS;
                c.top = kind::STONE;
            }
            filler_depth = 0;
        } else {
            let land = land_biome(temperature, moisture);
            if h <= SEA_LEVEL + 1 && land != biome::DESERT {
                c.biome = if cold {
                    biome::SNOWY_BEACH
                } else {
                    biome::BEACH
                };
                c.top = kind::SAND;
                c.filler = kind::SAND;
                c.under = kind::SANDSTONE;
                under_depth = 2;
            } else {
                c.biome = land;
                match land {
                    biome::DESERT => {
                        c.top = kind::SAND;
                        c.filler = kind::SAND;
                        filler_depth = 4;
                        c.under = kind::SANDSTONE;
                        under_depth = 4;
                    }
                    biome::SNOWY_PLAINS | biome::SNOWY_TAIGA => {
                        c.top = kind::SNOWY_GRASS;
                        c.filler = kind::DIRT;
                    }
                    _ => {
                        c.top = kind::GRASS;
                        c.filler = kind::DIRT;
                    }
                }
            }
        }
        c.filler_lo = h - filler_depth;
        c.under_lo = c.filler_lo - under_depth;
        c.deep_top = c.deep_top.clamp(c.bedrock_top + 1, c.under_lo);

        // Caves: two families of winding tunnels and occasional ravines, all
        // traced by the zero contours of 2D noise.
        let mut carves = 0;
        let mut carve = |lo: f32, hi: f32| {
            c.carves[carves] = (lo as i32, hi as i32);
            carves += 1;
        };
        let ta = field!(tunnel_a).abs();
        if ta < TUNNEL_WIDTH {
            let s = 1.0 - ta / TUNNEL_WIDTH;
            let centre = 22.0 + field!(depth_a) * 16.0;
            let r = 1.0 + 3.0 * s;
            carve(centre - r * 0.6, centre + r);
        }
        let tb = field!(tunnel_b).abs();
        if tb < TUNNEL_WIDTH {
            let s = 1.0 - tb / TUNNEL_WIDTH;
            let centre = 48.0 + field!(depth_b) * 22.0;
            let r = 1.0 + 2.6 * s;
            carve(centre - r * 0.6, centre + r);
        }
        let rv = field!(ravine).abs();
        if rv < RAVINE_WIDTH && field!(ravine_mask) > RAVINE_MASK {
            let s = 1.0 - rv / RAVINE_WIDTH;
            carve(8.0 + (1.0 - s) * 46.0, (h + 1) as f32);
        }
        let exposed = h >= SEA_LEVEL + 4 && river > RIVER_WIDTH * 2.5;
        let ceiling = if exposed { h } else { h.min(SEA_LEVEL) - 6 };
        for (lo, hi) in c.carves.iter_mut().take(carves) {
            *lo = (*lo).max(3);
            *hi = (*hi).min(ceiling);
        }
        let intact = !c.carved(h);

        // Trees and cacti, decided by this column's own grid cell.
        let (ox, oz, th) = self.tree_candidate(x.div_euclid(TREE_CELL), z.div_euclid(TREE_CELL));
        if ox == x && oz == z && intact && !wet && h > SEA_LEVEL && h + 14 < MAX_Y {
            let roll = ((th >> 8) & 1023) as u32;
            let pick = (th >> 18) & 3;
            let grow = ((th >> 20) % 3) as i32;
            let (species, density) = match c.biome {
                biome::FOREST => (if pick == 0 { TREE_BIRCH } else { TREE_OAK }, 760),
                biome::BIRCH_FOREST => (TREE_BIRCH, 820),
                biome::TAIGA => (TREE_SPRUCE, 700),
                biome::SNOWY_TAIGA => (TREE_SPRUCE, 640),
                biome::PLAINS => (TREE_OAK, 22),
                biome::SNOWY_PLAINS => (TREE_SPRUCE, 18),
                biome::DESERT => (0, 70),
                _ => (0, 0),
            };
            if roll < density {
                if species == 0 {
                    c.deco = kind::CACTUS;
                    c.deco_top = h + 1 + grow;
                    return c;
                }
                c.tree = species;
                c.trunk_top = h
                    + grow
                    + match species {
                        TREE_OAK => 4,
                        TREE_BIRCH => 5,
                        _ => 6 + ((th >> 24) & 1) as i32,
                    };
                c.shape = (th >> 32) as u32;
                if c.top == kind::SNOWY_GRASS {
                    c.top = kind::GRASS;
                }
                return c;
            }
        }
        if !intact {
            return c;
        }

        // Ground cover.
        let roll = ((hash >> 8) & 1023) as u32;
        let flower = || {
            let patch = hash2(self.keys.flower, x >> 3, z >> 3);
            kind::FLOWER + (patch % 5) as u8
        };
        let meadow = hash2(self.keys.flower, x >> 4, z >> 4) & 3 == 0;
        c.deco = match c.biome {
            biome::PLAINS if roll < if meadow { 110 } else { 8 } => flower(),
            biome::PLAINS if roll < 320 => kind::SHORT_GRASS,
            biome::FOREST if roll < 10 => flower(),
            biome::FOREST if roll < 170 => kind::SHORT_GRASS,
            biome::BIRCH_FOREST if roll < 16 => flower(),
            biome::BIRCH_FOREST if roll < 200 => kind::SHORT_GRASS,
            biome::TAIGA if roll < 110 => kind::FERN,
            biome::TAIGA if roll < 180 => kind::SHORT_GRASS,
            biome::SNOWY_PLAINS | biome::SNOWY_TAIGA | biome::SNOWY_BEACH => kind::SNOW,
            biome::DESERT if roll < 10 => kind::DEAD_BUSH,
            _ => kind::AIR,
        };
        if c.deco != kind::AIR {
            c.deco_top = h + 1;
        }
        c
    }

    #[inline]
    fn ore_cell(&self, cx: i32, cy: i32, cz: i32) -> Option<OreCell> {
        let y = cy * 4;
        if !(0..=MAX_ORE_Y).contains(&y) {
            return None;
        }
        let h = hash3(self.keys.ore, cx, cy, cz);
        let ore = pick_ore(y + 2, (h & 1023) as u32)?;
        let mut mask = ((h >> 32) as u8) | (1 << ((h >> 40) & 7));
        if ore == ORE_DIAMOND {
            mask &= (h >> 48) as u8 | (1 << ((h >> 56) & 7));
        }
        Some(OreCell {
            ore,
            mask,
            x: cx * 4 + ((h >> 10) & 0xff) as i32 % 3,
            y: y + ((h >> 18) & 0xff) as i32 % 3,
            z: cz * 4 + ((h >> 26) & 0x3f) as i32 % 3,
        })
    }

    fn ore_at(&self, x: i32, y: i32, z: i32) -> Option<u8> {
        let cell = self.ore_cell(x >> 2, y >> 2, z >> 2)?;
        let (dx, dy, dz) = (x - cell.x, y - cell.y, z - cell.z);
        if !(0..2).contains(&dx) || !(0..2).contains(&dy) || !(0..2).contains(&dz) {
            return None;
        }
        let bit = dx | (dy << 1) | (dz << 2);
        ((cell.mask >> bit) & 1 == 1).then_some(cell.ore)
    }

    /// Full generated kind at one position.
    fn kind_at(&self, x: i32, y: i32, z: i32) -> u8 {
        let col = self.column_at(x, z);
        let base = col.base_kind(y);
        match base {
            kind::STONE | kind::DEEPSLATE => match self.ore_at(x, y, z) {
                Some(ore) if base == kind::STONE => kind::ORE + ore,
                Some(ore) => kind::DEEP_ORE + ore,
                None => base,
            },
            kind::AIR if y >= TREE_MIN_Y => self.tree_kind_at(&col, x, y, z),
            _ => base,
        }
    }

    /// Log or leaves at an air position, or air. Logs win over leaves; among
    /// overlapping canopies the first tree in (cell z, cell x) order wins.
    fn tree_kind_at(&self, own: &Column, x: i32, y: i32, z: i32) -> u8 {
        if let Some(tree) = Tree::of(own, x, z) {
            if tree.is_trunk(x, y, z) {
                return tree.log();
            }
        }
        let (i0, i1) = (
            (x - TREE_REACH).div_euclid(TREE_CELL),
            (x + TREE_REACH).div_euclid(TREE_CELL),
        );
        let (j0, j1) = (
            (z - TREE_REACH).div_euclid(TREE_CELL),
            (z + TREE_REACH).div_euclid(TREE_CELL),
        );
        for j in j0..=j1 {
            for i in i0..=i1 {
                let (ox, oz, _) = self.tree_candidate(i, j);
                if (ox - x).abs() > TREE_REACH || (oz - z).abs() > TREE_REACH {
                    continue;
                }
                let col = if ox == x && oz == z {
                    *own
                } else {
                    self.column_at(ox, oz)
                };
                if let Some(tree) = Tree::of(&col, ox, oz) {
                    let (lo, hi) = tree.leaf_rows();
                    if y >= lo && y <= hi && tree.leaf_at(x - ox, y, z - oz) {
                        return tree.leaves();
                    }
                }
            }
        }
        kind::AIR
    }

    /// Computes the columns, trees and biome cells of a chunk.
    pub(crate) fn plan_chunk(&self, cx: i32, cz: i32, plan: &mut ChunkPlan) {
        // A 7x7 node patch starting one node before the chunk. The inner 5x5
        // serves the chunk's own columns; the outer ring is only computed when
        // a tree candidate just outside the chunk needs it.
        let gx0 = cx * 4 - 1;
        let gz0 = cz * 4 - 1;
        let mut nodes = [Node::default(); 49];
        let mut ready = 0u64;
        for j in 1..6 {
            for i in 1..6 {
                nodes[j * 7 + i] = self.node(gx0 + i as i32, gz0 + j as i32);
                ready |= 1 << (j * 7 + i);
            }
        }
        let mut quad = |x: i32, z: i32| {
            let i = ((x >> 2) - gx0) as usize;
            let j = ((z >> 2) - gz0) as usize;
            let at = j * 7 + i;
            for at in [at, at + 1, at + 7, at + 8] {
                if ready & (1 << at) == 0 {
                    nodes[at] = self.node(gx0 + (at % 7) as i32, gz0 + (at / 7) as i32);
                    ready |= 1 << at;
                }
            }
            [nodes[at], nodes[at + 1], nodes[at + 7], nodes[at + 8]]
        };
        let (x0, z0) = (cx * 16, cz * 16);
        let mut top_y = 0;
        for lz in 0..16 {
            for lx in 0..16 {
                let (x, z) = (x0 + lx as i32, z0 + lz as i32);
                let col = self.column(&quad(x, z), x, z);
                top_y = top_y.max(col.top_y());
                plan.cols[lz * 16 + lx] = col;
            }
        }
        for j in 0..4 {
            for i in 0..4 {
                plan.cell_biomes[j * 4 + i] = plan.cols[(j * 4 + 2) * 16 + i * 4 + 2].biome as u8;
            }
        }
        plan.tree_count = 0;
        let (lo_x, hi_x) = (x0 - TREE_REACH, x0 + 15 + TREE_REACH);
        let (lo_z, hi_z) = (z0 - TREE_REACH, z0 + 15 + TREE_REACH);
        for j in lo_z.div_euclid(TREE_CELL)..=hi_z.div_euclid(TREE_CELL) {
            for i in lo_x.div_euclid(TREE_CELL)..=hi_x.div_euclid(TREE_CELL) {
                let (ox, oz, _) = self.tree_candidate(i, j);
                if ox < lo_x || ox > hi_x || oz < lo_z || oz > hi_z {
                    continue;
                }
                let (lx, lz) = (ox - x0, oz - z0);
                let col = if (0..16).contains(&lx) && (0..16).contains(&lz) {
                    plan.cols[(lz * 16 + lx) as usize]
                } else {
                    self.column(&quad(ox, oz), ox, oz)
                };
                if let Some(tree) = Tree::of(&col, ox, oz) {
                    top_y = top_y.max(tree.top + 1);
                    plan.trees[plan.tree_count] = tree;
                    plan.tree_count += 1;
                }
            }
        }
        plan.top_y = top_y;
    }

    /// Fills `buf` with kinds for rows `y_lo..=y_hi` of a planned chunk,
    /// indexed `((y - y_lo) << 8) | (z << 4) | x`.
    pub(crate) fn fill_chunk(
        &self,
        plan: &ChunkPlan,
        cx: i32,
        cz: i32,
        y_lo: i32,
        y_hi: i32,
        buf: &mut [u8],
    ) {
        let rows = (y_hi - y_lo + 1) as usize;
        let buf = &mut buf[..rows * 256];
        buf.fill(kind::AIR);

        // Rows that are deepslate or stone in every column are written whole.
        let (mut bedrock, mut deep_lo, mut deep_hi, mut under) = (0, i32::MAX, 0, i32::MAX);
        for col in &plan.cols {
            bedrock = bedrock.max(col.bedrock_top);
            deep_lo = deep_lo.min(col.deep_top);
            deep_hi = deep_hi.max(col.deep_top);
            under = under.min(col.under_lo);
        }
        let deep_rows = (bedrock + 1, deep_lo - 1);
        let stone_rows = (deep_hi, under - 1);
        for (rows, k) in [(deep_rows, kind::DEEPSLATE), (stone_rows, kind::STONE)] {
            for y in rows.0.max(y_lo)..=rows.1.min(y_hi) {
                let at = ((y - y_lo) as usize) << 8;
                buf[at..at + 256].fill(k);
            }
        }

        let mut run = |at: usize, lo: i32, hi: i32, k: u8| {
            let mut y = lo.max(y_lo);
            let hi = hi.min(y_hi);
            while y <= hi {
                buf[(((y - y_lo) as usize) << 8) | at] = k;
                y += 1;
            }
        };
        for (at, col) in plan.cols.iter().enumerate() {
            run(at, 0, col.bedrock_top, kind::BEDROCK);
            let deep = (col.bedrock_top + 1, col.deep_top - 1);
            run(at, deep.0, deep.1.min(deep_rows.0 - 1), kind::DEEPSLATE);
            run(at, deep.0.max(deep_rows.1 + 1), deep.1, kind::DEEPSLATE);
            let stone = (col.deep_top, col.under_lo - 1);
            run(at, stone.0, stone.1.min(stone_rows.0 - 1), kind::STONE);
            run(at, stone.0.max(stone_rows.1 + 1), stone.1, kind::STONE);
            run(at, col.under_lo, col.filler_lo - 1, col.under);
            run(at, col.filler_lo, col.h - 1, col.filler);
            run(at, col.h, col.h, col.top);
            if col.h < SEA_LEVEL {
                run(at, col.h + 1, SEA_LEVEL - 1, kind::WATER);
                run(at, SEA_LEVEL, SEA_LEVEL, col.water_top);
            }
            run(at, col.h + 1, col.deco_top, col.deco);
            for &(lo, hi) in &col.carves {
                run(at, lo, hi.min(LAVA_LEVEL), kind::LAVA);
                run(at, lo.max(LAVA_LEVEL + 1), hi, kind::AIR);
            }
        }

        // Ore blobs replace stone and deepslate.
        for cy in (y_lo >> 2)..=(y_hi >> 2) {
            for cell_z in 0..4 {
                for cell_x in 0..4 {
                    let Some(cell) = self.ore_cell(cx * 4 + cell_x, cy, cz * 4 + cell_z) else {
                        continue;
                    };
                    for bit in 0..8 {
                        if (cell.mask >> bit) & 1 == 0 {
                            continue;
                        }
                        let x = (cell.x + (bit & 1)) & 15;
                        let y = cell.y + ((bit >> 1) & 1);
                        let z = (cell.z + ((bit >> 2) & 1)) & 15;
                        let at = (((y - y_lo) as usize) << 8) | ((z as usize) << 4) | x as usize;
                        match buf[at] {
                            kind::STONE => buf[at] = kind::ORE + cell.ore,
                            kind::DEEPSLATE => buf[at] = kind::DEEP_ORE + cell.ore,
                            _ => {}
                        }
                    }
                }
            }
        }

        // Canopies fill air only, first tree first; trunks then win over leaves.
        let (x0, z0) = (cx * 16, cz * 16);
        let trees = &plan.trees[..plan.tree_count];
        for tree in trees {
            let (lo, hi) = tree.leaf_rows();
            let leaves = tree.leaves();
            for y in lo.max(y_lo)..=hi.min(y_hi) {
                for dz in -TREE_REACH..=TREE_REACH {
                    let lz = tree.z + dz - z0;
                    if !(0..16).contains(&lz) {
                        continue;
                    }
                    for dx in -TREE_REACH..=TREE_REACH {
                        let lx = tree.x + dx - x0;
                        if !(0..16).contains(&lx) || !tree.leaf_at(dx, y, dz) {
                            continue;
                        }
                        let at = (((y - y_lo) as usize) << 8) | ((lz as usize) << 4) | lx as usize;
                        if buf[at] == kind::AIR {
                            buf[at] = leaves;
                        }
                    }
                }
            }
        }
        for tree in trees {
            let (lx, lz) = (tree.x - x0, tree.z - z0);
            if !(0..16).contains(&lx) || !(0..16).contains(&lz) {
                continue;
            }
            let at = ((lz as usize) << 4) | lx as usize;
            for y in (tree.ground + 1).max(y_lo)..=tree.top.min(y_hi) {
                buf[(((y - y_lo) as usize) << 8) | at] = tree.log();
            }
        }
    }
}

fn land_biome(temperature: f32, moisture: f32) -> i32 {
    if temperature < COLD {
        if moisture > 0.0 {
            biome::SNOWY_TAIGA
        } else {
            biome::SNOWY_PLAINS
        }
    } else if temperature < COOL {
        biome::TAIGA
    } else if temperature > 0.26 && moisture < 0.05 {
        biome::DESERT
    } else if moisture > 0.32 {
        biome::BIRCH_FOREST
    } else if moisture > -0.02 {
        biome::FOREST
    } else {
        biome::PLAINS
    }
}

const ORE_COAL: u8 = 0;
const ORE_IRON: u8 = 1;
const ORE_COPPER: u8 = 2;
const ORE_GOLD: u8 = 3;
const ORE_REDSTONE: u8 = 4;
const ORE_LAPIS: u8 = 5;
const ORE_DIAMOND: u8 = 6;

/// Ore for a cell centred at height `y`, from a roll in 0..1024.
fn pick_ore(y: i32, roll: u32) -> Option<u8> {
    let table: &[(u32, u8)] = if y < 16 {
        &[
            (14, ORE_DIAMOND),
            (52, ORE_REDSTONE),
            (66, ORE_LAPIS),
            (82, ORE_GOLD),
            (108, ORE_IRON),
        ]
    } else if y < 32 {
        &[
            (14, ORE_REDSTONE),
            (28, ORE_LAPIS),
            (46, ORE_GOLD),
            (82, ORE_IRON),
            (96, ORE_COPPER),
            (108, ORE_COAL),
        ]
    } else if y < 64 {
        &[(34, ORE_IRON), (66, ORE_COPPER), (108, ORE_COAL)]
    } else {
        &[(54, ORE_COAL), (70, ORE_COPPER), (78, ORE_IRON)]
    };
    table
        .iter()
        .find(|&&(limit, _)| roll < limit)
        .map(|&(_, ore)| ore)
}

/// Per-chunk working set shared by the chunk encoder.
pub(crate) struct ChunkPlan {
    pub cols: [Column; 256],
    pub trees: [Tree; MAX_TREES],
    pub tree_count: usize,
    pub cell_biomes: [u8; 16],
    /// Highest generated non-air y (edits not included).
    pub top_y: i32,
}

impl Default for ChunkPlan {
    fn default() -> Self {
        Self {
            cols: [Column::default(); 256],
            trees: [Tree::default(); MAX_TREES],
            tree_count: 0,
            cell_biomes: [0; 16],
            top_y: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

/// Local key of a block inside its chunk: `((y - MIN_Y) << 8) | (z << 4) | x`.
#[inline]
fn local_key(x: i32, y: i32, z: i32) -> u32 {
    (((y - MIN_Y) as u32) << 8) | (((z & 15) as u32) << 4) | (x & 15) as u32
}

pub struct World {
    seed: u64,
    terrain: Terrain,
    /// Per chunk, edits sorted by local key.
    edits: HashMap<(i32, i32), Vec<(u32, i32)>>,
    spawn: (f64, f64, f64),
}

impl World {
    pub fn new(seed: u64) -> Self {
        let terrain = Terrain::new(seed);
        let spawn = find_spawn(&terrain);
        Self {
            seed,
            terrain,
            edits: HashMap::new(),
            spawn,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn spawn_point(&self) -> (f64, f64, f64) {
        self.spawn
    }

    pub fn block(&self, x: i32, y: i32, z: i32) -> i32 {
        if !(MIN_Y..MAX_Y).contains(&y) {
            return block::AIR;
        }
        self.edit_at(x, y, z)
            .unwrap_or_else(|| self.generated(x, y, z))
    }

    pub fn set_block(&mut self, x: i32, y: i32, z: i32, block: i32) -> bool {
        if !(MIN_Y..MAX_Y).contains(&y) {
            return false;
        }
        let generated = self.generated(x, y, z);
        let key = local_key(x, y, z);
        let chunk = (x >> 4, z >> 4);
        let edits = self.edits.entry(chunk).or_default();
        let found = edits.binary_search_by_key(&key, |&(k, _)| k);
        let current = match found {
            Ok(index) => edits[index].1,
            Err(_) => generated,
        };
        let changed = current != block;
        match found {
            Ok(index) if block == generated => {
                edits.remove(index);
            }
            Ok(index) => edits[index].1 = block,
            Err(index) if block != generated => edits.insert(index, (key, block)),
            Err(_) => {}
        }
        if edits.is_empty() {
            self.edits.remove(&chunk);
        }
        changed
    }

    pub fn edits(&self) -> impl Iterator<Item = (i32, i32, i32, i32)> + '_ {
        self.edits.iter().flat_map(|(&(cx, cz), edits)| {
            edits.iter().map(move |&(key, state)| {
                (
                    cx * 16 + (key & 15) as i32,
                    (key >> 8) as i32 + MIN_Y,
                    cz * 16 + ((key >> 4) & 15) as i32,
                    state,
                )
            })
        })
    }

    pub fn chunk_has_edits(&self, chunk_x: i32, chunk_z: i32) -> bool {
        self.edits.contains_key(&(chunk_x, chunk_z))
    }

    pub(crate) fn terrain(&self) -> &Terrain {
        &self.terrain
    }

    /// Edits of one chunk, sorted by local key.
    pub(crate) fn chunk_edits(&self, chunk_x: i32, chunk_z: i32) -> &[(u32, i32)] {
        self.edits
            .get(&(chunk_x, chunk_z))
            .map_or(&[], |edits| edits.as_slice())
    }

    fn edit_at(&self, x: i32, y: i32, z: i32) -> Option<i32> {
        let edits = self.edits.get(&(x >> 4, z >> 4))?;
        let key = local_key(x, y, z);
        edits
            .binary_search_by_key(&key, |&(k, _)| k)
            .ok()
            .map(|index| edits[index].1)
    }

    fn generated(&self, x: i32, y: i32, z: i32) -> i32 {
        KIND_STATE[self.terrain.kind_at(x, y, z) as usize]
    }
}

/// Spawn column nearest the origin: dry solid ground with two blocks of air
/// above, preferring grass within 256 blocks, then any ground within 4096.
fn find_spawn(terrain: &Terrain) -> (f64, f64, f64) {
    let check = |x: i32, z: i32, grass_only: bool| {
        let col = terrain.column_at(x, z);
        let y = col.h;
        if y < SEA_LEVEL || (grass_only && col.top != kind::GRASS) {
            return None;
        }
        let ground = terrain.kind_at(x, y, z);
        let solid = matches!(
            ground,
            kind::GRASS | kind::SNOWY_GRASS | kind::SAND | kind::DIRT | kind::STONE | kind::GRAVEL
        );
        (solid
            && terrain.kind_at(x, y + 1, z) == kind::AIR
            && terrain.kind_at(x, y + 2, z) == kind::AIR)
            .then_some((x as f64 + 0.5, (y + 1) as f64, z as f64 + 0.5))
    };
    for (grass_only, limit) in [(true, 256), (false, 4096)] {
        if let Some(found) = check(0, 0, grass_only) {
            return found;
        }
        let mut radius = 1;
        while radius <= limit {
            let step = if radius <= 64 { 1 } else { 4 };
            let mut t = -radius;
            while t < radius {
                for (x, z) in [(t, -radius), (radius, t), (-t, radius), (-radius, -t)] {
                    if let Some(found) = check(x, z, grass_only) {
                        return found;
                    }
                }
                t += step;
            }
            radius += step;
        }
    }
    let col = terrain.column_at(0, 0);
    (0.5, (col.top_y() + 1) as f64, 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_stands_on_dry_solid_ground() {
        for seed in [0, 1, 42, 0xdead_beef] {
            let world = World::new(seed);
            let (x, y, z) = world.spawn_point();
            assert_eq!(x - x.floor(), 0.5);
            assert_eq!(z - z.floor(), 0.5);
            let (bx, by, bz) = (x.floor() as i32, y as i32, z.floor() as i32);
            let ground = world.block(bx, by - 1, bz);
            assert!(
                !state_is_air(ground) && !state_is_fluid(ground),
                "seed {seed}"
            );
            assert_eq!(world.block(bx, by, bz), block::AIR, "seed {seed}");
            assert_eq!(world.block(bx, by + 1, bz), block::AIR, "seed {seed}");
            assert!(by > SEA_LEVEL);
        }
    }

    #[test]
    fn set_block_round_trip_removes_edits() {
        let mut world = World::new(7);
        let (x, y, z) = (3, 40, -9);
        let original = world.block(x, y, z);
        assert!(!world.set_block(x, y, z, original), "unchanged block");
        assert!(!world.set_block(x, MIN_Y - 1, z, block::STONE));
        assert!(!world.set_block(x, MAX_Y, z, block::STONE));
        assert!(world.set_block(x, y, z, block::GLASS));
        assert_eq!(world.block(x, y, z), block::GLASS);
        assert!(world.chunk_has_edits(0, -1));
        assert_eq!(
            world.edits().collect::<Vec<_>>(),
            vec![(x, y, z, block::GLASS)]
        );
        assert!(world.set_block(x, y, z, block::DIAMOND_BLOCK));
        assert_eq!(world.edits().count(), 1);
        assert!(world.set_block(x, y, z, original));
        assert_eq!(world.block(x, y, z), original);
        assert_eq!(world.edits().count(), 0);
        assert!(!world.chunk_has_edits(0, -1));
    }

    #[test]
    fn different_seeds_differ() {
        let a = Terrain::new(1);
        let b = Terrain::new(2);
        let differing = (0..64)
            .filter(|i| a.column_at(i * 37, i * -53).h != b.column_at(i * 37, i * -53).h)
            .count();
        assert!(differing > 32, "{differing}");
    }

    #[test]
    fn every_biome_and_feature_appears() {
        let terrain = Terrain::new(0);
        let mut biomes = [0u32; 65];
        let mut trees = [0u32; 4];
        let mut decos = [0u32; kind::COUNT];
        let mut caves = 0;
        for j in -100..100 {
            for i in -100..100 {
                let (x, z) = (i * 41 + j, j * 41 - i);
                let col = terrain.column_at(x, z);
                biomes[col.biome as usize] += 1;
                trees[col.tree as usize] += 1;
                decos[col.deco as usize] += 1;
                caves += (col.carves.iter().any(|&(lo, hi)| lo <= hi)) as u32;
            }
        }
        for b in [
            biome::PLAINS,
            biome::FOREST,
            biome::BIRCH_FOREST,
            biome::TAIGA,
            biome::SNOWY_PLAINS,
            biome::SNOWY_TAIGA,
            biome::DESERT,
            biome::BEACH,
            biome::RIVER,
            biome::OCEAN,
        ] {
            assert!(biomes[b as usize] > 0, "biome {b} missing");
        }
        assert!(trees[1..].iter().all(|&n| n > 0), "{trees:?}");
        for k in [
            kind::SNOW,
            kind::CACTUS,
            kind::SHORT_GRASS,
            kind::FERN,
            kind::DEAD_BUSH,
        ] {
            assert!(decos[k as usize] > 0, "deco {k} missing");
        }
        for f in 0..5 {
            assert!(decos[(kind::FLOWER + f) as usize] > 0, "flower {f} missing");
        }
        assert!(caves > 0);
        let mut ores = [0u32; 7];
        for cy in 0..28 {
            for cz in -20..20 {
                for cx in -20..20 {
                    if let Some(cell) = terrain.ore_cell(cx, cy, cz) {
                        ores[cell.ore as usize] += 1;
                    }
                }
            }
        }
        assert!(ores.iter().all(|&n| n > 0), "{ores:?}");
    }

    /// Prints the seed-0 spawn and a vertical slice through it:
    /// `cargo test --release -p sculk world::tests::slice -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn slice() {
        let terrain = Terrain::new(0);
        let world = World::new(0);
        println!("spawn {:?}", world.spawn_point());
        let at = |name: &str, default: i32| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let z = at("SLICE_Z", world.spawn_point().2.floor() as i32);
        let x0 = at("SLICE_X", world.spawn_point().0.floor() as i32) - 60;
        for y in (0..at("SLICE_TOP", 110)).rev() {
            let mut line = format!("{y:4} ");
            for x in x0..x0 + 120 {
                line.push(match terrain.kind_at(x, y, z) {
                    kind::AIR => ' ',
                    kind::STONE => '#',
                    kind::DEEPSLATE => '%',
                    kind::BEDROCK => 'B',
                    kind::DIRT => 'd',
                    kind::GRASS | kind::SNOWY_GRASS => 'g',
                    kind::SAND | kind::SANDSTONE => 's',
                    kind::GRAVEL => 'v',
                    kind::WATER => '~',
                    kind::ICE => 'i',
                    kind::LAVA => 'L',
                    kind::SNOW | kind::SNOW_BLOCK => '*',
                    k if (kind::ORE..kind::OAK_LOG).contains(&k) => 'o',
                    k if (kind::OAK_LOG..kind::OAK_LEAVES).contains(&k) => '|',
                    k if (kind::OAK_LEAVES..kind::SHORT_GRASS).contains(&k) => '@',
                    _ => ',',
                });
            }
            println!("{line}");
        }
    }

    /// Prints a coarse biome map and height statistics:
    /// `cargo test --release -p sculk world::tests::map -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn map() {
        let terrain = Terrain::new(0);
        let mut heights = Vec::new();
        let mut biomes = [0u32; 65];
        for j in -40..40 {
            let mut line = String::new();
            for i in -80..80 {
                let col = terrain.column_at(i * 16, j * 32);
                heights.push(col.h);
                biomes[col.biome as usize] += 1;
                line.push(match col.biome {
                    biome::PLAINS => '.',
                    biome::FOREST => 'f',
                    biome::BIRCH_FOREST => 'b',
                    biome::TAIGA => 't',
                    biome::SNOWY_PLAINS => '-',
                    biome::SNOWY_TAIGA => 'T',
                    biome::DESERT => 'd',
                    biome::BEACH | biome::SNOWY_BEACH => ':',
                    biome::RIVER | biome::FROZEN_RIVER => 'r',
                    biome::STONY_PEAKS => '^',
                    biome::SNOWY_SLOPES => 'A',
                    _ => '~',
                });
            }
            println!("{line}");
        }
        heights.sort_unstable();
        let pct = |p: usize| heights[(heights.len() - 1) * p / 100];
        println!(
            "height p1 {} p10 {} p50 {} p90 {} p99 {} max {}",
            pct(1),
            pct(10),
            pct(50),
            pct(90),
            pct(99),
            heights[heights.len() - 1]
        );
        for (id, n) in biomes.iter().enumerate() {
            if *n > 0 {
                println!("biome {id}: {n}");
            }
        }
    }
}
