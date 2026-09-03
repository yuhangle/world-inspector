//! Core library for reading Bedrock world data.
//! Provides structured access to player inventories from LevelDB save files.

pub mod ffi;

use std::path::Path;
use std::rc::Rc;

use bedrock_nbt::{CompoundTag, Tag};
use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib, CompressionLevel};
use miniz_oxide::inflate::{decompress_to_vec, decompress_to_vec_zlib};
use rusty_leveldb::compressor::NoneCompressor;
use rusty_leveldb::{Compressor, CompressorList, LdbIterator, Options, DB};
use serde::Serialize;

// ── Compressors ──

struct ZlibCompressor(u8);
impl ZlibCompressor {
    fn new(level: u8) -> Self { assert!(level <= 10); Self(level) }
}
impl Compressor for ZlibCompressor {
    fn encode(&self, block: Vec<u8>) -> rusty_leveldb::Result<Vec<u8>> {
        Ok(compress_to_vec_zlib(&block, self.0))
    }
    fn decode(&self, block: Vec<u8>) -> rusty_leveldb::Result<Vec<u8>> {
        decompress_to_vec_zlib(&block).map_err(|e| rusty_leveldb::Status {
            code: rusty_leveldb::StatusCode::CompressionError, err: e.to_string(),
        })
    }
}

struct RawZlibCompressor(u8);
impl RawZlibCompressor {
    fn new(level: u8) -> Self { assert!(level <= 10); Self(level) }
}
impl Compressor for RawZlibCompressor {
    fn encode(&self, block: Vec<u8>) -> rusty_leveldb::Result<Vec<u8>> {
        Ok(compress_to_vec(&block, self.0))
    }
    fn decode(&self, block: Vec<u8>) -> rusty_leveldb::Result<Vec<u8>> {
        decompress_to_vec(&block).map_err(|e| rusty_leveldb::Status {
            code: rusty_leveldb::StatusCode::CompressionError, err: e.to_string(),
        })
    }
}

fn mcpe_options(compression_level: u8) -> Options {
    let mut opt = Options::default();
    let mut list = CompressorList::new();
    list.set_with_id(0, NoneCompressor {});
    list.set_with_id(2, ZlibCompressor::new(compression_level));
    list.set_with_id(4, RawZlibCompressor::new(compression_level));
    opt.compressor_list = Rc::new(list);
    opt.compressor = 4;
    opt.reuse_logs = false;
    opt.reuse_manifest = false;
    opt
}

// ── Data structures ──

/// A single item with binary NBT (pre-encoded for inventoryui).
pub struct EncodedItem {
    pub slot: i32,
    pub name: String,
    pub count: i32,
    pub damage: i32,
    pub nbt_bytes: Vec<u8>,
}

/// A single item in an inventory.
#[derive(Clone, Serialize)]
pub struct Item {
    pub slot: i32,
    pub name: String,
    pub count: i32,
    pub damage: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<serde_json::Value>,
}

/// A player's full inventory (main + armor + offhand).
#[derive(Clone, Serialize)]
pub struct PlayerInventory {
    pub player_key: String,
    pub inventory: Vec<Item>,
    pub armor: Vec<Item>,
    pub offhand: Option<Item>,
}

// ── World handle ──

/// An opened Bedrock world database.
pub struct WorldHandle {
    db: DB,
    _world_path: String,
}

impl WorldHandle {
    /// Open a Bedrock world by its world path (the directory containing level.dat).
    pub fn open(world_path: &str) -> Result<Self, String> {
        let db_path = resolve_db_path(world_path, 0);
        if !Path::new(&db_path).is_dir() || !Path::new(&format!("{}/CURRENT", db_path)).exists() {
            return Err("LevelDB directory not found".to_string());
        }

        let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
        opt.read_only = true;

        let db = DB::open(&db_path, opt).map_err(|e| format!("Failed to open DB: {}", e))?;

        Ok(WorldHandle { db, _world_path: world_path.to_string() })
    }

