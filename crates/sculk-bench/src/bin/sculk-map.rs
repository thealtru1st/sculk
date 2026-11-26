//! Renders a top-down map of a Sculk world to PNG.
//!
//! usage: sculk-map [--seed S] [--chunks N] [--scale BLOCKS_PER_PIXEL] [--top Y] [--out FILE]

use flate2::write::ZlibEncoder;
use flate2::Compression;
use sculk::vanilla::block as b;
use sculk::world::{World, MIN_Y};
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let mut seed = 0u64;
    let mut chunks = 32i32;
    let mut top = 200i32;
    let mut scale = 1i32;
    let mut out = String::from("docs/images/terrain-map.png");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--seed" => seed = value()?.parse()?,
            "--chunks" => chunks = value()?.parse()?,
            "--top" => top = value()?.parse()?,
            "--scale" => scale = value()?.parse::<i32>()?.max(1),
            "--out" => out = value()?,
            _ => anyhow::bail!(
                "usage: sculk-map [--seed S] [--chunks N] [--scale B] [--top Y] [--out FILE]"
            ),
        }
    }
    let world = World::new(seed);
    let size = (chunks * 16 / scale) as usize;
    let origin = -(chunks * 8);
    let mut heights = vec![MIN_Y; size * size];
    let mut colors = vec![[0u8; 3]; size * size];
    for row in 0..size {
        let z = origin + row as i32 * scale;
        for col in 0..size {
            let x = origin + col as i32 * scale;
            let mut y = top;
            let mut water_depth = 0;
            let mut color = None;
            while y > MIN_Y {
                let state = world.block(x, y, z);
                if state == b::WATER {
                    water_depth += 1;
                } else if state != b::AIR {
                    color = Some(if water_depth > 0 {
                        WATER
                    } else {
                        color_of(state)
                    });
                    break;
                }
                y -= 1;
            }
            let idx = row * size + col;
            heights[idx] = y + water_depth;
            let mut rgb = color.unwrap_or([0, 0, 0]);
            if water_depth > 0 {
                let shade = 1.0 - (water_depth.min(12) as f32 / 24.0);
                rgb = rgb.map(|c| (c as f32 * shade) as u8);
            }
            colors[idx] = rgb;
        }
    }
    // Relief shading from the northern neighbour.
    let mut pixels = Vec::with_capacity(size * (size * 3 + 1));
    for row in 0..size {
        pixels.push(0); // PNG filter: none
        for col in 0..size {
            let idx = row * size + col;
            let north = if row > 0 {
                heights[idx - size]
            } else {
                heights[idx]
            };
            let factor = match heights[idx].cmp(&north) {
                std::cmp::Ordering::Greater => 1.12,
                std::cmp::Ordering::Less => 0.82,
                std::cmp::Ordering::Equal => 1.0,
            };
            for c in colors[idx] {
                pixels.push((c as f32 * factor).min(255.0) as u8);
            }
        }
    }
    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, png(size as u32, size as u32, &pixels)?)?;
    println!("wrote {out} ({size}x{size}, seed {seed})");
    Ok(())
}

const WATER: [u8; 3] = [64, 64, 255];

/// Approximate map colours for the blocks the generator places on the surface.
fn color_of(state: i32) -> [u8; 3] {
    match state {
        b::GRASS_BLOCK | b::SHORT_GRASS | b::FERN => [127, 178, 56],
        b::GRASS_BLOCK_SNOWY | b::SNOW | b::SNOW_BLOCK => [255, 255, 255],
        b::ICE | b::PACKED_ICE => [160, 160, 255],
        b::SAND | b::SANDSTONE => [247, 233, 163],
        b::GRAVEL => [150, 140, 135],
        b::DIRT | b::COARSE_DIRT | b::PODZOL => [151, 109, 77],
        b::STONE | b::ANDESITE | b::COBBLESTONE => [112, 112, 112],
        b::OAK_LEAVES | b::OAK_LEAVES_PERSISTENT => [0, 124, 0],
        b::BIRCH_LEAVES | b::BIRCH_LEAVES_PERSISTENT => [80, 140, 40],
        b::SPRUCE_LEAVES | b::SPRUCE_LEAVES_PERSISTENT => [40, 90, 50],
        b::OAK_LOG | b::BIRCH_LOG | b::SPRUCE_LOG => [102, 76, 51],
        b::CACTUS => [0, 124, 0],
        b::DANDELION => [229, 229, 51],
        b::POPPY => [153, 51, 51],
        b::CORNFLOWER | b::AZURE_BLUET | b::OXEYE_DAISY => [200, 200, 230],
        b::CLAY => [164, 168, 184],
        b::BEDROCK => [60, 60, 60],
        _ => [130, 130, 130],
    }
}

fn png(width: u32, height: u32, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut out, b"IHDR", &ihdr);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(raw)?;
    chunk(&mut out, b"IDAT", &encoder.finish()?);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = !0u32;
    for &byte in kind.iter().chain(data) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}
