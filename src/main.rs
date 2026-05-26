use std::path::Path;
use world_inspector::WorldHandle;
use std::rc::Rc;

use bedrock_nbt::{CompoundTag, Tag};
use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib, CompressionLevel};
use miniz_oxide::inflate::{decompress_to_vec, decompress_to_vec_zlib};
use rusty_leveldb::compressor::NoneCompressor;
use rusty_leveldb::{Compressor, CompressorList, LdbIterator, Options, DB};

// ── Compressors (kept for CLI-specific DB operations) ──

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
    opt
}

// ── CLI ──

enum Command {
    QueryBlock { world_path: String, x: i32, y: i32, z: i32, dim_id: u8, dim_name: String },
    ShowInfo { world_path: String },
    ListPlayers { world_path: String },
    ListActors { world_path: String },
    ShowPlayer { world_path: String, player_key: String, dump: bool, json: bool },
}

fn print_help() {
    eprintln!(
"Usage: world-inspector <world_path> <x> <y> <z> [dimension]
       world-inspector <world_path>
       world-inspector <world_path> --players
       world-inspector <world_path> --actors
       world-inspector <world_path> --player <key>

查询坐标方块：
  world-inspector /path/to/world -3 -60 -3          查主世界方块

查看存档概要：
  world-inspector /path/to/world                    世界信息、DB 统计、玩家数量

列出玩家：
  world-inspector /path/to/world --players          显示所有玩家

列出实体(含玩家)：
  world-inspector /path/to/world --actors           显示所有实体

查看玩家完整数据：
  world-inspector /path/to/world --player '~local_player'
  world-inspector /path/to/world --player player_<UUID>
  world-inspector /path/to/world --player <key> --dump  转储完整 NBT 原始数据
  world-inspector /path/to/world --player <key> --json  以 JSON 格式输出背包数据
");
}

fn parse_args() -> Result<Command, String> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 || (args.len() == 2 && args[1] == "--help") {
        print_help();
        std::process::exit(if args.len() == 2 { 0 } else { 1 });
    }

    let world_path = args[1].clone();

    if args.len() >= 3 && (args[2] == "--players" || args[2] == "-l") {
        return Ok(Command::ListPlayers { world_path });
    }

    if args.len() >= 3 && (args[2] == "--actors" || args[2] == "-a") {
        return Ok(Command::ListActors { world_path });
    }

    if args.len() >= 4 && (args[2] == "--player" || args[2] == "-p") {
        let remaining: Vec<&String> = args[4..].iter().collect();
        let dump = remaining.contains(&&"--dump".to_string()) || remaining.contains(&&"-d".to_string());
        let json = remaining.contains(&&"--json".to_string()) || remaining.contains(&&"-j".to_string());
        return Ok(Command::ShowPlayer { world_path, player_key: args[3].clone(), dump, json });
    }

    if args.len() == 2 {
        return Ok(Command::ShowInfo { world_path });
    }

    if args.len() < 5 {
        return Err("缺少坐标参数。需要 <x> <y> <z>".into());
    }

    let x = args[2].parse::<i32>().map_err(|_| format!("x 格式错误: '{}'", args[2]))?;
    let y = args[3].parse::<i32>().map_err(|_| format!("y 格式错误: '{}'", args[3]))?;
    let z = args[4].parse::<i32>().map_err(|_| format!("z 格式错误: '{}'", args[4]))?;

    let (dim_id, dim_name) = if let Some(raw) = args.get(5) {
        match raw.to_lowercase().as_str() {
            "overworld" | "0" => (0u8, "overworld".into()),
            "nether" | "1"    => (1u8, "nether".into()),
            "end" | "2"       => (2u8, "end".into()),
            _ => return Err(format!("未知维度 '{}'，可选: overworld(0) nether(1) end(2)", raw)),
        }
    } else {
        (0u8, "overworld".into())
    };

    Ok(Command::QueryBlock { world_path, x, y, z, dim_id, dim_name })
}

// ── Coordinate helpers ──

fn floor_div(a: i32, b: i32) -> i32 { a.div_euclid(b) }

fn subchunk_key(cx: i32, cz: i32, cy: i8) -> Vec<u8> {
    let mut key = Vec::with_capacity(10);
    key.extend_from_slice(&cx.to_le_bytes());
    key.extend_from_slice(&cz.to_le_bytes());
    key.push(0x2f);
    key.push(cy as u8);
    key
}