    /// Get a player's inventory by their player key.
    /// The key can be "player_<UUID>", "player_server_<UUID>", or just "<UUID>".
    pub fn get_player_inventory(&mut self, player_key: &str) -> Result<PlayerInventory, String> {
        let players = scan_all_player_keys(&mut self.db);

        let needle = player_key.as_bytes();
        let partial_matches: Vec<_> = players.iter().filter(|(k, _)| {
            String::from_utf8_lossy(k).contains(player_key)
        }).collect();

        let found = players.iter().find(|(k, _)| k.as_slice() == needle)
            .or_else(|| {
                if partial_matches.len() == 1 { Some(partial_matches[0]) } else { None }
            });

        let (key, value) = found.ok_or_else(|| "Player not found".to_string())?;
        let key_label = String::from_utf8_lossy(key).to_string();
        let tag = CompoundTag::from_binary_nbt(value, true)
            .map(|(t, _)| t)
            .map_err(|_| "Failed to parse NBT".to_string())?;

        // Follow ServerId link for player_server_ data
        let (data_key, data_tag) = if key.starts_with(b"player_") && !key.starts_with(b"player_server_") {
            if let Some(server_id) = tag.get("ServerId").and_then(|t| t.as_str()) {
                let server_key = server_id.as_bytes();
                if let Some(sv) = players.iter().find(|(k, _)| k.as_slice() == server_key) {
                    let data_tag = CompoundTag::from_binary_nbt(&sv.1, true)
                        .map(|(t, _)| t)
                        .unwrap_or_else(|_| tag.clone());
                    (String::from_utf8_lossy(&sv.0).to_string(), data_tag)
                } else {
                    (key_label, tag)
                }
            } else {
                (key_label, tag)
            }
        } else {
            (key_label, tag)
        };

        Ok(build_player_inventory(&data_key, &data_tag))
    }

    /// Get a player's inventory items with pre-serialized binary NBT.
    pub fn get_player_encoded_items(&mut self, player_key: &str) -> Result<Vec<EncodedItem>, String> {
        let players = scan_all_player_keys(&mut self.db);

        let needle = player_key.as_bytes();
        let found = players.iter().find(|(k, _)| k.as_slice() == needle)
            .or_else(|| players.iter().find(|(k, _)| {
                String::from_utf8_lossy(k).contains(player_key)
            }));

        let (key, value) = found.ok_or_else(|| "Player not found".to_string())?;
        let tag = CompoundTag::from_binary_nbt(value, true)
            .map(|(t, _)| t)
            .map_err(|_| "Failed to parse NBT".to_string())?;

        // Follow ServerId link
        let data_tag = if key.starts_with(b"player_") && !key.starts_with(b"player_server_") {
            if let Some(server_id) = tag.get("ServerId").and_then(|t| t.as_str()) {
                let server_key = server_id.as_bytes();
                if let Some(sv) = players.iter().find(|(k, _)| k.as_slice() == server_key) {
                    CompoundTag::from_binary_nbt(&sv.1, true)
                        .map(|(t, _)| t)
                        .unwrap_or_else(|_| tag.clone())
                } else { tag }
            } else { tag }
        } else { tag };

        let items = collect_player_encoded_items(&data_tag);
        Ok(items)
    }

    /// List all player keys found in the world database.
    pub fn list_player_keys(&mut self) -> Vec<String> {
        scan_all_player_keys(&mut self.db)
            .into_iter()
            .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
            .collect()
    }
}

// ── Internal helpers ──

/// Bedrock 版所有维度共享同一个 db 文件夹。
fn resolve_db_path(world_path: &str, _dim_id: u8) -> String {
    format!("{}/db", world_path.trim_end_matches('/'))
}

fn tag_to_i32(tag: &Tag) -> Option<i32> {
    match tag { Tag::Byte(v) => Some(*v as i32), Tag::Short(v) => Some(*v as i32), Tag::Int(v) => Some(*v), Tag::Long(v) => Some(*v as i32), _ => None }
}

// 前缀扫描统一用 seek_to_first + 分类（fork 的 leveldb seek 对部分前缀有索引定位缺陷）。

