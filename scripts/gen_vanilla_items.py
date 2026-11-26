#!/usr/bin/env python3
"""Generate Sculk's item, block-drop and crafting tables from Mojang's data
generator output.

Produce the input with the official server jar for the targeted version:

    java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar \
        --reports --server --output generated

then run:

    python3 scripts/gen_vanilla_items.py path/to/generated

Writes crates/sculk/src/vanilla_items.rs. The output is deterministic: every
table is built from sorted inputs.

Sources and inference rules
---------------------------
Items: protocol ids come from reports/registries.json (minecraft:item). The
max stack size of every item is read from its `minecraft:max_stack_size`
component in reports/minecraft/components/item/*.json; nothing is inferred.

Block <-> item: an item is the block item of a block when both share a name
and the item's `item_name` translation key is `block.minecraft.<name>` (this
rejects e.g. the `wheat` crop item). A block without such an item takes the
single item kind its own loot table can drop (wall torch -> torch, wheat ->
wheat_seeds, redstone_wire -> redstone). Such a loot-derived item also places
that block, unless it already places a same-named block; among several
candidate blocks, `wall_`, `attached_`, `_plant` and `potted_` variants lose,
then the lowest block id wins.

Tool requirement: whether a block needs the correct tool for drops is not in
the data, so it is inferred as
    required = mineable/pickaxe + needs_{stone,iron,diamond}_tool
             + blocks named directly (not through a mineable/* tag) by a tool
               rule with correct_for_drops=true (cobweb)
             + SHOVEL_REQUIRED (snow layers and snow block)
             - HAND_HARVESTABLE (pickaxe-mineable blocks vanilla still drops
               by hand: rails, pistons, buttons, shulker boxes, ...)
Whether a held item is correct is evaluated exactly like vanilla's
`minecraft:tool` component: the first rule naming the block that carries
`correct_for_drops` decides. The incorrect_for_*_tool tags enter through those
rules, so tiers (gold = wood, copper = stone) fall out of the data. Tool items
are grouped into classes with identical correct-sets; each required block
stores a bit mask of the classes that may harvest it.

Drops: block loot tables are folded per block state at generation time. A
block without its own table that has `wall_` in its name uses the table of the
block named without it (vanilla's dropsLike: wall torches, signs, banners,
heads, coral fans).
`block_state_property` is evaluated against the state; `survives_explosion`,
`location_check` and `entity_properties` are true; silk-touch `match_tool` is
false (Sculk has no enchantments); item-list `match_tool` (shears,
#cluster_max_harvestables) stays as a runtime test on the held item.
`random_chance` and `table_bonus` keep their unenchanted chance. `set_count`
(constant, uniform, binomial) and `limit_count` shape the count; fortune,
`apply_bonus` and `explosion_decay` are ignored. Each pool
becomes an ordered list of candidates; the first whose conditions pass is
emitted. States with identical results are run-length encoded.

Recipes: every minecraft:crafting_shaped and minecraft:crafting_shapeless
recipe whose result carries no components. Ingredients become deduplicated,
sorted sets of item ids (tags resolved recursively). Shaped patterns are
trimmed to their bounding box and bucketed by (width, height, occupied cells);
shapeless recipes by ingredient count. Within a bucket recipes are sorted by
id, and `rank` is the recipe's position in id order.
"""
import json
import math
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "sculk" / "src" / "vanilla_items.rs"

SHOVEL_REQUIRED = ["snow", "snow_block"]
HAND_HARVESTABLE_TAGS = ["rails", "buttons", "shulker_boxes"]
HAND_HARVESTABLE = [
    "piston", "sticky_piston", "piston_head", "conduit", "heavy_core", "packed_mud",
    "ice", "packed_ice", "blue_ice",
    "amethyst_cluster", "large_amethyst_bud", "medium_amethyst_bud", "small_amethyst_bud",
]
SECONDARY_MARKERS = ["wall_", "attached_", "_plant", "potted_"]

EMPTY = 0xFFFF


def strip(name):
    return name.split(":", 1)[1] if ":" in name else name


def const_name(name):
    return strip(name).replace("/", "_").replace(".", "_").upper()