fn block_entity_key(cx: i32, cz: i32) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.extend_from_slice(&cx.to_le_bytes());
    key.extend_from_slice(&cz.to_le_bytes());
    key.push(0x31);
    key
}

fn resolve_db_path(world_path: &str, dim_id: u8) -> Vec<String> {
    let base = world_path.trim_end_matches('/');
    match dim_id {
        0 => vec![format!("{}/db", base)],
        1 => vec![format!("{}/DIM-1/db", base), format!("{}/db", base)],
        2 => vec![format!("{}/DIM1/db", base), format!("{}/db", base)],
        _ => vec![format!("{}/db", base)],
    }
}

// ── Sub-chunk ──

struct BlockInfo {
    name: String,
    states: CompoundTag,
    palette: Vec<(String, CompoundTag)>,
}

fn parse_block_storage(data: &[u8], lx: usize, ly: usize, lz: usize) -> Option<BlockInfo> {
    let mut pos = 0;
    let layer_header = *data.get(pos)?; pos += 1;
    let bpb = (layer_header >> 1) as usize;
    if bpb == 0 || bpb > 16 { return None; }

    let indices_per_word = 32 / bpb;
    let word_count = (4096 + indices_per_word - 1) / indices_per_word;
    let mut words: Vec<u32> = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        if pos + 4 > data.len() { return None; }
        let mut word = [0u8; 4];
        word.copy_from_slice(&data[pos..pos + 4]); words.push(u32::from_le_bytes(word)); pos += 4;
    }
    if pos + 4 > data.len() { return None; }
    let mut count_bytes = [0u8; 4];
    count_bytes.copy_from_slice(&data[pos..pos + 4]); pos += 4;
    let palette_count = u32::from_le_bytes(count_bytes) as usize;

    let mut palette: Vec<(String, CompoundTag)> = Vec::with_capacity(palette_count);
    for _ in 0..palette_count {
        match CompoundTag::from_binary_nbt(&data[pos..], true) {
            Ok((tag, consumed)) => {
                if consumed == 0 { break; }
                let name = tag.get("name").and_then(|t| t.as_str()).unwrap_or("unknown").to_string();
                let states = tag.get("states")
                    .and_then(|t| if let Tag::Compound(map) = t { Some(CompoundTag::from_map(map.clone())) } else { None })
                    .unwrap_or_default();
                palette.push((name, states));
                pos += consumed;
            }
            Err(_) => break,
        }
    }

    let block_index = ly + lz * 16 + lx * 256;
    let palette_index = if palette.is_empty() { 0 } else {
        let word_idx = block_index / indices_per_word;
        let inword_idx = block_index % indices_per_word;
        let bit_off = inword_idx * bpb;
        if word_idx >= words.len() { 0 } else {
            ((words[word_idx] >> bit_off) & ((1u32 << bpb) - 1)) as usize
        }
    };
    let (name, states) = palette.get(palette_index).cloned()
        .unwrap_or_else(|| (format!("<<idx_{}>>", palette_index), CompoundTag::new()));
    Some(BlockInfo { name, states, palette })
}

fn get_block_from_subchunk(data: &[u8], lx: usize, ly: usize, lz: usize) -> Option<BlockInfo> {
    if data.is_empty() { return None; }
    let version = data[0];
    let sc = *data.get(1)? as usize;
    if sc == 0 { return None; }
    let start = if version >= 9 { 3 } else { 2 };
    if start >= data.len() { return None; }
    parse_block_storage(&data[start..], lx, ly, lz)
}

// ── Block entity ──

fn tag_to_i32(tag: &Tag) -> Option<i32> {
    match tag { Tag::Byte(v) => Some(*v as i32), Tag::Short(v) => Some(*v as i32), Tag::Int(v) => Some(*v), Tag::Long(v) => Some(*v as i32), _ => None }
}

fn tag_to_f64(tag: &Tag) -> Option<f64> {
    match tag { Tag::Float(v) => Some(*v as f64), Tag::Double(v) => Some(*v as f64), _ => None }
}

