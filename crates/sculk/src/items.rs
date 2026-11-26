//! Item behavior backed by the generated vanilla tables: which item a block
//! state corresponds to, what a block item places, stack sizes, and what a
//! block drops when broken.
//!
//! Drop evaluation is a table lookup: the state's run gives a drop spec, the
//! spec gives a tool requirement and a slice of loot candidates, and each
//! candidate needs at most a tool test, a chance test and a count roll. All
//! random decisions are derived from the caller's `roll` by hashing it with a
//! per-decision salt, so one `roll` drives independent choices.

use crate::vanilla_items as v;

pub const AIR_ITEM: i32 = 0;

fn item_index(item: i32) -> Option<usize> {
    usize::try_from(item).ok().filter(|&i| i < v::ITEM_COUNT)
}

fn state_index(state: i32) -> Option<usize> {
    usize::try_from(state).ok().filter(|&s| s < v::STATE_COUNT)
}

/// Index of the run containing `state`, given the sorted first state of each
/// run (the first run always starts at state 0).
fn run_of(starts: &[u16], state: usize) -> usize {
    starts.partition_point(|&first| usize::from(first) <= state) - 1
}

/// The item a block state corresponds to (its block item, or the item its
/// loot table yields, as for wall torches and crops); `AIR_ITEM` if none.
#[allow(dead_code)] // Not needed by the server yet (no pick-block handling).
pub fn item_from_block_state(state: i32) -> i32 {
    state_index(state).map_or(AIR_ITEM, |s| {
        i32::from(v::BLOCK_ITEM[run_of(&v::BLOCK_FIRST_STATE, s)])
    })
}

/// Default state placed by a block item; `vanilla::block::AIR` otherwise.
pub fn block_state_from_item(item: i32) -> i32 {
    item_index(item).map_or(crate::vanilla::block::AIR, |i| i32::from(v::PLACE_STATE[i]))
}

/// Maximum stack size of an item; unknown ids stack to 64.
pub fn max_stack(item: i32) -> i16 {
    item_index(item).map_or(64, |i| i16::from(v::MAX_STACK[i]))
}

/// A 32-bit integer hash (xorshift-multiply), salted per random decision.
fn mix(roll: u32, salt: u32) -> u32 {
    let mut x = roll ^ salt.wrapping_mul(0x9E37_79B9);
    x = (x ^ (x >> 16)).wrapping_mul(0x7FEB_352D);
    x = (x ^ (x >> 15)).wrapping_mul(0x846C_A68B);
    x ^ (x >> 16)
}

/// Harvest class bit and `match_tool` predicate bits of a held item.
fn tool_bits(item: i32) -> (u16, u8) {
    let Ok(id) = u16::try_from(item) else {
        return (0, 0);
    };
    match v::TOOL_ITEMS.binary_search_by_key(&id, |t| t.item) {
        Ok(i) => {
            let t = v::TOOL_ITEMS[i];
            let class = if t.class == 0xFF { 0 } else { 1 << t.class };
            (class, t.preds)
        }
        Err(_) => (0, 0),
    }
}

fn tool_matches(code: u8, preds: u8) -> bool {
    if code == 0 {
        return true;
    }
    let held = preds & (1 << ((code - 1) / 2)) != 0;
    if code % 2 == 1 {
        held
    } else {
        !held
    }
}

fn roll_count(count: &v::Count, roll: u32, salt: u32) -> i32 {
    let (min, max) = (i32::from(count.min), i32::from(count.max));
    let span = (max - min + 1) as u32;
    let mut n = min;
    if span > 1 {
        n += (mix(roll, salt) % span) as i32;
    }
    for trial in 0..u32::from(count.bin_n) {
        if mix(roll, salt + 1 + trial) < count.bin_p {
            n += 1;
        }
    }
    n.clamp(i32::from(count.floor), i32::from(count.ceil))
}