def load_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def load_tags(data_dir, registry):
    base = data_dir / "tags" / registry
    raw = {}
    for p in sorted(base.rglob("*.json")):
        name = str(p.relative_to(base).with_suffix("")).replace("\\", "/")
        raw[name] = load_json(p)["values"]
    resolved = {}

    def resolve(name, stack):
        if name in resolved:
            return resolved[name]
        if name in stack:
            raise SystemExit(f"tag cycle at {registry}/{name}")
        out = set()
        for value in raw[name]:
            required = True
            if isinstance(value, dict):
                required = value.get("required", True)
                value = value["id"]
            if value.startswith("#"):
                ref = strip(value[1:])
                if ref in raw:
                    out |= resolve(ref, stack | {name})
                elif required:
                    raise SystemExit(f"missing tag {value} in {name}")
            else:
                out.add(strip(value))
        resolved[name] = frozenset(out)
        return resolved[name]

    for name in sorted(raw):
        resolve(name, frozenset())
    return resolved


def block_set(spec, block_tags):
    """Resolve a HolderSet (`#tag`, id, or list of ids) to block names."""
    if isinstance(spec, list):
        out = set()
        for s in spec:
            out |= block_set(s, block_tags)
        return frozenset(out)
    if spec.startswith("#"):
        return block_tags[strip(spec[1:])]
    return frozenset([strip(spec)])


def item_set(spec, item_tags, item_ids):
    if isinstance(spec, list):
        out = set()
        for s in spec:
            out |= item_set(s, item_tags, item_ids)
        return frozenset(out)
    if spec.startswith("#"):
        return frozenset(item_ids[n] for n in item_tags[strip(spec[1:])] if n in item_ids)
    return frozenset([item_ids[strip(spec)]])


# ---------------------------------------------------------------------------
# Loot table folding
# ---------------------------------------------------------------------------

class Unsupported(Exception):
    pass


# A requirement is (tool, chance): tool is None or (predicate index, wanted).
ALWAYS = (None, 1.0)


def req_and(a, b):
    if a is None or b is None:
        return None
    tool_a, chance_a = a
    tool_b, chance_b = b
    tool = tool_a
    if tool_b is not None:
        if tool_a is None:
            tool = tool_b
        elif tool_a[0] == tool_b[0]:
            if tool_a[1] != tool_b[1]:
                return None
        else:
            raise Unsupported("two different tool predicates in one condition")
    return (tool, chance_a * chance_b)