fn tag_to_i64(tag: &Tag) -> Option<i64> {
    match tag { Tag::Byte(v) => Some(*v as i64), Tag::Short(v) => Some(*v as i64), Tag::Int(v) => Some(*v as i64), Tag::Long(v) => Some(*v), _ => None }
}

fn find_block_entity(data: &[u8], tx: i32, ty: i32, tz: i32) -> Option<CompoundTag> {
    let mut off = 0;
    while off < data.len() {
        match CompoundTag::from_binary_nbt(&data[off..], true) {
            Ok((tag, consumed)) => {
                if consumed == 0 { break; }
                if tag.get("x").and_then(|t| t.as_i32()) == Some(tx)
                    && tag.get("y").and_then(|t| t.as_i32()) == Some(ty)
                    && tag.get("z").and_then(|t| t.as_i32()) == Some(tz) { return Some(tag); }
                off += consumed;
            }
            Err(_) => break,
        }
    }
    None
}

// ── NBT pretty-print ──

fn tag_to_lines(tag: &Tag, prefix: &str, indent: usize) -> Vec<String> {
    let pad = "  ".repeat(indent);
    match tag {
        Tag::Compound(map) => {
            let mut lines = vec![format!("{}{}{{", pad, prefix)];
            let ct = CompoundTag::from_map(map.clone());
            let items: Vec<_> = ct.iter_sorted().into_iter().collect();
            for (i, (key, val)) in items.iter().enumerate() {
                let comma = if i < items.len() - 1 { "," } else { "" };
                let sub = tag_to_lines(val, &format!("\"{}\": ", key), indent + 1);
                for (j, sl) in sub.iter().enumerate() {
                    if j == sub.len() - 1 {
                        lines.push(format!("{}{}", sl.strip_suffix(',').unwrap_or(sl), comma));
                    } else { lines.push(sl.clone()); }
                }
            }
            lines.push(format!("{}}}", pad));
            lines
        }
        Tag::List(lv) => {
            let mut lines = vec![format!("{}{}[", pad, prefix)];
            for (i, elem) in lv.elements.iter().enumerate() {
                let comma = if i < lv.elements.len() - 1 { "," } else { "" };
                let sub = tag_to_lines(elem, "", indent + 1);
                for (j, sl) in sub.iter().enumerate() {
                    if j == sub.len() - 1 {
                        lines.push(format!("{}{}", sl.strip_suffix(',').unwrap_or(sl), comma));
                    } else { lines.push(sl.clone()); }
                }
            }
            lines.push(format!("{}]", pad));
            lines
        }
        Tag::String(s) => vec![format!("{}{}\"{}\"", pad, prefix, s)],
        Tag::Byte(b) => vec![format!("{}{}{}b", pad, prefix, b)],
        Tag::Short(s) => vec![format!("{}{}{}s", pad, prefix, s)],
        Tag::Int(i) => vec![format!("{}{}{}", pad, prefix, i)],
        Tag::Long(l) => vec![format!("{}{}{}L", pad, prefix, l)],
        Tag::Float(f) => vec![format!("{}{}{}f", pad, prefix, f)],
        Tag::Double(d) => vec![format!("{}{}{}d", pad, prefix, d)],
        Tag::ByteArray(arr) => vec![format!("{}{}[B; {} bytes]", pad, prefix, arr.len())],
        Tag::IntArray(arr) => vec![format!("{}{}[I; {} ints]", pad, prefix, arr.len())],
        Tag::LongArray(arr) => vec![format!("{}{}[L; {} longs]", pad, prefix, arr.len())],
        Tag::End => vec![],
    }
}

fn format_chest_items(be: &CompoundTag) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(Tag::List(list_val)) = be.get("Items") {
        for elem in &list_val.elements {
            let tag = match elem { Tag::Compound(map) => CompoundTag::from_map(map.clone()), _ => continue };
            let name = tag.get("Name").and_then(|t| t.as_str()).unwrap_or("unknown");
            let count = tag.get("Count").and_then(tag_to_i32).unwrap_or(0);
            let slot = tag.get("Slot").and_then(tag_to_i32);
            let damage = tag.get("Damage").and_then(tag_to_i32).unwrap_or(0);
            let slot_str = slot.map(|s| format!("[Slot {:2}]", s)).unwrap_or_else(|| "[???]   ".into());
            lines.push(format!("  {} {}  x{}  (damage: {})", slot_str, name, count, damage));
            if let Some(item_tag) = tag.get("tag") {
                for nl in &tag_to_lines(item_tag, "", 2) { lines.push(format!("     {}", nl)); }
            }
        }
    }
    if lines.is_empty() { lines.push("  (empty)".into()); }
    lines
}