/// Calls `emit(item, count)` for every stack dropped when `block_state` is
/// broken with `held_item`: one per loot pool that yields something.
pub fn for_each_drop(block_state: i32, held_item: i32, roll: u32, mut emit: impl FnMut(i32, i16)) {
    let Some(state) = state_index(block_state) else {
        return;
    };
    let run = run_of(&v::DROP_RUN_START, state);
    let spec = v::DROP_SPECS[usize::from(v::DROP_RUN_SPEC[run])];
    let (class, preds) = tool_bits(held_item);
    if spec.req != 0 && v::REQ_MASKS[usize::from(spec.req)] & class == 0 {
        return;
    }
    let start = usize::from(spec.start);
    let candidates = &v::CANDIDATES[start..start + usize::from(spec.len)];
    let mut pool_done = false;
    for (i, c) in candidates.iter().enumerate() {
        let salt = (i as u32) << 4;
        if !pool_done
            && tool_matches(c.tool, preds)
            && (c.chance == 0 || mix(roll, salt) < c.chance)
        {
            pool_done = true;
            if c.item != 0 {
                let n = roll_count(&v::COUNTS[usize::from(c.count)], roll, salt | 1);
                if n > 0 {
                    emit(i32::from(c.item), n.min(i32::from(i16::MAX)) as i16);
                }
            }
        }
        if c.last {
            pool_done = false;
        }
    }
}