class Folder:
    def __init__(self, item_ids, item_tags):
        self.item_ids = item_ids
        self.item_tags = item_tags
        self.tool_preds = {}  # canonical json -> index

    def tool_pred(self, items_spec):
        key = json.dumps(items_spec, sort_keys=True)
        if key not in self.tool_preds:
            self.tool_preds[key] = items_spec
        return key

    def cond(self, c, block, props):
        """Fold one condition: None (never), or a requirement tuple."""
        kind = strip(c["condition"])
        if kind in ("survives_explosion", "location_check", "entity_properties"):
            return ALWAYS
        if kind == "block_state_property":
            if strip(c["block"]) != block:
                return None
            for key, want in c.get("properties", {}).items():
                have = props.get(key)
                if isinstance(want, dict):
                    lo, hi = want.get("min"), want.get("max")
                    v = int(have)
                    if (lo is not None and v < int(lo)) or (hi is not None and v > int(hi)):
                        return None
                elif have != str(want).lower():
                    return None
            return ALWAYS
        if kind == "match_tool":
            pred = c["predicate"]
            if set(pred) == {"predicates"} and "minecraft:enchantments" in pred["predicates"]:
                return None
            if set(pred) == {"items"}:
                return ((self.tool_pred(pred["items"]), True), 1.0)
            raise Unsupported(f"match_tool {pred}")
        if kind == "random_chance":
            chance = c["chance"]
            if not isinstance(chance, (int, float)):
                raise Unsupported(f"random_chance {chance}")
            return (None, float(chance))
        if kind == "random_chance_with_enchanted_bonus":
            return (None, float(c["unenchanted_chance"]))
        if kind == "table_bonus":
            return (None, float(c["chances"][0]))
        if kind == "inverted":
            inner = self.cond(c["term"], block, props)
            if inner is None:
                return ALWAYS
            if inner == ALWAYS:
                return None
            tool, chance = inner
            if chance != 1.0:
                raise Unsupported("inverted random condition")
            return ((tool[0], not tool[1]), 1.0)
        if kind == "any_of":
            live = [self.cond(t, block, props) for t in c["terms"]]
            live = [r for r in live if r is not None]
            if any(r == ALWAYS for r in live):
                return ALWAYS
            if not live:
                return None
            if len(set(live)) == 1:
                return live[0]
            raise Unsupported(f"any_of with several live terms {live}")
        if kind == "all_of":
            return self.conds(c["terms"], block, props)
        raise Unsupported(f"condition {kind}")

    def conds(self, conds, block, props):
        req = ALWAYS
        for c in conds or []:
            req = req_and(req, self.cond(c, block, props))
            if req is None:
                return None
        return req

    def number(self, value):
        """Number provider -> (min, max, binomial n, binomial p)."""
        if isinstance(value, (int, float)):
            v = math.floor(value)
            return (v, v, 0, 0.0)
        kind = strip(value.get("type", "minecraft:uniform"))
        if kind == "constant":
            v = math.floor(value["value"])
            return (v, v, 0, 0.0)
        if kind == "uniform":
            return (math.floor(value["min"]), math.floor(value["max"]), 0, 0.0)
        if kind == "binomial":
            return (0, 0, int(value["n"]), float(value["p"]))
        raise Unsupported(f"number provider {value}")

    def count(self, functions, block, props):
        """Fold count-affecting functions into (min, max, n, p, floor, ceil)."""
        lo, hi, bn, bp = 1, 1, 0, 0.0
        floor, ceil = -128, 127
        for f in functions:
            req = self.conds(f.get("conditions"), block, props)
            if req is None:
                continue
            if req != ALWAYS:
                raise Unsupported(f"runtime condition on function {f['function']}")
            kind = strip(f["function"])
            if kind == "set_count":
                a, b, n, p = self.number(f["count"])
                if f.get("add", False):
                    if n and bn and p != bp:
                        raise Unsupported("two binomials")
                    lo, hi, bn, bp = lo + a, hi + b, bn + n, (p if n else bp)
                else:
                    lo, hi, bn, bp = a, b, n, p
                    floor, ceil = -128, 127
            elif kind == "limit_count":
                limit = f["limit"]
                if "min" in limit:
                    floor = max(floor, math.floor(limit["min"]))
                if "max" in limit:
                    ceil = min(ceil, math.floor(limit["max"]))
        return (lo, hi, bn, bp, floor, ceil)

    def entry(self, e, block, props, pool_functions):
        """Expand an entry into ordered candidates (req, item id or 0, count)."""
        req = self.conds(e.get("conditions"), block, props)
        if req is None:
            return []
        kind = strip(e["type"])
        if kind == "item":
            count = self.count(e.get("functions", []) + pool_functions, block, props)
            return [(req, self.item_ids[strip(e["name"])], count)]
        if kind in ("empty", "dynamic"):
            return [(req, 0, (0, 0, 0, 0.0, -128, 127))]
        if kind == "alternatives":
            if req[1] != 1.0:
                raise Unsupported("random condition on alternatives")
            out = []
            for child in e["children"]:
                for r, item, count in self.entry(child, block, props, pool_functions):
                    r = req_and(req, r)
                    if r is not None:
                        out.append((r, item, count))
            return out
        raise Unsupported(f"entry {kind}")

    def table(self, table, block, props):
        """Fold a loot table for one state into a tuple of pools."""
        pools = []
        for pool in table.get("pools", []):
            if pool["rolls"] != 1.0 or pool.get("bonus_rolls", 0.0) != 0.0:
                raise Unsupported("multiple rolls")
            if len(pool["entries"]) != 1:
                raise Unsupported("weighted pool")
            pool_req = self.conds(pool.get("conditions"), block, props)
            if pool_req is None:
                continue
            if pool_req[1] != 1.0:
                raise Unsupported("random pool condition")
            cands = []
            for r, item, count in self.entry(pool["entries"][0], block, props, pool.get("functions", [])):
                r = req_and(pool_req, r)
                if r is None:
                    continue
                cands.append((r, item, count))
                if r == ALWAYS:
                    break
            while cands and cands[-1][1] == 0:
                cands.pop()
            if cands:
                pools.append(tuple(cands))
        return tuple(pools)