// ── level.dat ──

fn read_level_dat(path: &str) -> Result<CompoundTag, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读取失败: {}", e))?;
    if raw.len() < 8 { return Err("文件长度不足".into()); }
    let nbt_size = i32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
    let end = std::cmp::min(8 + nbt_size, raw.len());
    CompoundTag::from_binary_nbt(&raw[8..end], true).map(|(tag, _)| tag)
        .map_err(|e| format!("NBT 解析错误: {}", e))
}

// ── Player scanning ──

fn scan_player_keys(db: &mut DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = Vec::new();
    let prefixes: &[&[u8]] = &[b"~local_player", b"~player_", b"player_"];

    for prefix in prefixes {
        let mut iter = match db.new_iter() {
            Ok(it) => it,
            Err(_) => return results,
        };
        iter.seek(prefix);
        while let Some((key, value)) = iter.next() {
            if !key.starts_with(prefix) { break; }
            if prefix == b"player_" && key.starts_with(b"player_server_") { continue; }
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
    iter.seek(b"player_server_");
    while let Some((key, value)) = iter.next() {
        if !key.starts_with(b"player_server_") { break; }
        results.push((key, value));
    }
    results
}

fn scan_actor_keys(db: &mut DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    iter.seek(b"actorprefix");
    while let Some((key, value)) = iter.next() {
        if !key.starts_with(b"actorprefix") { break; }
        results.push((key, value));
    }
    results
}

fn fmt_pos(tag: &Tag) -> String {
    match tag {
        Tag::List(lv) => {
            let v: Vec<String> = lv.elements.iter().filter_map(tag_to_f64).map(|n| format!("{:.1}", n)).collect();
            if v.len() >= 3 { format!("({}, {}, {})", v[0], v[1], v[2]) } else { v.join(", ") }
        }
        _ => "?".into(),
    }
}

fn fmt_dim_id(id: i32) -> &'static str {
    match id { 0 => "主世界", 1 => "下界", 2 => "末地", _ => "?" }
}

fn fmt_game_type(id: i32) -> &'static str {
    match id { 0 => "生存", 1 => "创造", 2 => "冒险", 3 => "旁观", _ => "?" }
}

fn show_nbt_keys(tag: &CompoundTag, max_keys: usize) {
    let keys: Vec<String> = tag.iter_sorted().into_iter().map(|(k, _)| k.to_string()).collect();
    for k in &keys[..std::cmp::min(keys.len(), max_keys)] {
        if let Some(v) = tag.get(k) {
            let val_str = match v {
                Tag::String(s) => format!("\"{}\"", s),
                _ => String::new(),
            };
            let type_name = match v {
                Tag::Byte(_) => "byte",
                Tag::Short(_) => "short",
                Tag::Int(_) => "int",
                Tag::Long(_) => "long",
                Tag::Float(_) => "float",
                Tag::Double(_) => "double",
                Tag::String(_) => "string",
                Tag::Compound(_) => "compound",
                Tag::List(l) => {
                    if l.elements.is_empty() { "[]" } else { match l.elements.first().unwrap() {
                        Tag::Compound(_) => "compound[]",
                        _ => "list",
                    }}
                },
                _ => "?",
            };
            if val_str.is_empty() {
                println!("    {}: {}", k, type_name);
            } else {
                println!("    {}: {}", k, val_str);
            }
        }
    }
    if keys.len() > max_keys {
        println!("    ... 还有 {} 个字段", keys.len() - max_keys);
    }
}

fn show_player_info_full(key: &[u8], value: &[u8]) {
    show_player_info_full_inner(key, value, false);
}