/// The stack dropped when `block_state` is broken with `held_item`, or
/// `(AIR_ITEM, 0)`. When several loot pools yield (leaves, mature crops),
/// only the first is returned; see `for_each_drop` for all of them.
#[cfg(test)]
pub fn break_drop(block_state: i32, held_item: i32, roll: u32) -> (i32, i16) {
    let mut first = (AIR_ITEM, 0);
    for_each_drop(block_state, held_item, roll, |item, count| {
        if first.1 == 0 {
            first = (item, count);
        }
    });
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vanilla::block;
    use crate::vanilla_items::item;

    /// Deterministic stream of rolls for statistical tests.
    fn rolls(n: usize) -> impl Iterator<Item = u32> {
        let mut x = 0x1234_5678u32;
        (0..n).map(move |_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        })
    }

    fn states_of(default: i32) -> std::ops::Range<i32> {
        let b = run_of(&v::BLOCK_FIRST_STATE, default as usize);
        let first = i32::from(v::BLOCK_FIRST_STATE[b]);
        let end = v::BLOCK_FIRST_STATE
            .get(b + 1)
            .map_or(v::STATE_COUNT as i32, |&s| i32::from(s));
        first..end
    }

    #[test]
    fn stone_needs_a_pickaxe() {
        assert_eq!(break_drop(block::STONE, AIR_ITEM, 7), (AIR_ITEM, 0));
        assert_eq!(break_drop(block::STONE, item::DIRT, 7), (AIR_ITEM, 0));
        assert_eq!(
            break_drop(block::STONE, item::WOODEN_SHOVEL, 7),
            (AIR_ITEM, 0)
        );
        assert_eq!(
            break_drop(block::STONE, item::WOODEN_PICKAXE, 7),
            (item::COBBLESTONE, 1)
        );
        assert_eq!(
            break_drop(block::STONE, item::GOLDEN_PICKAXE, 7),
            (item::COBBLESTONE, 1)
        );
    }

    #[test]
    fn iron_ore_needs_stone_tier() {
        for tool in [
            AIR_ITEM,
            item::WOODEN_PICKAXE,
            item::GOLDEN_PICKAXE,
            item::IRON_AXE,
        ] {
            assert_eq!(break_drop(block::IRON_ORE, tool, 3), (AIR_ITEM, 0));
        }
        for tool in [
            item::STONE_PICKAXE,
            item::COPPER_PICKAXE,
            item::IRON_PICKAXE,
            item::NETHERITE_PICKAXE,
        ] {
            assert_eq!(break_drop(block::IRON_ORE, tool, 3), (item::RAW_IRON, 1));
        }
        assert_eq!(
            break_drop(block::OBSIDIAN, item::IRON_PICKAXE, 3),
            (AIR_ITEM, 0)
        );
        assert_eq!(
            break_drop(block::OBSIDIAN, item::DIAMOND_PICKAXE, 3),
            (item::OBSIDIAN, 1)
        );
    }

    #[test]
    fn hand_harvestable_blocks() {
        assert_eq!(break_drop(block::GRASS_BLOCK, AIR_ITEM, 0), (item::DIRT, 1));
        assert_eq!(break_drop(block::DIRT, AIR_ITEM, 0), (item::DIRT, 1));
        assert_eq!(break_drop(block::OAK_LOG, AIR_ITEM, 0), (item::OAK_LOG, 1));
        assert_eq!(break_drop(block::RAIL, AIR_ITEM, 0), (item::RAIL, 1));
        assert_eq!(break_drop(block::WALL_TORCH, AIR_ITEM, 0), (item::TORCH, 1));
        assert_eq!(
            break_drop(block::OAK_WALL_SIGN, AIR_ITEM, 0),
            (item::OAK_SIGN, 1)
        );
        assert_eq!(break_drop(block::GLASS, AIR_ITEM, 0), (AIR_ITEM, 0));
        assert_eq!(break_drop(block::AIR, AIR_ITEM, 0), (AIR_ITEM, 0));
        assert_eq!(break_drop(-1, AIR_ITEM, 0), (AIR_ITEM, 0));
        assert_eq!(break_drop(i32::MAX, AIR_ITEM, 0), (AIR_ITEM, 0));
    }

    #[test]
    fn shears_and_swords() {
        assert_eq!(break_drop(block::COBWEB, AIR_ITEM, 0), (AIR_ITEM, 0));
        assert_eq!(
            break_drop(block::COBWEB, item::WOODEN_SWORD, 0),
            (item::STRING, 1)
        );
        assert_eq!(
            break_drop(block::COBWEB, item::SHEARS, 0),
            (item::COBWEB, 1)
        );
        assert_eq!(
            break_drop(block::OAK_LEAVES, item::SHEARS, 0),
            (item::OAK_LEAVES, 1)
        );
    }

    #[test]
    fn leaves_drop_at_vanilla_odds() {
        const N: usize = 400_000;
        let (mut first, mut all) = ([0usize; 3], [0usize; 3]);
        let mut stick_counts = [0usize; 3];
        let kinds = [item::OAK_SAPLING, item::STICK, item::APPLE];
        for roll in rolls(N) {
            let (drop, _) = break_drop(block::OAK_LEAVES, AIR_ITEM, roll);
            if let Some(k) = kinds.iter().position(|&i| i == drop) {
                first[k] += 1;
            } else {
                assert_eq!(drop, AIR_ITEM);
            }
            for_each_drop(block::OAK_LEAVES, AIR_ITEM, roll, |drop, count| {
                let k = kinds.iter().position(|&i| i == drop).unwrap();
                all[k] += 1;
                if drop == item::STICK {
                    stick_counts[count as usize] += 1;
                }
            });
        }
        let rate = |n: usize| n as f64 / N as f64;
        // Independent pools: sapling 1/20, sticks 1/50, apple 1/200.
        assert!(
            (rate(all[0]) - 0.05).abs() < 0.002,
            "sapling {}",
            rate(all[0])
        );
        assert!(
            (rate(all[1]) - 0.02).abs() < 0.001,
            "stick {}",
            rate(all[1])
        );
        assert!(
            (rate(all[2]) - 0.005).abs() < 0.0005,
            "apple {}",
            rate(all[2])
        );
        // break_drop keeps only the first pool that yields.
        assert!((rate(first[0]) - 0.05).abs() < 0.002);
        assert!((rate(first[1]) - 0.019).abs() < 0.001);
        assert!((rate(first[2]) - 0.004655).abs() < 0.0005);
        // Sticks come in stacks of one or two, evenly.
        assert_eq!(stick_counts[0], 0);
        let ones = stick_counts[1] as f64 / all[1] as f64;
        assert!((ones - 0.5).abs() < 0.05, "single sticks {ones}");
    }

    #[test]
    fn state_dependent_drops() {
        let slab_counts: Vec<i16> = states_of(block::OAK_SLAB)
            .map(|s| break_drop(s, AIR_ITEM, 0).1)
            .collect();
        assert!(slab_counts.contains(&1) && slab_counts.contains(&2));
        // Mature wheat yields wheat first, seeds from the second pool.
        let mature = states_of(block::WHEAT).last().unwrap();
        assert_eq!(
            break_drop(block::WHEAT, AIR_ITEM, 0),
            (item::WHEAT_SEEDS, 1)
        );
        assert_eq!(break_drop(mature, AIR_ITEM, 0).0, item::WHEAT);
        let mut seeds = 0;
        for_each_drop(mature, AIR_ITEM, 9, |drop, count| {
            if drop == item::WHEAT_SEEDS {
                seeds += count;
            }
        });
        assert_eq!(seeds, 1);
        // Snow layers need a shovel and drop one snowball per layer.
        assert_eq!(break_drop(block::SNOW, AIR_ITEM, 0), (AIR_ITEM, 0));
        let snowballs: Vec<i16> = states_of(block::SNOW)
            .map(|s| break_drop(s, item::WOODEN_SHOVEL, 0).1)
            .collect();
        assert_eq!(snowballs, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn block_item_round_trip() {
        let mut checked = 0;
        for (b, &first) in v::BLOCK_FIRST_STATE.iter().enumerate() {
            let default = i32::from(v::BLOCK_DEFAULT_STATE[b]);
            let end = v::BLOCK_FIRST_STATE
                .get(b + 1)
                .map_or(v::STATE_COUNT as i32, |&s| i32::from(s));
            let item = item_from_block_state(default);
            if item == AIR_ITEM || states_of(block_state_from_item(item)) != states_of(default) {
                continue; // no item, or the item places a sibling (wall torch -> torch)
            }
            for state in i32::from(first)..end {
                assert_eq!(item_from_block_state(state), item);
                assert_eq!(block_state_from_item(item_from_block_state(state)), default);
            }
            checked += 1;
        }
        assert!(checked > 1000, "only {checked} round-tripping blocks");
        assert_eq!(item_from_block_state(block::OAK_LOG), item::OAK_LOG);
        assert_eq!(block_state_from_item(item::OAK_LOG), block::OAK_LOG);
        assert_eq!(item_from_block_state(block::WALL_TORCH), item::TORCH);
        assert_eq!(block_state_from_item(item::REDSTONE), block::REDSTONE_WIRE);
        assert_eq!(block_state_from_item(item::WHEAT_SEEDS), block::WHEAT);
        assert_eq!(block_state_from_item(item::WHEAT), block::AIR);
        assert_eq!(block_state_from_item(item::STICK), block::AIR);
        assert_eq!(block_state_from_item(-5), block::AIR);
        assert_eq!(item_from_block_state(block::AIR), AIR_ITEM);
    }

    #[test]
    fn stack_sizes() {
        assert_eq!(max_stack(item::COBBLESTONE), 64);
        assert_eq!(max_stack(item::WOODEN_PICKAXE), 1);
        assert_eq!(max_stack(item::DIAMOND_CHESTPLATE), 1);
        assert_eq!(max_stack(item::WATER_BUCKET), 1);
        assert_eq!(max_stack(item::BUCKET), 16);
        assert_eq!(max_stack(item::ENDER_PEARL), 16);
        assert_eq!(max_stack(item::OAK_SIGN), 16);
        assert_eq!(max_stack(item::EGG), 16);
    }
}