# ---------------------------------------------------------------------------
# Rust emission helpers
# ---------------------------------------------------------------------------

def rust_array(name, ty, values, per_line=16):
    out = [f"pub static {name}: [{ty}; {len(values)}] = [\n"]
    for i in range(0, len(values), per_line):
        out.append("    " + " ".join(f"{v}," for v in values[i:i + per_line]) + "\n")
    out.append("];\n\n")
    return "".join(out)


def rust_structs(name, ty, rows):
    out = [f"pub static {name}: [{ty}; {len(rows)}] = [\n"]
    for row in rows:
        out.append(f"    {row},\n")
    out.append("];\n\n")
    return "".join(out)


def chance_u32(p):
    if p >= 1.0:
        return 0
    return min(max(round(p * 2**32), 1), 2**32 - 1)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    gen = pathlib.Path(sys.argv[1])
    reports = gen / "reports"
    data_dir = gen / "data" / "minecraft"
    blocks = load_json(reports / "blocks.json")
    registries = load_json(reports / "registries.json")

    item_ids = {strip(n): e["protocol_id"] for n, e in registries["minecraft:item"]["entries"].items()}
    item_names = {i: n for n, i in item_ids.items()}
    n_items = len(item_ids)
    if sorted(item_names) != list(range(n_items)) or item_ids["air"] != 0 or n_items > 0xFFFE:
        raise SystemExit("unexpected item id layout")
    components = {}
    comp_dir = reports / "minecraft" / "components" / "item"
    for name in sorted(item_ids):
        components[name] = load_json(comp_dir / f"{name}.json")["components"]
    item_tags = load_tags(data_dir, "item")
    block_tags = load_tags(data_dir, "block")

    # Block states, in id order.
    block_list = []  # (first_state, name, default_state, [(state id, props)])
    for name, b in blocks.items():
        states = [(s["id"], s.get("properties", {})) for s in b["states"]]
        default = next(s["id"] for s in b["states"] if s.get("default"))
        block_list.append((states[0][0], strip(name), default, states))
    block_list.sort()
    n_states = sum(len(b[3]) for b in block_list)
    expect = 0
    for first, name, _, states in block_list:
        if [s for s, _ in states] != list(range(first, first + len(states))) or first != expect:
            raise SystemExit(f"non-contiguous states at {name}")
        expect = first + len(states)
    if n_states > 0xFFFF:
        raise SystemExit("too many block states for u16")
    block_names = {b[1] for b in block_list}
    default_state = {b[1]: b[2] for b in block_list}
    block_index = {b[1]: i for i, b in enumerate(block_list)}

    # --- Tool classes and requirements -------------------------------------
    tools = {n: c["minecraft:tool"]["rules"] for n, c in components.items() if "minecraft:tool" in c}
    required = set(block_tags["mineable/pickaxe"])
    for tier in ("stone", "iron", "diamond"):
        required |= block_tags[f"needs_{tier}_tool"]
    for rules in tools.values():
        for rule in rules:
            if rule.get("correct_for_drops") and not (isinstance(rule["blocks"], str) and rule["blocks"].startswith("#minecraft:mineable/")):
                required |= block_set(rule["blocks"], block_tags)
    required |= set(SHOVEL_REQUIRED)
    for t in HAND_HARVESTABLE_TAGS:
        required -= block_tags[t]
    required -= set(HAND_HARVESTABLE)
    required &= block_names

    def correct(rules, block):
        for rule in rules:
            if "correct_for_drops" in rule and block in block_set(rule["blocks"], block_tags):
                return rule["correct_for_drops"]
        return False

    class_of_set = {}
    tool_class = {}
    for name in sorted(tools, key=lambda n: item_ids[n]):
        ok = frozenset(b for b in required if correct(tools[name], b))
        if ok:
            if ok not in class_of_set:
                class_of_set[ok] = len(class_of_set)
            tool_class[name] = class_of_set[ok]
    if len(class_of_set) > 16:
        raise SystemExit("more than 16 tool classes")
    req_mask = {}
    for b in sorted(required):
        mask = 0
        for ok, cls in class_of_set.items():
            if b in ok:
                mask |= 1 << cls
        req_mask[b] = mask
    mask_list = [0] + sorted(set(req_mask.values()) - {0}) if req_mask else [0]
    # Required blocks no tool can harvest keep a distinct non-zero entry: mask 0
    # in slot 0 means "no requirement", so use a sentinel index for "never".
    never_index = None
    if any(m == 0 for m in req_mask.values()):
        mask_list.append(0)
        never_index = len(mask_list) - 1

    def req_index(block):
        if block not in req_mask:
            return 0
        m = req_mask[block]
        return never_index if m == 0 else mask_list.index(m)

    # --- Loot folding -------------------------------------------------------
    folder = Folder(item_ids, item_tags)
    loot_dir = data_dir / "loot_table" / "blocks"
    state_pools = [()] * n_states
    for first, name, _, states in block_list:
        path = loot_dir / f"{name}.json"
        if not path.exists():
            # Wall variants share the loot table of their floor block.
            path = loot_dir / f"{name.replace('wall_', '', 1)}.json"
        if not path.exists():
            continue
        table = load_json(path)
        for sid, props in states:
            try:
                state_pools[sid] = folder.table(table, name, props)
            except Unsupported as err:
                raise SystemExit(f"{name}: unsupported loot construct: {err}")
    pred_keys = sorted(folder.tool_preds)
    if len(pred_keys) > 8:
        raise SystemExit("more than 8 tool predicates")
    pred_index = {k: i for i, k in enumerate(pred_keys)}
    pred_items = [item_set(folder.tool_preds[k], item_tags, item_ids) for k in pred_keys]

    count_specs = {}
    cand_rows = []
    spec_index = {}
    spec_rows = []
    count_rows = []

    def count_id(c):
        lo, hi, bn, bp, floor, ceil = c
        key = (lo, hi, bn, chance_u32(bp) if bn else 0, max(floor, -128), min(ceil, 127))
        if not -128 <= lo <= hi <= 127 or bn > 12:
            raise SystemExit(f"count out of range {c}")
        if key not in count_specs:
            count_specs[key] = len(count_specs)
            count_rows.append(key)
        return count_specs[key]

    def spec_id(block, pools):
        key = (req_index(block) if pools else 0, pools)
        if key in spec_index:
            return spec_index[key]
        start = len(cand_rows)
        for pool in pools:
            for i, (req, item, count) in enumerate(pool):
                tool, chance = req
                tool_code = 0 if tool is None else 1 + 2 * pred_index[tool[0]] + (0 if tool[1] else 1)
                cand_rows.append((chance_u32(chance), item, count_id(count), tool_code, i == len(pool) - 1))
        length = len(cand_rows) - start
        if length > 255 or start > 0xFFFF:
            raise SystemExit("drop table too large")
        spec_index[key] = len(spec_rows)
        spec_rows.append((start, length, key[0]))
        return spec_index[key]

    spec_id("air", ())
    state_block = [None] * n_states
    for first, name, _, states in block_list:
        for sid, _ in states:
            state_block[sid] = name
    drop_starts, drop_specs = [], []
    for sid in range(n_states):
        s = spec_id(state_block[sid], state_pools[sid])
        if not drop_specs or drop_specs[-1] != s:
            drop_starts.append(sid)
            drop_specs.append(s)

    # --- Block <-> item -----------------------------------------------------
    block_item = {}
    for name in sorted(block_names):
        if name in item_ids and components[name].get("minecraft:item_name", {}).get("translate") == f"block.minecraft.{name}":
            block_item[name] = item_ids[name]
    placed_by_name = set(block_item.values())
    derived = {}
    for first, name, default, states in block_list:
        if name in block_item:
            continue
        kinds = {c[1] for p in state_pools[default] for c in p} - {0}
        if not kinds:
            kinds = {c[1] for sid, _ in states for p in state_pools[sid] for c in p} - {0}
        if len(kinds) == 1:
            derived[name] = kinds.pop()
    place = [0] * n_items
    for name, item in block_item.items():
        place[item] = default_state[name]
    candidates = {}
    for name, item in derived.items():
        block_item[name] = item
        if item not in placed_by_name:
            candidates.setdefault(item, []).append(name)
    for item, names in sorted(candidates.items()):
        names.sort(key=lambda n: (any(m in n for m in SECONDARY_MARKERS), block_index[n]))
        place[item] = default_state[names[0]]
        print(f"  {item_names[item]} places {names[0]}")
    block_rows = [(b[0], block_item.get(b[1], 0), b[2]) for b in block_list]

    max_stack = [components[item_names[i]].get("minecraft:max_stack_size", 64) for i in range(n_items)]

    tool_rows = []
    for name in sorted(item_ids, key=lambda n: item_ids[n]):
        iid = item_ids[name]
        cls = tool_class.get(name, 0xFF)
        preds = sum(1 << i for i, s in enumerate(pred_items) if iid in s)
        if cls != 0xFF or preds:
            tool_rows.append((iid, cls, preds))

    # --- Recipes ------------------------------------------------------------
    ing_index = {}
    ing_sets = []

    def ingredient(spec):
        names = spec if isinstance(spec, list) else [spec]
        ids = set()
        for n in names:
            if n.startswith("#"):
                ids |= {item_ids[m] for m in item_tags[strip(n[1:])] if m in item_ids}
            else:
                ids.add(item_ids[strip(n)])
        key = tuple(sorted(ids))
        if not key:
            raise SystemExit(f"empty ingredient {spec}")
        if key not in ing_index:
            ing_index[key] = len(ing_sets)
            ing_sets.append(key)
        return ing_index[key]

    recipes = []
    skipped = 0
    for path in sorted((data_dir / "recipe").glob("*.json")):
        r = load_json(path)
        kind = strip(r["type"])
        if kind not in ("crafting_shaped", "crafting_shapeless"):
            continue
        res = r["result"]
        if set(res) - {"id", "count"}:
            skipped += 1
            continue
        result, count = item_ids[strip(res["id"])], res.get("count", 1)
        if kind == "crafting_shaped":
            rows = r["pattern"]
            used_rows = [i for i, row in enumerate(rows) if row.strip()]
            used_cols = [j for j in range(max(map(len, rows))) if any(j < len(row) and row[j] != " " for row in rows)]
            y0, y1, x0, x1 = used_rows[0], used_rows[-1], used_cols[0], used_cols[-1]
            keys = {k: ingredient(v) for k, v in sorted(r["key"].items())}
            cells = []
            for y in range(y0, y1 + 1):
                for x in range(x0, x1 + 1):
                    ch = rows[y][x] if x < len(rows[y]) else " "
                    cells.append(EMPTY if ch == " " else keys[ch])
            w, h = x1 - x0 + 1, y1 - y0 + 1
            mirrored = [cells[y * w + (w - 1 - x)] for y in range(h) for x in range(w)]
            recipes.append(("shaped", path.stem, w, h, cells, cells == mirrored, result, count))
        else:
            ings = sorted(ingredient(v) for v in r["ingredients"])
            if not 1 <= len(ings) <= 9:
                raise SystemExit(f"bad shapeless recipe {path.stem}")
            recipes.append(("shapeless", path.stem, 0, 0, ings, False, result, count))
    rank = {r[1]: i for i, r in enumerate(sorted(recipes, key=lambda r: r[1]))}

    shaped = sorted(
        (r for r in recipes if r[0] == "shaped"),
        key=lambda r: (((r[2] - 1) * 3 + (r[3] - 1)) * 9 + sum(c != EMPTY for c in r[4]) - 1, r[1]),
    )
    shaped_cells, shaped_rows, shaped_offsets = [], [], [0] * 82
    for r in shaped:
        bucket = ((r[2] - 1) * 3 + (r[3] - 1)) * 9 + sum(c != EMPTY for c in r[4]) - 1
        shaped_offsets[bucket + 1] += 1
        shaped_rows.append((len(shaped_cells), r[6], rank[r[1]], r[7], r[5], r[1]))
        shaped_cells.extend(r[4])
    for i in range(1, 82):
        shaped_offsets[i] += shaped_offsets[i - 1]
    shapeless = sorted((r for r in recipes if r[0] == "shapeless"), key=lambda r: (len(r[4]), r[1]))
    shapeless_ings, shapeless_rows, shapeless_offsets = [], [], [0] * 10
    for r in shapeless:
        shapeless_offsets[len(r[4])] += 1
        shapeless_rows.append((len(shapeless_ings), r[6], rank[r[1]], r[7], r[1]))
        shapeless_ings.extend(r[4])
    for i in range(1, 10):
        shapeless_offsets[i] += shapeless_offsets[i - 1]
    ing_offsets, ing_items = [0], []
    for s in ing_sets:
        ing_items.extend(s)
        ing_offsets.append(len(ing_items))
    if max(len(shaped_cells), len(shapeless_ings), len(ing_items)) > 0xFFFF:
        raise SystemExit("recipe tables too large for u16 offsets")

    # --- Emit ---------------------------------------------------------------
    out = []
    out.append("// @generated by scripts/gen_vanilla_items.py from Mojang data generator output.\n")
    out.append("// Do not edit manually.\n")
    out.append("#![allow(dead_code)]\n\n")
    out.append(f"pub const ITEM_COUNT: usize = {n_items};\n")
    out.append(f"pub const STATE_COUNT: usize = {n_states};\n")
    out.append(f"pub const RECIPE_COUNT: usize = {len(recipes)};\n\n")

    out.append("/// Item protocol ids.\npub mod item {\n")
    for name in sorted(item_ids):
        out.append(f"    pub const {const_name(name)}: i32 = {item_ids[name]};\n")
    out.append("}\n\n")

    out.append("/// Max stack size per item id (`minecraft:max_stack_size`).\n")
    out.append(rust_array("MAX_STACK", "u8", max_stack, 32))
    out.append("/// Default placement state per item id; 0 when the item places nothing.\n")
    out.append(rust_array("PLACE_STATE", "u16", place))
    out.append("/// Per block, in state order: first state id, item, default state.\n")
    out.append(rust_array("BLOCK_FIRST_STATE", "u16", [r[0] for r in block_rows]))
    out.append(rust_array("BLOCK_ITEM", "u16", [r[1] for r in block_rows]))
    out.append(rust_array("BLOCK_DEFAULT_STATE", "u16", [r[2] for r in block_rows]))

    out.append(
        "/// A tool-like item: `class` is its harvest class bit (0xFF: none) and\n"
        "/// `preds` the loot `match_tool` predicates it satisfies.\n"
        "#[derive(Clone, Copy)]\npub struct ToolItem {\n    pub item: u16,\n    pub class: u8,\n    pub preds: u8,\n}\n\n"
    )
    out.append("/// Sorted by item id.\n")
    out.append(rust_structs("TOOL_ITEMS", "ToolItem", [f"ToolItem {{ item: {i}, class: {c}, preds: {p} }}" for i, c, p in tool_rows]))
    out.append("/// Harvest masks referenced by `DropSpec::req`; index 0 means no requirement.\n")
    out.append(rust_array("REQ_MASKS", "u16", mask_list))
    out.append("// Loot `match_tool` predicates, by bit: " + ", ".join(pred_keys) + "\n")
    for i, key in enumerate(pred_keys):
        out.append(f"pub const PRED_{const_name(json.loads(key).lstrip('#'))}: u8 = 1 << {i};\n")
    out.append("\n")

    out.append(
        "/// Count of a drop: uniform in `min..=max`, plus `bin_n` trials succeeding\n"
        "/// below `bin_p` (of 2^32), clamped to `floor..=ceil`.\n"
        "#[derive(Clone, Copy)]\npub struct Count {\n    pub bin_p: u32,\n    pub min: i8,\n    pub max: i8,\n"
        "    pub floor: i8,\n    pub ceil: i8,\n    pub bin_n: u8,\n}\n\n"
    )
    out.append(rust_structs("COUNTS", "Count", [
        f"Count {{ bin_p: {p}, min: {lo}, max: {hi}, floor: {fl}, ceil: {ce}, bin_n: {n} }}"
        for lo, hi, n, p, fl, ce in count_rows
    ]))
    out.append(
        "/// One alternative of a loot pool. `chance` is out of 2^32 (0: always);\n"
        "/// `tool` is 0 (any), 1 + 2k (held item satisfies predicate k) or\n"
        "/// 2 + 2k (it does not); `last` closes the pool.\n"
        "#[derive(Clone, Copy)]\npub struct Candidate {\n    pub chance: u32,\n    pub item: u16,\n"
        "    pub count: u8,\n    pub tool: u8,\n    pub last: bool,\n}\n\n"
    )
    out.append(rust_structs("CANDIDATES", "Candidate", [
        f"Candidate {{ chance: {ch}, item: {it}, count: {co}, tool: {tc}, last: {str(la).lower()} }}"
        for ch, it, co, tc, la in cand_rows
    ]))
    out.append(
        "/// Drops of a run of states: `len` candidates from `start`; `req` indexes\n"
        "/// `REQ_MASKS`.\n"
        "#[derive(Clone, Copy)]\npub struct DropSpec {\n    pub start: u16,\n    pub len: u8,\n    pub req: u8,\n}\n\n"
    )
    out.append(rust_structs("DROP_SPECS", "DropSpec", [f"DropSpec {{ start: {s}, len: {n}, req: {r} }}" for s, n, r in spec_rows]))
    out.append("/// Runs of block states sharing a drop spec: first state of each run.\n")
    out.append(rust_array("DROP_RUN_START", "u16", drop_starts))
    out.append(rust_array("DROP_RUN_SPEC", "u16", drop_specs))

    out.append("/// Ingredient sets: items of set i are `ING_ITEMS[ING_OFFSETS[i]..ING_OFFSETS[i + 1]]`, sorted.\n")
    out.append(rust_array("ING_OFFSETS", "u16", ing_offsets))
    out.append(rust_array("ING_ITEMS", "u16", ing_items))
    out.append(f"/// Marks an empty cell in `SHAPED_CELLS`.\npub const EMPTY_CELL: u16 = {EMPTY};\n\n")
    out.append(
        "/// A shaped recipe; its `w * h` cells start at `cells`, row-major.\n"
        "#[derive(Clone, Copy)]\npub struct Shaped {\n    pub cells: u16,\n    pub result: u16,\n    pub rank: u16,\n"
        "    pub count: u8,\n    pub symmetric: bool,\n}\n\n"
    )
    out.append(
        "/// Shaped recipes bucketed by `((w - 1) * 3 + (h - 1)) * 9 + (cells - 1)`;\n"
        "/// bucket b is `SHAPED[SHAPED_BUCKETS[b]..SHAPED_BUCKETS[b + 1]]`.\n"
    )
    out.append(rust_array("SHAPED_BUCKETS", "u16", shaped_offsets))
    out.append(rust_structs("SHAPED", "Shaped", [
        f"Shaped {{ cells: {c}, result: {res}, rank: {rk}, count: {n}, symmetric: {str(sym).lower()} }}, // {rid}"
        for c, res, rk, n, sym, rid in shaped_rows
    ]))
    out.append(rust_array("SHAPED_CELLS", "u16", shaped_cells))
    out.append(
        "/// A shapeless recipe; its ingredient sets start at `ings` (count from the bucket).\n"
        "#[derive(Clone, Copy)]\npub struct Shapeless {\n    pub ings: u16,\n    pub result: u16,\n    pub rank: u16,\n    pub count: u8,\n}\n\n"
    )
    out.append("/// Shapeless recipes bucketed by ingredient count n: `SHAPELESS_BUCKETS[n - 1]..SHAPELESS_BUCKETS[n]`.\n")
    out.append(rust_array("SHAPELESS_BUCKETS", "u16", shapeless_offsets))
    out.append(rust_structs("SHAPELESS", "Shapeless", [
        f"Shapeless {{ ings: {i}, result: {res}, rank: {rk}, count: {n} }}, // {rid}"
        for i, res, rk, n, rid in shapeless_rows
    ]))
    out.append(rust_array("SHAPELESS_INGS", "u16", shapeless_ings))

    text = "".join(out).replace(", }", " }")
    OUT.write_text(text, encoding="utf-8", newline="\n")
    print(
        f"wrote {OUT.relative_to(ROOT)}: {len(recipes)} recipes ({len(shaped)} shaped, "
        f"{len(shapeless)} shapeless, {skipped} skipped), {len(ing_sets)} ingredient sets, "
        f"{len(drop_starts)} drop runs, {len(spec_rows)} drop specs, {len(cand_rows)} candidates, "
        f"{len(class_of_set)} tool classes, {len(required)} tool-gated blocks"
    )


if __name__ == "__main__":
    main()