fn show_player_info_full_inner(key: &[u8], value: &[u8], dump: bool) {
    let key_label = String::from_utf8_lossy(key);
    println!("  ─── {} ───", key_label);

    let tag = match CompoundTag::from_binary_nbt(value, true) {
        Ok((t, _)) => t,
        Err(_) => {
            let first = if value.len() > 8 { &value[..8] } else { value };
            println!("   无法解析为 NBT, 首字节: {:02x?}", first);
            return;
        }
    };

    if dump {
        for line in tag_to_lines(&tag.to_tag(), "", 1) {
            println!("{}", line);
        }
        println!("\n  (完整 NBT 转储，{} 字节)", value.len());
        return;
    }

    if let Some(id) = tag.get("identifier").and_then(|t| t.as_str()) {
        println!("  类型: {}", id);
    }

    for id_key in &["MsaId", "SelfSignedId", "ServerId"] {
        if let Some(val) = tag.get(*id_key).and_then(|t| t.as_str()) {
            println!("  {}: {}", id_key, val);
        }
    }

    if let Some(pos) = tag.get("Pos") {
        println!("  位置: {}", fmt_pos(pos));
    }

    let dim = tag.get("DimensionId").and_then(tag_to_i32)
        .or_else(|| tag.get("Dimension").and_then(tag_to_i32));
    if let Some(d) = dim {
        println!("  维度: {} ({})", d, fmt_dim_id(d));
    }

    if let Some(rot) = tag.get("Rotation") {
        if let Tag::List(lv) = rot {
            if lv.elements.len() >= 2 {
                println!("  朝向: yaw={:.1}°, pitch={:.1}°",
                    tag_to_f64(&lv.elements[0]).unwrap_or(0.0),
                    tag_to_f64(&lv.elements[1]).unwrap_or(0.0));
            }
        }
    }

    let health = tag.get("Health").and_then(|t| match t {
        Tag::Short(v) => Some(*v as f64),
        Tag::Float(v) => Some(*v as f64),
        Tag::Int(v) => Some(*v as f64),
        _ => None,
    });
    if let Some(h) = health {
        println!("  生命: {:.1}", h);
    }

    if let Some(gt) = tag.get("playerGameType").and_then(tag_to_i32) {
        println!("  游戏模式: {}", fmt_game_type(gt));
    }

    let xp_level = tag.get("Level").and_then(tag_to_i32);
    let xp_progress = tag.get("XpProgress").and_then(tag_to_f64);
    if let Some(lvl) = xp_level {
        let xp_str = xp_progress.map(|p| format!(" ({:.0}%)", p * 100.0)).unwrap_or_default();
        println!("  等级: {}{}", lvl, xp_str);
    }

    if let Some(food) = tag.get("foodLevel").and_then(tag_to_i32) {
        println!("  饱食度: {}", food);
    }
    if let Some(sat) = tag.get("foodSaturationLevel").and_then(|t| tag_to_f64(t)) {
        println!("  饱和度: {:.1}", sat);
    }

    if let Some(air) = tag.get("Air").and_then(tag_to_i32) {
        if air < 300 { println!("  氧气: {} ticks", air); }
    }

    if let Some(fire) = tag.get("Fire").and_then(tag_to_i32) {
        if fire > 0 { println!("  着火: {} ticks", fire); }
    }

    if let Some(uid) = tag.get("PlayerId").and_then(|t| t.as_str()) {
        println!("  XUID: {}", uid);
    }

    if let Some(uid) = tag.get("UniqueID").and_then(tag_to_i64)
        .or_else(|| tag.get("UniqueId").and_then(tag_to_i64))
    {
        println!("  UniqueID: {}", uid);
    }

    if let Some(abi) = tag.get("abilities").and_then(|t| {
        if let Tag::Compound(m) = t { Some(CompoundTag::from_map(m.clone())) } else { None }
    }) {
        println!("  能力:");
        if let Some(spd) = abi.get("walkSpeed").and_then(tag_to_f64) {
            println!("    行走速度: {:.2}", spd);
        }
        for prop in &["mayfly", "instabuild", "invulnerable", "lightning", "flying"] {
            if let Some(Tag::Byte(v)) = abi.get(prop) {
                if *v != 0 { println!("    {}: 是", prop); }
            }
        }
    }

    if let Some(Tag::List(inv)) = tag.get("Inventory") {
        println!("  物品栏: {} 格", inv.elements.len());
        let shown: Vec<String> = inv.elements.iter().filter_map(|elem| {
            if let Tag::Compound(map) = elem {
                let item = CompoundTag::from_map(map.clone());
                let name = item.get("Name").and_then(|t| t.as_str())?;
                let count = item.get("Count").and_then(tag_to_i32)?;
                let slot = item.get("Slot").and_then(tag_to_i32).unwrap_or(0);
                Some(format!("[Slot {:2}] {} x{}", slot, name, count))
            } else { None }
        }).take(8).collect();
        for s in &shown { println!("    {}", s); }
        if inv.elements.len() > 8 {
            println!("    ... 还有 {} 格", inv.elements.len() - 8);
        }
    }

    let known_keys: &[&str] = &["identifier", "Pos", "DimensionId", "Dimension", "Rotation",
        "Health", "playerGameType", "Level", "XpProgress", "foodLevel",
        "foodSaturationLevel", "Air", "Fire", "PlayerId", "UniqueID", "UniqueId",
        "abilities", "Inventory", "MsaId", "SelfSignedId", "ServerId", "InternalComponents"];
    let has_unknown = tag.iter_sorted().into_iter().any(|(k, _)| !known_keys.contains(&k));
    if has_unknown {
        println!("\n  其他 NBT 字段:");
        show_nbt_keys(&tag, 20);
    }
}

