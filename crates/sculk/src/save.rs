//! Compact binary world save: seed, block edits, and player inventories.
//!
//! Layout (little-endian): magic `SCULKSV1`, u64 seed, u32 edit count, then
//! edits as (i32 x, i32 y, i32 z, i32 state), u32 inventory count, then for
//! each inventory a u8 name length, the name bytes, and 46 × (i16 count,
//! i32 item). Writes go to a temporary file that is renamed into place.

use crate::inventory::{Inventory, INVENTORY_SIZE};
use crate::Result;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

const MAGIC: &[u8; 8] = b"SCULKSV1";

pub struct SaveData {
    pub seed: u64,
    pub edits: Vec<(i32, i32, i32, i32)>,
    pub inventories: Vec<(String, Inventory)>,
}

pub fn load(path: &Path) -> Result<Option<SaveData>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    decode(&bytes)
        .map(Some)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "corrupt world save").into())
}

pub fn store(path: &Path, data: &SaveData) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, encode(data))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn encode(data: &SaveData) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + data.edits.len() * 16 + data.inventories.len() * 300);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&data.seed.to_le_bytes());
    out.extend_from_slice(&(data.edits.len() as u32).to_le_bytes());
    for &(x, y, z, state) in &data.edits {
        for v in [x, y, z, state] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&(data.inventories.len() as u32).to_le_bytes());
    for (name, inventory) in &data.inventories {
        let name = &name.as_bytes()[..name.len().min(255)];
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        for slot in inventory.slots {
            out.extend_from_slice(&slot.count.to_le_bytes());
            out.extend_from_slice(&slot.item_id.to_le_bytes());
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<SaveData> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(8)? != MAGIC {
        return None;
    }
    let seed = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
    let edit_count = r.u32()? as usize;
    let mut edits = Vec::with_capacity(edit_count.min(bytes.len() / 16));
    for _ in 0..edit_count {
        edits.push((r.i32()?, r.i32()?, r.i32()?, r.i32()?));
    }
    let inventory_count = r.u32()? as usize;
    let mut inventories = Vec::with_capacity(inventory_count.min(bytes.len() / 64));
    for _ in 0..inventory_count {
        let len = r.take(1)?[0] as usize;
        let name = std::str::from_utf8(r.take(len)?).ok()?.to_owned();
        let mut inventory = Inventory::default();
        for slot in 0..INVENTORY_SIZE {
            let count = i16::from_le_bytes(r.take(2)?.try_into().ok()?);
            let item = r.i32()?;
            inventory.set(slot, count, item);
        }
        inventories.push((name, inventory));
    }
    (r.pos == bytes.len()).then_some(SaveData {
        seed,
        edits,
        inventories,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Some(out)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_reject_corruption() {
        let mut inventory = Inventory::default();
        inventory.set(36, 12, 35);
        let data = SaveData {
            seed: 42,
            edits: vec![(1, -60, -7, 9), (-100, 70, 3, 0)],
            inventories: vec![("Steve".into(), inventory)],
        };
        let bytes = encode(&data);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.seed, 42);
        assert_eq!(back.edits, data.edits);
        assert_eq!(back.inventories[0].0, "Steve");
        assert_eq!(back.inventories[0].1.get(36).count, 12);
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_none());
        }
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(decode(&bad).is_none());
    }
}