fn scan_player_keys(db: &mut DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    iter.seek_to_first();
    while let Some((key, value)) = iter.next() {
        if key.starts_with(b"~local_player") || key.starts_with(b"~player_")
            || (key.starts_with(b"player_") && !key.starts_with(b"player_server_")) {
            results.push((key, value));
        }
    }
    results
}

fn scan_all_player_keys(db: &mut DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = scan_player_keys(db);
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    iter.seek_to_first();
    while let Some((key, value)) = iter.next() {
        if key.starts_with(b"player_server_") {
            results.push((key, value));
        }
    }
    results
}

fn tag_to_json(tag: &Tag) -> serde_json::Value {
    match tag {
        Tag::Compound(map) => {
            let ct = CompoundTag::from_map(map.clone());
            let mut obj = serde_json::Map::new();
            for (k, v) in ct.iter_sorted() {
                obj.insert(k.to_string(), tag_to_json(&v));
            }
            serde_json::Value::Object(obj)
        }
        Tag::List(lv) => {
            let arr: Vec<serde_json::Value> = lv.elements.iter().map(tag_to_json).collect();
            serde_json::Value::Array(arr)
        }
        Tag::String(s) => serde_json::Value::String(s.clone()),
        Tag::Byte(b) => serde_json::Value::Number(serde_json::Number::from(*b)),
        Tag::Short(s) => serde_json::Value::Number(serde_json::Number::from(*s)),
        Tag::Int(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
        Tag::Long(l) => serde_json::Value::Number(serde_json::Number::from(*l)),
        Tag::Float(f) => {
            if f.is_finite() {
                serde_json::Value::Number(serde_json::Number::from_f64(*f as f64).unwrap_or(serde_json::Number::from(0)))
            } else {
                serde_json::Value::String(f.to_string())
            }
        }
        Tag::Double(d) => {
            if d.is_finite() {
                serde_json::Value::Number(serde_json::Number::from_f64(*d).unwrap_or(serde_json::Number::from(0)))
            } else {
                serde_json::Value::String(d.to_string())
            }
        }
        Tag::ByteArray(arr) => serde_json::Value::Array(arr.iter().map(|b| serde_json::Value::Number(serde_json::Number::from(*b))).collect()),
        Tag::IntArray(arr) => serde_json::Value::Array(arr.iter().map(|i| serde_json::Value::Number(serde_json::Number::from(*i))).collect()),
        Tag::LongArray(arr) => serde_json::Value::Array(arr.iter().map(|l| serde_json::Value::Number(serde_json::Number::from(*l))).collect()),
        Tag::End => serde_json::Value::Null,
    }
}

fn inventory_item_to_json(elem: &Tag) -> Item {
    let ct = if let Tag::Compound(map) = elem { CompoundTag::from_map(map.clone()) } else {
        return Item { slot: 0, name: "minecraft:air".into(), count: 0, damage: 0, tag: None };
    };
    let name = ct.get("Name").and_then(|t| t.as_str()).unwrap_or("minecraft:air");
    let name = if name.is_empty() { "minecraft:air" } else { name };
    let count = ct.get("Count").and_then(tag_to_i32).unwrap_or(0);
    let slot = ct.get("Slot").and_then(tag_to_i32).unwrap_or(0);
    let damage = ct.get("Damage").and_then(tag_to_i32).unwrap_or(0);
    let tag = ct.get("tag").map(tag_to_json);
    Item { slot, name: name.to_string(), count, damage, tag }
}

/// Extract an item's data with its `tag` sub-compound serialized to binary NBT.
fn inventory_item_to_encoded(elem: &Tag) -> (i32, String, i32, i32, Vec<u8>) {
    let ct = if let Tag::Compound(map) = elem { CompoundTag::from_map(map.clone()) } else {
        return (0, "minecraft:air".into(), 0, 0, Vec::new());
    };
    let name = ct.get("Name").and_then(|t| t.as_str()).unwrap_or("minecraft:air");
    let name = if name.is_empty() { "minecraft:air" } else { name };
    let count = ct.get("Count").and_then(tag_to_i32).unwrap_or(0);
    let slot = ct.get("Slot").and_then(tag_to_i32).unwrap_or(0);
    let damage = ct.get("Damage").and_then(tag_to_i32).unwrap_or(0);
    let nbt_bytes = ct.get("tag")
        .and_then(|t| t.as_compound())
        .map(|m| CompoundTag::from_map(m.clone()).to_binary_nbt(true, false))
        .unwrap_or_default();
    (slot, name.to_string(), count, damage, nbt_bytes)
}

/// Collect all player items (inventory + armor + offhand) as a flat list of EncodedItem.
pub fn collect_player_encoded_items(tag: &CompoundTag) -> Vec<EncodedItem> {
    let mut items = Vec::new();

    if let Some(Tag::List(inv_list)) = tag.get("Inventory") {
        for elem in &inv_list.elements {
            let (slot, name, count, damage, nbt) = inventory_item_to_encoded(elem);
            if count <= 0 || name == "minecraft:air" { continue; }
            if slot >= 0 && slot <= 35 {
                items.push(EncodedItem { slot, name, count, damage, nbt_bytes: nbt });
            }
        }
    }

    if let Some(Tag::List(armor_list)) = tag.get("Armor") {
        let slot_mapping = [100, 101, 102, 103];
        for (i, elem) in armor_list.elements.iter().enumerate() {
            if i >= 4 { break; }
            let (_, name, count, damage, nbt) = inventory_item_to_encoded(elem);
            if name.is_empty() || name == "minecraft:air" || count <= 0 { continue; }
            items.push(EncodedItem { slot: slot_mapping[i], name, count, damage, nbt_bytes: nbt });
        }
    }

    if let Some(Tag::List(offhand_list)) = tag.get("Offhand") {
        if let Some(first) = offhand_list.elements.first() {
            let (_, name, count, damage, nbt) = inventory_item_to_encoded(first);
            if !name.is_empty() && name != "minecraft:air" && count > 0 {
                items.push(EncodedItem { slot: 104, name, count, damage, nbt_bytes: nbt });
            }
        }
    }

    items
}

fn build_player_inventory(player_key: &str, tag: &CompoundTag) -> PlayerInventory {
    let mut inventory = Vec::new();
    let mut armor = Vec::new();
    let mut offhand = None;

    if let Some(Tag::List(inv)) = tag.get("Inventory") {
        for elem in &inv.elements {
            let item = inventory_item_to_json(elem);
            let slot = item.slot;
            if item.count <= 0 || item.name == "minecraft:air" { continue; }
            if slot >= 0 && slot <= 35 {
                inventory.push(item);
            }
        }
    }

    if let Some(Tag::List(armor_list)) = tag.get("Armor") {
        let armor_slot_mapping = [100, 101, 102, 103];
        for (i, elem) in armor_list.elements.iter().enumerate() {
            if i >= 4 { break; }
            if let Tag::Compound(map) = elem {
                let ct = CompoundTag::from_map(map.clone());
                let name = ct.get("Name").and_then(|t| t.as_str()).unwrap_or("minecraft:air");
                let count = ct.get("Count").and_then(tag_to_i32).unwrap_or(0);
                if name.is_empty() || name == "minecraft:air" || count <= 0 { continue; }
                let mut item = inventory_item_to_json(elem);
                item.slot = armor_slot_mapping[i];
                armor.push(item);
            }
        }
    }

    if let Some(Tag::List(offhand_list)) = tag.get("Offhand") {
        if let Some(first) = offhand_list.elements.first() {
            if let Tag::Compound(map) = first {
                let ct = CompoundTag::from_map(map.clone());
                let name = ct.get("Name").and_then(|t| t.as_str()).unwrap_or("minecraft:air");
                let count = ct.get("Count").and_then(tag_to_i32).unwrap_or(0);
                if !name.is_empty() && name != "minecraft:air" && count > 0 {
                    let mut item = inventory_item_to_json(first);
                    item.slot = 104;
                    offhand = Some(item);
                }
            }
        }
    }

    PlayerInventory { player_key: player_key.to_string(), inventory, armor, offhand }
}