fn db_summary(db: &mut DB) {
    let mut cats: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return,
    };

    let mut total = 0;
    while let Some((key, _)) = iter.next() {
        total += 1;
        if total > 200_000 { break; }
        let cat = if key.len() >= 9 {
            match key[8] {
                0x2f => "subchunks",
                0x31 => "block_entities",
                0x32 => "entities",
                0x33 => "pending_ticks",
                0x34 => "block_extra",
                0x36 => "biome_state",
                0x38 => "chunk_version",
                _ => "other",
            }
        } else if key.starts_with(b"~") || key.starts_with(b"player_") {
            "player_data"
        } else {
            "other"
        };
        *cats.entry(cat).or_insert(0) += 1;
    }

    println!("  DB 键: {} 条", total);
    for (cat, n) in cats {
        if n > 0 { println!("    {}: {}", cat, n); }
    }
}

// ── Main ──

fn main() {
    let cmd = match parse_args() {
        Ok(c) => c,
        Err(e) => { eprintln!("错误: {}", e); eprintln!("使用 --help 查看帮助"); std::process::exit(1); }
    };

    let (world_path, dim_id, _dim_name) = match &cmd {
        Command::QueryBlock { world_path, dim_id, dim_name, .. } => (world_path.as_str(), *dim_id, dim_name.as_str()),
        Command::ShowInfo { world_path } | Command::ListPlayers { world_path } | Command::ListActors { world_path } | Command::ShowPlayer { world_path, .. } => (world_path.as_str(), 0u8, "overworld"),
    };

    if !Path::new(world_path).is_dir() {
        eprintln!("错误: 存档目录不存在: {}", world_path);
        std::process::exit(1);
    }

    let level_dat_path = format!("{}/level.dat", world_path);
    println!("═══ {} ═══", world_path);
    let level = match read_level_dat(&level_dat_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("  level.dat: {}", e); std::process::exit(1); }
    };

    let world_name = level.get("LevelName").and_then(|t| t.as_str()).unwrap_or("(unknown)");
    let inv_ver = level.get("InventoryVersion").and_then(|t| t.as_str()).unwrap_or("(unknown)");
    println!("  World: {}  (v{})", world_name, inv_ver);

    let db_candidates = resolve_db_path(world_path, dim_id);
    let mut db_path = None;
    for c in &db_candidates {
        if Path::new(c).is_dir() && Path::new(&format!("{}/CURRENT", c)).exists() {
            db_path = Some(c.clone()); break;
        }
    }
    let db_path = match db_path {
        Some(p) => { println!("  DB: {}", p); p }
        None => { eprintln!("错误: 找不到 LevelDB 数据"); std::process::exit(1); }
    };

    let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
    opt.read_only = true;
    let mut db = match DB::open(&db_path, opt) {
        Ok(d) => d,
        Err(e) => { eprintln!("  DB 打开失败: {}", e); std::process::exit(1); }
    };

    // ─── Dispatch ───
    match cmd {
        Command::QueryBlock { x: tx, y: ty, z: tz, .. } => {
            let cx = floor_div(tx, 16); let cz = floor_div(tz, 16);
            let cy = floor_div(ty, 16) as i8;
            let lx = (tx - cx * 16) as usize; let ly = (ty - (cy as i32) * 16) as usize; let lz = (tz - cz * 16) as usize;

            println!("\n═══ 查询 ({}, {}, {}) ═══", tx, ty, tz);
            println!("  区块 ({}, {})  子区块 y={}  区块内 ({}, {}, {})", cx, cz, cy, lx, ly, lz);

            let skey = subchunk_key(cx, cz, cy);
            let sdata = match db.get(&skey) {
                Some(d) => d,
                None => {
                    eprintln!("\n错误: 子区块不存在");
                    eprintln!("  键: {:02x?}", skey);
                    eprintln!("  原因: 区块未生成 / 坐标超出范围 / 无数据");
                    eprintln!("  提示: 尝试周围坐标或其他维度");
                    std::process::exit(1);
                }
            };
            println!("  子区块: {} 字节 (v{})", sdata.len(), sdata[0]);

            let info = match get_block_from_subchunk(&sdata, lx, ly, lz) {
                Some(i) => i,
                None => { eprintln!("\n错误: 子区块数据无法解析"); std::process::exit(1); }
            };

            println!("\n  → {}", info.name);
            if !info.states.empty() {
                println!("    属性:");
                for (k, v) in info.states.iter_sorted() { println!("      {} = {}", k, v.to_snbt()); }
            }
            if !info.palette.is_empty() {
                println!("\n    子区块方块调色板 ({} 种):", info.palette.len());
                for (i, (name, states)) in info.palette.iter().enumerate() {
                    print!("      [{}] {}", i, name);
                    if !states.empty() {
                        let props: Vec<String> = states.iter_sorted().into_iter()
                            .map(|(k, v)| format!("{}={}", k, v.to_snbt())).collect();
                        print!(" ({})", props.join(", "));
                    }
                    println!();
                }
            }

            let is_container = info.name.contains("chest") || info.name.contains("barrel")
                || info.name.contains("shulker") || info.name.contains("hopper")
                || info.name.contains("dispenser") || info.name.contains("Dropper");
            if is_container {
                println!("\n  ═══ 容器内容 ═══");
                let ekey = block_entity_key(cx, cz);
                if let Some(edata) = db.get(&ekey) {
                    match find_block_entity(&edata, tx, ty, tz) {
                        Some(be_tag) => {
                            if let Some(id) = be_tag.get("id").and_then(|t| t.as_str()) { println!("  ID: {}", id); }
                            if let Some(Tag::String(cn)) = be_tag.get("CustomName") {
                                let c = if cn.starts_with('"') && cn.ends_with('"') { cn[1..cn.len()-1].to_string() } else { cn.clone() };
                                println!("  名称: {}", c);
                            }
                            println!();
                            for line in format_chest_items(&be_tag) { println!("{}", line); }
                        }
                        None => println!("  该坐标处未找到方块实体"),
                    }
                } else { println!("  区块无方块实体数据"); }
            }
        }

        Command::ShowInfo { .. } => {
            let players = scan_player_keys(&mut db);
            let actors = scan_actor_keys(&mut db);

            println!("\n═══ 存档概要 ═══");
            println!("  玩家: {} 位", players.len());
            println!("  实体(actor): {} 个", actors.len());

            db_summary(&mut db);

            if !players.is_empty() {
                println!("  使用 --players 查看所有玩家");
                println!("  使用 --actors 查看所有实体");
                println!("  使用 --player <key> 查看玩家详情");
            }
        }

        Command::ListPlayers { .. } => {
            let players = scan_player_keys(&mut db);

            println!("\n═══ 玩家列表 ({} 位) ═══", players.len());
            if players.is_empty() {
                println!("  未找到玩家记录");
            } else {
                for (key, _) in &players {
                    println!("  {}", String::from_utf8_lossy(key));
                }
            }
        }

        Command::ListActors { .. } => {
            let actors = scan_actor_keys(&mut db);

            println!("\n═══ 实体列表 ({} 个) ═══", actors.len());
            if actors.is_empty() {
                println!("  未找到实体");
            } else {
                let mut player_actors: Vec<&Vec<u8>> = Vec::new();
                let mut type_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

                for (key, value) in &actors {
                    let actor_type = if let Ok((tag, _)) = CompoundTag::from_binary_nbt(value, true) {
                        tag.get("identifier").and_then(|t| t.as_str())
                            .or_else(|| tag.get("id").and_then(|t| t.as_str()))
                            .unwrap_or("unknown")
                            .to_string()
                    } else {
                        "unknown".to_string()
                    };

                    if actor_type == "minecraft:player" {
                        player_actors.push(key);
                    }
                    *type_counts.entry(actor_type).or_insert(0) += 1;
                }

                println!("\n  实体类型分布 (前 30):");
                let sorted: Vec<_> = {
                    let mut v: Vec<_> = type_counts.into_iter().collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1));
                    v
                };
                for (t, n) in sorted.iter().take(30) {
                    let marker = if t == "minecraft:player" { " ←" } else { "" };
                    println!("    {}: {}{}", t, n, marker);
                }

                if !player_actors.is_empty() {
                    println!("\n  玩家实体 ({}):", player_actors.len());
                    let show = std::cmp::min(player_actors.len(), 20);
                    for i in 0..show {
                        let k = player_actors[i];
                        let key_hex: Vec<String> = k.iter().map(|b| format!("{:02x}", b)).collect();
                        println!("    [{}] key={}", i + 1, key_hex.join(" "));
                    }
                    if player_actors.len() > 20 {
                        println!("    ... 还有 {} 个", player_actors.len() - 20);
                    }
                }

                println!("\n  使用 --player <key> 查看玩家数据");
            }
        }

        Command::ShowPlayer { ref player_key, dump, json, .. } => {
            // JSON mode: use library's WorldHandle to get structured data
            if json {
                match WorldHandle::open(world_path) {
                    Ok(mut handle) => {
                        match handle.get_player_inventory(player_key) {
                            Ok(inv) => {
                                println!("{}", serde_json::to_string_pretty(&inv).unwrap());
                            }
                            Err(e) => {
                                eprintln!("错误: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("错误: {}", e);
                        std::process::exit(1);
                    }
                }
                return;
            }

            let players = scan_all_player_keys(&mut db);

            let needle = player_key.as_bytes();
            let partial_matches: Vec<_> = players.iter().filter(|(k, _)| {
                String::from_utf8_lossy(k).contains(player_key)
            }).collect();

            let found = players.iter().find(|(k, _)| k.as_slice() == needle)
                .or_else(|| {
                    if partial_matches.len() == 1 { Some(partial_matches[0]) } else { None }
                });

            if let Some((key, value)) = found {
                let key_label = String::from_utf8_lossy(key);
                println!("\n═══ {} ═══", key_label);
                if dump {
                    show_player_info_full_inner(key, value, true);
                } else {
                    show_player_info_full(key, value);
                }

                if key.starts_with(b"player_") && !key.starts_with(b"player_server_") {
                    if let Ok((tag, _)) = CompoundTag::from_binary_nbt(value, true) {
                        if let Some(server_id) = tag.get("ServerId").and_then(|t| t.as_str()) {
                            let server_key = server_id.as_bytes();
                            if let Some(sv) = players.iter().find(|(k, _)| k.as_slice() == server_key) {
                                println!("\n═══ 关联服务器数据: {} ═══", server_id);
                                show_player_info_full_inner(&sv.0, &sv.1, dump);
                            }
                        }
                    }
                }
            } else if partial_matches.len() > 1 {
                eprintln!("错误: '{}' 匹配 {} 位玩家:", player_key, partial_matches.len());
                for (k, _) in partial_matches {
                    eprintln!("  {}", String::from_utf8_lossy(k));
                }
                std::process::exit(1);
            } else {
                eprintln!("错误: 未找到玩家 '{}'", player_key);
                let show = std::cmp::min(players.len(), 10);
                eprintln!("可用玩家 (前 {} 位):", show);
                for (k, _) in players.iter().take(show) {
                    eprintln!("  {}", String::from_utf8_lossy(k));
                }
                if players.len() > 10 {
                    eprintln!("  ... 还有 {} 位", players.len() - 10);
                }
                eprintln!("\n提示: 用 --player <UUID> 搜索，或用 --actors 列出所有实体");
                std::process::exit(1);
            }
        }
    }

    println!();
    std::process::exit(0);
}
