use std::path::Path;
use world_inspector::WorldHandle;
use std::rc::Rc;

use base64::{Engine as _, engine::general_purpose};
use bedrock_nbt::{CompoundTag, Tag};
use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib, CompressionLevel};
use miniz_oxide::inflate::{decompress_to_vec, decompress_to_vec_zlib};
use rusty_leveldb::compressor::NoneCompressor;
use rayon::prelude::*;
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
    WipeActors { world_path: String, include_players: bool },
    ExportActors { world_path: String, output_file: String, no_players: bool },
    ExportChunks { world_path: String, output_file: String, bx1: i32, bz1: i32, bx2: i32, bz2: i32, dim: Option<u8> },
    ImportActors { world_path: String, input_file: String, skip_existing: bool },
    ImportChunks { world_path: String, input_file: String, skip_existing: bool, to: Option<(i32, i32)>, dim_override: Option<u8>, dry_run: bool },
    EntityDensity { world_path: String, group_size: u32 },
    DeleteChunks { world_path: String, bx1: i32, bz1: i32, bx2: i32, bz2: i32, dim_id: u8, dim_name: String },
    BatchDeleteChunks { world_path: String, input_file: String, invert: bool },
}

fn print_help() {
    eprintln!(
"Usage: world-inspector <world_path> <x> <y> <z> [dimension]
       world-inspector <world_path>
       world-inspector <world_path> --players
       world-inspector <world_path> --actors
       world-inspector <world_path> --player <key>
       world-inspector <world_path> --wipe-actors [--include-players]
       world-inspector <world_path> --export-actors <file> [--no-players]
       world-inspector <world_path> --export-chunks <file> <bx> <bz>
       world-inspector <world_path> --export-chunks <file> <bx1> <bz1> <bx2> <bz2>
       world-inspector <world_path> --import-chunks <file> [--skip-existing]
       world-inspector <world_path> --import-actors <file> [--skip-existing]
       world-inspector <world_path> --entity-density [N]

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

实体管理：
  world-inspector /path/to/world --wipe-actors                  擦除非玩家实体
  world-inspector /path/to/world --wipe-actors --include-players 擦除全部实体（含玩家数据）
  world-inspector /path/to/world --export-actors <file>         导出实体到 JSON 文件（含玩家）
  world-inspector /path/to/world --export-actors <file> --no-players  导出仅非玩家实体
  world-inspector /path/to/world --import-actors <file>         从 JSON 导入实体（覆盖）
  world-inspector /path/to/world --import-actors <file> --skip-existing  跳过已存在实体

区块管理：
  world-inspector /path/to/world --export-chunks <file> <bx> <bz> [dimension]            导出单区块(方块坐标)
  world-inspector /path/to/world --export-chunks <file> <bx1> <bz1> <bx2> <bz2> [dimension]  导出区块范围
  world-inspector /path/to/world --import-chunks <file>           从 JSON 导入区块（覆盖）
  world-inspector /path/to/world --import-chunks <file> --skip-existing  跳过已存在 key
  world-inspector /path/to/world --import-chunks <file> --to <bx> <bz> [--dimension <dim>] [--dry-run]  定点平移导入(复制到新位置)
  world-inspector /path/to/world --delete-chunks <bx1> <bz1> <bx2> <bz2> [dimension]  删除区块范围
  world-inspector /path/to/world --batch-delete-chunks <file> [--invert]  从 JSON 批量删除区块

实体密度分析：

  world-inspector /path/to/world --entity-density          按单区块统计实体密度 Top 5
  world-inspector /path/to/world --entity-density 2        按 2×2 区块组统计
  world-inspector /path/to/world --entity-density 4        按 4×4 区块组统计
");
}

/// Parse a dimension argument: "overworld"/"0", "nether"/"1", "end"/"2".
fn parse_dimension_arg(raw: &str) -> Result<u8, String> {
    match raw.to_lowercase().as_str() {
        "overworld" | "0" => Ok(0u8),
        "nether" | "1"    => Ok(1u8),
        "end" | "2"       => Ok(2u8),
        _ => Err(format!("未知维度 '{}'，可选: overworld(0) nether(1) end(2)", raw)),
    }
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

    if args.len() >= 3 && args[2] == "--wipe-actors" {
        let include_players = args[3..].iter().any(|a| a == "--include-players");
        return Ok(Command::WipeActors { world_path, include_players });
    }

    if args.len() >= 4 && args[2] == "--export-actors" {
        let no_players = args[4..].iter().any(|a| a == "--no-players");
        return Ok(Command::ExportActors { world_path, output_file: args[3].clone(), no_players });
    }

    if args.len() >= 4 && args[2] == "--import-actors" {
        let skip_existing = args[4..].iter().any(|a| a == "--skip-existing");
        return Ok(Command::ImportActors { world_path, input_file: args[3].clone(), skip_existing });
    }

    if args.len() >= 4 && args[2] == "--export-chunks" {
        let file = args[3].clone();
        let extra: Vec<&String> = args[4..].iter().collect();
        // Optional trailing dimension token (non-numeric, e.g. "nether")
        let (mut dim, mut coords): (Option<u8>, Vec<&String>) = (None, Vec::new());
        for tok in &extra {
            if tok.parse::<i32>().is_ok() {
                coords.push(tok);
            } else if dim.is_none() {
                dim = Some(parse_dimension_arg(tok)?);
            } else {
                return Err(format!("参数 '{}' 无法解析", tok));
            }
        }
        match coords.len() {
            2 => {
                let bx = coords[0].parse::<i32>().map_err(|_| format!("bx 格式错误: '{}'", coords[0]))?;
                let bz = coords[1].parse::<i32>().map_err(|_| format!("bz 格式错误: '{}'", coords[1]))?;
                return Ok(Command::ExportChunks { world_path, output_file: file, bx1: bx, bz1: bz, bx2: bx, bz2: bz, dim });
            }
            4 => {
                let bx1 = coords[0].parse::<i32>().map_err(|_| format!("bx1 格式错误: '{}'", coords[0]))?;
                let bz1 = coords[1].parse::<i32>().map_err(|_| format!("bz1 格式错误: '{}'", coords[1]))?;
                let bx2 = coords[2].parse::<i32>().map_err(|_| format!("bx2 格式错误: '{}'", coords[2]))?;
                let bz2 = coords[3].parse::<i32>().map_err(|_| format!("bz2 格式错误: '{}'", coords[3]))?;
                return Ok(Command::ExportChunks { world_path, output_file: file, bx1, bz1, bx2, bz2, dim });
            }
            _ => return Err("区块导出需要 2 个坐标(单区块) 或 4 个坐标(范围)，维度可选".into()),
        }
    }

    if args.len() >= 4 && args[2] == "--import-chunks" {
        let mut skip_existing = false;
        let mut dry_run = false;
        let mut to: Option<(i32, i32)> = None;
        let mut dim_override: Option<u8> = None;
        let mut rest = args[4..].iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--skip-existing" => skip_existing = true,
                "--dry-run" => dry_run = true,
                "--to" => {
                    let bx = rest.next().ok_or("--to 需要 <bx> <bz>")?
                        .parse::<i32>().map_err(|_| "--to bx 格式错误".to_string())?;
                    let bz = rest.next().ok_or("--to 需要 <bx> <bz>")?
                        .parse::<i32>().map_err(|_| "--to bz 格式错误".to_string())?;
                    to = Some((bx, bz));
                }
                "--dimension" => {
                    let raw = rest.next().ok_or("--dimension 需要维度名".to_string())?;
                    dim_override = Some(parse_dimension_arg(raw)?);
                }
                _ => return Err(format!("未知参数 '{}'", arg)),
            }
        }
        return Ok(Command::ImportChunks { world_path, input_file: args[3].clone(), skip_existing, to, dim_override, dry_run });
    }

    if args.len() >= 3 && args[2] == "--entity-density" {
        let group_size = if args.len() >= 4 {
            args[3].parse::<u32>().map_err(|_| format!("区块组大小格式错误: '{}'", args[3]))?
        } else {
            1
        };
        if group_size == 0 || group_size > 256 {
            return Err("区块组大小必须在 1-256 之间".into());
        }
        return Ok(Command::EntityDensity { world_path, group_size });
    }

    if args.len() >= 7 && args[2] == "--delete-chunks" {
        let bx1 = args[3].parse::<i32>().map_err(|_| format!("bx1 格式错误: '{}'", args[3]))?;
        let bz1 = args[4].parse::<i32>().map_err(|_| format!("bz1 格式错误: '{}'", args[4]))?;
        let bx2 = args[5].parse::<i32>().map_err(|_| format!("bx2 格式错误: '{}'", args[5]))?;
        let bz2 = args[6].parse::<i32>().map_err(|_| format!("bz2 格式错误: '{}'", args[6]))?;
        let (dim_id, dim_name) = if let Some(raw) = args.get(7) {
            let d = parse_dimension_arg(raw)?;
            (d, raw.to_string())
        } else {
            (0u8, "overworld".into())
        };
        return Ok(Command::DeleteChunks { world_path, bx1, bz1, bx2, bz2, dim_id, dim_name });
    }

    if args.len() >= 4 && args[2] == "--batch-delete-chunks" {
        let invert = args[4..].iter().any(|a| a == "--invert");
        return Ok(Command::BatchDeleteChunks { world_path, input_file: args[3].clone(), invert });
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
        let d = parse_dimension_arg(raw)?;
        (d, raw.to_string())
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

/// Bedrock 版所有维度共享同一个 db 文件夹，dim_id 仅供查询时区分维度标签。
fn resolve_db_path(world_path: &str, _dim_id: u8) -> String {
    format!("{}/db", world_path.trim_end_matches('/'))
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

/// 前缀扫描统一使用 seek_to_first + 分类。
/// 注意: fork 的 leveldb seek 存在索引定位缺陷（对部分 ASCII 前缀会跳过头），
/// 全量扫描可避免。DB 键量级 ~10 万，扫描成本可接受。
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

fn scan_actor_keys(db: &mut DB) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    iter.seek_to_first();
    while let Some((key, value)) = iter.next() {
        if key.starts_with(b"actorprefix") {
            results.push((key, value));
        }
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

// ── Entity management ──

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("hex string length must be even".into());
    }
    (0..s.len()).step_by(2).map(|i| {
        u8::from_str_radix(&s[i..i+2], 16)
            .map_err(|_| format!("invalid hex at position {}", i))
    }).collect()
}

fn cmd_export_actors(db: &mut DB, output_file: &str, no_players: bool) {
    // Collect entity-related keys:
    //   actorprefix*  — entity NBT data
    //   digp*         — entity digest / chunk linkage
    //   player_*, ~*  — player data (omitted if no_players)
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();

    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => { eprintln!("错误: 无法创建 DB 迭代器"); std::process::exit(1); }
    };
    iter.seek_to_first();
    while let Some((key, value)) = iter.next() {
        let is_actorprefix = key.starts_with(b"actorprefix");
        let is_digp = key.starts_with(b"digp");
        let is_player_data = key.starts_with(b"player_") || key.starts_with(b"~");

        if is_actorprefix {
            // Determine if this actor is a player
            let is_player_actor = CompoundTag::from_binary_nbt(&value, true).ok()
                .map(|(tag, _)| tag.get("identifier").and_then(|t| t.as_str().map(|s| s.to_string())))
                .flatten()
                .as_deref()
                == Some("minecraft:player");

            // Skip player actors when --no-players
            if no_players && is_player_actor {
                continue;
            }

            counts.entry("actors").or_insert(0);
            *counts.get_mut("actors").unwrap() += 1;
            let identifier = if is_player_actor { Some("minecraft:player".to_string()) } else {
                CompoundTag::from_binary_nbt(&value, true).ok().and_then(|(tag, _)| {
                    tag.get("identifier").and_then(|t| t.as_str().map(|s| s.to_string()))
                        .or_else(|| tag.get("id").and_then(|t| t.as_str().map(|s| s.to_string())))
                })
            };
            entries.push(serde_json::json!({
                "key_hex": hex_encode(&key), "value_base64": general_purpose::STANDARD.encode(&value),
                "identifier": identifier,
            }));
        } else if is_digp {
            *counts.entry("digp").or_insert(0) += 1;
            entries.push(serde_json::json!({
                "key_hex": hex_encode(&key), "value_base64": general_purpose::STANDARD.encode(&value),
            }));
        } else if is_player_data && !no_players {
            *counts.entry("players").or_insert(0) += 1;
            entries.push(serde_json::json!({
                "key_hex": hex_encode(&key), "value_base64": general_purpose::STANDARD.encode(&value),
            }));
        }
    }

    if entries.is_empty() {
        println!("  未找到实体数据，导出空文件");
    }

    let total = entries.len();
    let export = serde_json::json!({
        "total": total,
        "entries": entries,
    });

    let json_str = serde_json::to_string_pretty(&export)
        .unwrap_or_else(|e| { eprintln!("错误: JSON 序列化失败: {}", e); std::process::exit(1); });
    std::fs::write(output_file, &json_str)
        .unwrap_or_else(|e| { eprintln!("错误: 写入文件失败: {}", e); std::process::exit(1); });

    print!("  已导出 {} 条记录", total);
    for (cat, n) in &counts {
        print!(", {}: {}", cat, n);
    }
    println!("  → {}", output_file);
}

fn cmd_import_actors(db: &mut DB, input_file: &str, skip_existing: bool) {
    let json_str = std::fs::read_to_string(input_file)
        .unwrap_or_else(|e| { eprintln!("错误: 读取文件失败: {}", e); std::process::exit(1); });
    let data: serde_json::Value = serde_json::from_str(&json_str)
        .unwrap_or_else(|e| { eprintln!("错误: JSON 解析失败: {}", e); std::process::exit(1); });

    let total = data["total"].as_u64().unwrap_or(0) as usize;
    let entries = match data["entries"].as_array() {
        Some(arr) => arr,
        None => { eprintln!("错误: JSON 格式无效，缺少 entries 字段"); std::process::exit(1); }
    };

    let mut imported = 0usize;
    let mut skipped = 0usize;

    for entry in entries {
        let key_hex = entry["key_hex"].as_str()
            .unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
        let value_b64 = entry["value_base64"].as_str()
            .unwrap_or_else(|| { eprintln!("错误: 条目缺少 value_base64"); std::process::exit(1); });

        let key = hex_decode(key_hex)
            .unwrap_or_else(|e| { eprintln!("错误: key 解析失败 ({}): {}", key_hex, e); std::process::exit(1); });
        let value = general_purpose::STANDARD.decode(value_b64)
            .unwrap_or_else(|e| { eprintln!("错误: base64 解码失败: {}", e); std::process::exit(1); });

        if skip_existing {
            if db.get(&key).is_some() {
                skipped += 1;
                continue;
            }
        }

        db.put(&key, &value)
            .unwrap_or_else(|e| { eprintln!("错误: DB 写入失败: {}", e); std::process::exit(1); });
        imported += 1;
    }

    println!("  导入完成: 总数 {}, 已导入 {}, 已跳过 {}", total, imported, skipped);
}

// ── Entity density analysis ──

struct EntityRecord {
    dim: u8,
    group_x: i32,
    group_z: i32,
    identifier: String,
}

fn dim_label(dim: u8) -> &'static str {
    match dim { 0 => "主世界", 1 => "下界", 2 => "末地", _ => "?" }
}

fn cmd_entity_density(db: &mut DB, group_size: u32) {
    // Phase 1: scan all actorprefix entries (sequential — DB iterator not Send)
    let actors = scan_actor_keys(db);
    let total = actors.len();
    if total == 0 {
        println!("\n═══ 区块实体密度分析 ═══\n  存档中无实体数据");
        return;
    }
    eprintln!("  已收集 {} 个实体，正在并行解码 NBT...", total);

    // Phase 2: parallel NBT decode + position/dimension extraction
    let records: Vec<EntityRecord> = actors.par_iter().filter_map(|(_key, value)| {
        let tag = CompoundTag::from_binary_nbt(value, true).ok()?.0;

        let identifier = tag.get("identifier")
            .or_else(|| tag.get("id"))
            .and_then(|t| t.as_str())
            .unwrap_or("unknown")
            .to_string();

        let dim = tag.get("DimensionId")
            .or_else(|| tag.get("Dimension"))
            .and_then(|t| match t {
                Tag::Byte(v) => Some(*v as u8),
                Tag::Short(v) => Some(*v as u8),
                Tag::Int(v) => Some(*v as u8),
                _ => None,
            })
            .unwrap_or(0);

        let pos = tag.get("Pos")?;
        let Tag::List(lv) = pos else { return None; };
        if lv.elements.len() < 3 { return None; }
        let x = tag_to_f64(&lv.elements[0])?;
        let z = tag_to_f64(&lv.elements[2])?;

        let cx = (x as i64 as i32).div_euclid(16);
        let cz = (z as i64 as i32).div_euclid(16);
        let gx = cx.div_euclid(group_size as i32);
        let gz = cz.div_euclid(group_size as i32);

        Some(EntityRecord { dim, group_x: gx, group_z: gz, identifier })
    }).collect();

    // Phase 3: aggregate by (dimension, group)
    #[derive(Default)]
    struct GroupStat {
        count: usize,
        types: std::collections::HashMap<String, usize>,
    }

    let mut groups: std::collections::HashMap<(u8, i32, i32), GroupStat> = std::collections::HashMap::new();
    for rec in &records {
        let key = (rec.dim, rec.group_x, rec.group_z);
        let stat = groups.entry(key).or_default();
        stat.count += 1;
        *stat.types.entry(rec.identifier.clone()).or_insert(0) += 1;
    }

    // Phase 4: top 5
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count));

    // Per-dimension stats
    let mut dim_counts: std::collections::BTreeMap<u8, usize> = std::collections::BTreeMap::new();
    for rec in &records {
        *dim_counts.entry(rec.dim).or_insert(0) += 1;
    }

    let gs = group_size;
    println!("\n═══ 区块实体密度 Top 5 ({}×{} 区块组) ═══", gs, gs);
    print!("  实体总数: {}", total);
    for (d, n) in &dim_counts {
        print!("  {}: {}", dim_label(*d), n);
    }
    println!("  区块组: {}", sorted.len());

    for (rank, ((dim, gx, gz), stat)) in sorted.iter().take(5).enumerate() {
        let cx_min = gx * gs as i32;
        let cz_min = gz * gs as i32;
        let cx_max = cx_min + gs as i32 - 1;
        let cz_max = cz_min + gs as i32 - 1;
        let mid_cx = (cx_min + cx_max) / 2;
        let mid_cz = (cz_min + cz_max) / 2;
        let mid_bx = mid_cx * 16 + 8;
        let mid_bz = mid_cz * 16 + 8;

        println!("\n  #{}  [{}] 区块组 ({}, {}) ~ ({}, {})  中点方块 ({}, {})",
            rank + 1, dim_label(*dim), cx_min, cz_min, cx_max, cz_max, mid_bx, mid_bz);
        println!("      实体总数: {}", stat.count);

        let mut top_types: Vec<_> = stat.types.iter().collect();
        top_types.sort_by(|a, b| b.1.cmp(a.1));
        println!("      Top 实体:");
        for (t, n) in top_types.iter().take(5) {
            println!("        {}: {}", t, n);
        }
    }
}

// ── Chunk management ──

fn collect_chunk_keys(db: &mut DB, cx: i32, cz: i32) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut results = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    let mut seek_key = Vec::with_capacity(8);
    seek_key.extend_from_slice(&cx.to_le_bytes());
    seek_key.extend_from_slice(&cz.to_le_bytes());
    iter.seek(&seek_key);
    while let Some((key, value)) = iter.next() {
        if key.len() < 8 { continue; }
        let kcx = i32::from_le_bytes(key[0..4].try_into().unwrap());
        let kcz = i32::from_le_bytes(key[4..8].try_into().unwrap());
        if kcx != cx || kcz != cz { break; }
        results.push((key, value));
    }
    results
}

fn cmd_export_chunks(db: &mut DB, output_file: &str, bx1: i32, bz1: i32, bx2: i32, bz2: i32, dim_filter: Option<u8>) {
    let cx1 = bx1.div_euclid(16);
    let cz1 = bz1.div_euclid(16);
    let cx2 = bx2.div_euclid(16);
    let cz2 = bz2.div_euclid(16);
    let (cxa, cxb) = if cx1 <= cx2 { (cx1, cx2) } else { (cx2, cx1) };
    let (cza, czb) = if cz1 <= cz2 { (cz1, cz2) } else { (cz2, cz1) };
    let dim_label = dim_filter.map(|d| match d { 0 => "主世界", 1 => "下界", 2 => "末地", _ => "?" }).unwrap_or("全部维度");
    println!("  导出区域: 区块 ({}, {}) ~ ({}, {})  [{}{}]",
        cxa, cza, cxb, czb, dim_label,
        if dim_filter.is_none() { "（未指定维度时含全部维度）" } else { "" });

    let matches_dim = |key: &[u8]| -> bool {
        dim_filter.map_or(true, |f| extract_dim_from_key(key) == f)
    };

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut chunk_list: Vec<String> = Vec::new();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut current = 0;

    // ── Phase 1: chunk keys in rect (dimension-filtered) ──
    for cx in cxa..=cxb {
        for cz in cza..=czb {
            let keys = collect_chunk_keys(db, cx, cz);
            if keys.is_empty() { continue; }
            current += 1;
            chunk_list.push(format!("{},{}", cx, cz));
            for (key, value) in &keys {
                if !matches_dim(key) { continue; }
                *counts.entry("chunk").or_insert(0) += 1;
                entries.push(serde_json::json!({
                    "key_hex": hex_encode(key),
                    "value_base64": general_purpose::STANDARD.encode(value),
                }));
            }
        }
    }

    // ── Phase 2+3 (单次全扫): digp + actorprefix 收集 ──
    // 注: fork 的 leveldb seek 存在缺陷(manifest last_seq 偏小时, seek 会定位到 seq 超快照的
    // 隐藏版本而被过滤跳过, 导致部分前缀的键"找不到"), 全量扫描可避免。
    // digp 键先收集(建立存储键链接集), actorprefix 键暂存, 扫描结束后按链接集筛选。
    let mut actor_keys: std::collections::HashSet<[u8; 8]> = std::collections::HashSet::new();
    let mut has_digp_world = false;
    let mut actors_pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => { eprintln!("错误: 无法创建 DB 迭代器"); std::process::exit(1); }
    };
    iter.seek_to_first();
    while let Some((key, value)) = iter.next() {
        if key.starts_with(b"digp") {
            has_digp_world = true;
            let (cx, cz, dim) = match key.len() {
                12 => (i32::from_le_bytes(key[4..8].try_into().unwrap()),
                       i32::from_le_bytes(key[8..12].try_into().unwrap()), 0u8),
                16 => (i32::from_le_bytes(key[4..8].try_into().unwrap()),
                       i32::from_le_bytes(key[8..12].try_into().unwrap()),
                       i32::from_le_bytes(key[12..16].try_into().unwrap()) as u8),
                _ => continue,
            };
            if cx >= cxa && cx <= cxb && cz >= cza && cz <= czb && dim_filter.map_or(true, |f| dim == f) {
                *counts.entry("digp").or_insert(0) += 1;
                for chunk in value.chunks_exact(8) {
                    if let Ok(k) = chunk.try_into() { actor_keys.insert(k); }
                }
                entries.push(serde_json::json!({
                    "key_hex": hex_encode(&key),
                    "value_base64": general_purpose::STANDARD.encode(&value),
                }));
            }
        } else if key.starts_with(b"actorprefix") {
            if key.len() == 19 {
                actors_pending.push((key, value));
            }
        }
    }

    // actorprefix 筛选: 现代存储按 digp 链接(维度权威); 旧版无 digp 按 Pos 兜底
    for (key, value) in &actors_pending {
        let storage_key: [u8; 8] = key[11..19].try_into().unwrap();
        let linked = if has_digp_world {
            actor_keys.contains(&storage_key)
        } else {
            CompoundTag::from_binary_nbt(value, true).ok().map(|(tag, _)| {
                if tag.get("identifier").and_then(|t| t.as_str()) == Some("minecraft:player") { return false; }
                match tag.get("Pos") {
                    Some(Tag::List(lv)) if lv.elements.len() >= 3 => {
                        let x = tag_to_f64(&lv.elements[0]).unwrap_or(f64::NAN);
                        let z = tag_to_f64(&lv.elements[2]).unwrap_or(f64::NAN);
                        if !x.is_finite() || !z.is_finite() { return false; }
                        let cx = (x as i64 as i32).div_euclid(16);
                        let cz = (z as i64 as i32).div_euclid(16);
                        cx >= cxa && cx <= cxb && cz >= cza && cz <= czb
                    }
                    _ => false,
                }
            }).unwrap_or(false)
        };
        if linked {
            *counts.entry("actors").or_insert(0) += 1;
            entries.push(serde_json::json!({
                "key_hex": hex_encode(key),
                "value_base64": general_purpose::STANDARD.encode(value),
            }));
        }
    }

    if entries.is_empty() {
        println!("  未找到区块数据，导出空文件");
    }

    // origin: 导出矩形的最小方块坐标（用于定点导入的偏移基准）
    let export = serde_json::json!({
        "total": entries.len(),
        "origin": { "x": cxa * 16, "z": cza * 16 },
        "chunks": chunk_list,
        "entries": entries,
    });

    let json_str = serde_json::to_string_pretty(&export)
        .unwrap_or_else(|e| { eprintln!("错误: JSON 序列化失败: {}", e); std::process::exit(1); });
    std::fs::write(output_file, &json_str)
        .unwrap_or_else(|e| { eprintln!("错误: 写入文件失败: {}", e); std::process::exit(1); });

    print!("  已导出 {} 个区块 (区块坐标 {}/{} ~ {}/{})", current, cxa, cza, cxb, czb);
    for (cat, n) in &counts { print!(", {}: {}", cat, n); }
    println!("  → {}", output_file);
}

#[derive(Default)]
struct ImportStats {
    chunks: usize,
    by_tag: std::collections::BTreeMap<u8, usize>,
    actors: usize,
    digp: usize,
    digp_dropped: usize,      // 值无法重建的 digp（未写入）
    digp_refs_dropped: usize, // 重建时丢弃的未知 actor 引用
    players_dropped: usize,   // 0x32 中丢弃的玩家实体
    skipped: usize,
    overflow: usize,          // 坐标溢出跳过的键
    unchanged: usize,         // 值未变化(保持原值)
    parse_failed: usize,      // NBT 解析失败的 actor（跳过）
}

fn cmd_import_chunks_inner(db: &mut DB, input_file: &str, skip_existing: bool, to: Option<(i32, i32)>, dim_override: Option<u8>, dry_run: bool) {
    let json_str = std::fs::read_to_string(input_file)
        .unwrap_or_else(|e| { eprintln!("错误: 读取文件失败: {}", e); std::process::exit(1); });
    let data: serde_json::Value = serde_json::from_str(&json_str)
        .unwrap_or_else(|e| { eprintln!("错误: JSON 解析失败: {}", e); std::process::exit(1); });

    let entries = match data["entries"].as_array() {
        Some(arr) => arr,
        None => { eprintln!("错误: JSON 格式无效，缺少 entries 字段"); std::process::exit(1); }
    };

    // ── 平移参数: origin → (dx_chunks, dz_chunks) ──
    let reloc = to.map(|(bx, bz)| {
        let origin_cx: (i32, i32) = match (data["origin"]["x"].as_i64(), data["origin"]["z"].as_i64()) {
            (Some(x), Some(z)) => (x.div_euclid(16) as i32, z.div_euclid(16) as i32),
            _ => {
                // 旧文件无 origin 字段: 以文件内最小区块坐标为基准
                let mut min_cx = i32::MAX; let mut min_cz = i32::MAX; let mut found = false;
                for entry in entries {
                    if let Some(k) = entry["key_hex"].as_str().and_then(|h| hex_decode(h).ok()) {
                        if let Some((cx, cz, _, _, _)) = parse_chunk_key(&k) {
                            min_cx = min_cx.min(cx); min_cz = min_cz.min(cz); found = true;
                        }
                    }
                }
                if !found {
                    eprintln!("错误: 文件中无区块数据，无法定点导入"); std::process::exit(1);
                }
                println!("  文件无 origin 字段，以最小区块坐标 ({}, {}) 为基准", min_cx, min_cz);
                (min_cx, min_cz)
            }
        };
        let (to_cx, to_cz) = (bx.div_euclid(16), bz.div_euclid(16));
        let (dx, dz) = (to_cx - origin_cx.0, to_cz - origin_cx.1);
        println!("  定点平移: 基准区块 ({}, {}) → 目标区块 ({}, {})  [偏移 ({}, {}) 区块 = ({}, {}) 方块]",
            origin_cx.0, origin_cx.1, to_cx, to_cz, dx, dz, dx * 16, dz * 16);
        (dx, dz)
    });

    // ── 同位置导入: 所有条目原样写入（含 digp/actorprefix） ──
    if reloc.is_none() {
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for entry in entries {
            let key_hex = entry["key_hex"].as_str()
                .unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
            let value_b64 = entry["value_base64"].as_str()
                .unwrap_or_else(|| { eprintln!("错误: 条目缺少 value_base64"); std::process::exit(1); });
            let key = hex_decode(key_hex)
                .unwrap_or_else(|e| { eprintln!("错误: key 解析失败: {}", e); std::process::exit(1); });
            let value = general_purpose::STANDARD.decode(value_b64)
                .unwrap_or_else(|e| { eprintln!("错误: base64 解码失败: {}", e); std::process::exit(1); });
            if skip_existing && db.get(&key).is_some() {
                skipped += 1;
                continue;
            }
            if dry_run { imported += 1; continue; }
            db.put(&key, &value)
                .unwrap_or_else(|e| { eprintln!("错误: DB 写入失败: {}", e); std::process::exit(1); });
            imported += 1;
        }
        let total = data["total"].as_u64().unwrap_or(0) as usize;
        println!("  导入完成: 总数 {}, 已导入 {}, 已跳过 {}", total, imported, skipped);
        if dry_run { println!("  (dry-run 预览，未写入任何数据)"); }
        return;
    }

    let (dx, dz) = reloc.unwrap_or((0, 0));

    // ── Pass 0 (仅 reloc): 清理目标区块的旧实体 ──
    // 目标区块的 digp 将被覆盖; 旧 digp 引用的 actorprefix 若保留会成为孤儿,
    // BDS 对孤儿的处理可能损坏数据(实测: digp 值被写成无效 UTF-8) → 导入前主动删除
    let mut cleared_old_actors = 0usize;
    if reloc.is_some() {
        let mut target_digp_keys: Vec<Vec<u8>> = Vec::new();
        for entry in entries {
            let key_hex = entry["key_hex"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
            let key = match hex_decode(key_hex) { Ok(k) => k, Err(_) => continue };
            if !key.starts_with(b"digp") { continue; }
            if let Some(nk) = relocate_digp_key(&key, dx, dz, dim_override) {
                target_digp_keys.push(nk);
            }
        }
        for dk in &target_digp_keys {
            let old_val = match db.get(dk) { Some(v) => v, None => continue };
            for chunk in old_val.chunks_exact(8) {
                if let Ok(k) = chunk.try_into() {
                    let apk = build_actorprefix_key(k);
                    if db.get(&apk).is_some() {
                        cleared_old_actors += 1;
                        if !dry_run {
                            let _ = db.delete(&apk);
                        }
                    }
                }
            }
        }
        if cleared_old_actors > 0 {
            println!("  {}目标区域旧实体 {} 个{}",
                if dry_run { "将清理" } else { "已清理" },
                cleared_old_actors,
                if dry_run { " (dry-run 未执行)" } else { "" });
        }
    }

    let mut stats = ImportStats::default();
    let mut written: Vec<Vec<u8>> = Vec::new();  // 验证用

    // ── Pass 1: actorprefix 实体（重生成 UniqueID + 新键） ──
    let mut uid_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut key_map: std::collections::HashMap<[u8; 8], [u8; 8]> = std::collections::HashMap::new();
    let mut uid_gen = UidGen::new();
    const POS_FIELDS: &[&str] = &["Pos", "HomePos", "BedPosition", "SleepingPos",
        "LastSleepPos", "RestingPos", "LastRestingPos", "PatrolTarget"];

    let reloc_actors = reloc.is_some();
    if reloc_actors {
        for entry in entries {
            let key_hex = entry["key_hex"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
            let value_b64 = entry["value_base64"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 value_base64"); std::process::exit(1); });
            let key = hex_decode(key_hex).unwrap_or_else(|e| { eprintln!("错误: key 解析失败: {}", e); std::process::exit(1); });
            if !key.starts_with(b"actorprefix") || key.len() != 19 { continue; }
            let value = general_purpose::STANDARD.decode(value_b64).unwrap_or_else(|e| { eprintln!("错误: base64 解码失败: {}", e); std::process::exit(1); });
            let old_key: [u8; 8] = key[11..19].try_into().unwrap();
            let old_uid = decode_actor_storage_key(&old_key);

            let new_uid = *uid_map.entry(old_uid).or_insert_with(|| uid_gen.next(db));
            let new_key = build_actorprefix_key(encode_actor_storage_key(new_uid));

            // 值改写: 位置字段 + UniqueID (+ 强制维度时同步 DimensionId)
            let (kind, data) = unpack_value(&value);
            let (mut ct, _) = match CompoundTag::from_binary_nbt(&data, true) {
                Ok(v) => v,
                Err(_) => { stats.parse_failed += 1; continue; }
            };
            for f in POS_FIELDS {
                if let Some(t) = ct.get_mut(f) { shift_pos_list_tag(t, dx * 16, dz * 16); }
            }
            ct.set("UniqueID", Tag::Long(new_uid));
            sync_actor_storage_key(&mut ct, &encode_actor_storage_key(new_uid));
            if let Some(d) = dim_override { ct.set("DimensionId", Tag::Int(d as i32)); }
            let bytes = ct.to_binary_nbt(true, false);
            let new_value = repack_value(kind, &bytes);

            if skip_existing && db.get(&new_key).is_some() { stats.skipped += 1; continue; }
            // 登记映射供 Pass 3 重建 digp; 解析/跳过失败的实体不产生悬空引用
            key_map.insert(old_key, encode_actor_storage_key(new_uid));
            if dry_run { stats.actors += 1; continue; }
            db.put(&new_key, &new_value).unwrap_or_else(|e| { eprintln!("错误: DB 写入失败: {}", e); std::process::exit(1); });
            stats.actors += 1;
            written.push(new_key);
        }
    }

    // ── Pass 2: 区块键（key 平移 + 值按类型改写） ──
    for entry in entries {
        let key_hex = entry["key_hex"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
        let value_b64 = entry["value_base64"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 value_base64"); std::process::exit(1); });
        let key = hex_decode(key_hex).unwrap_or_else(|e| { eprintln!("错误: key 解析失败: {}", e); std::process::exit(1); });
        let value = general_purpose::STANDARD.decode(value_b64).unwrap_or_else(|e| { eprintln!("错误: base64 解码失败: {}", e); std::process::exit(1); });

        if key.starts_with(b"actorprefix") || key.starts_with(b"digp") { continue; }

        let parsed = parse_chunk_key(&key);
        let new_key = if reloc.is_some() {
            match parsed {
                Some(_) => match relocate_chunk_key(&key, dx, dz, dim_override) {
                    Some(k) => k,
                    None => { stats.overflow += 1; continue; }
                },
                None => key.clone(),  // 非区块键（理论上导出不含）: 原样保留
            }
        } else {
            key.clone()
        };

        let new_value = if reloc.is_some() {
            match parsed.map(|p| p.3) {
                Some(0x31) => match shift_block_entity_value(&value, dx * 16, dz * 16) {
                    Some(v) => v,
                    None => { stats.unchanged += 1; value.clone() }
                },
                Some(0x33) => match shift_pending_ticks_value(&value, dx * 16, dz * 16) {
                    Some(v) => v,
                    None => { stats.unchanged += 1; value.clone() }
                },
                Some(0x32) => {
                    let (v, players) = shift_entity_value(&value, dx * 16, dz * 16, &mut uid_map, &mut uid_gen, db);
                    stats.players_dropped += players;
                    match v { Some(v) => v, None => { stats.unchanged += 1; value.clone() } }
                }
                _ => value.clone(),  // 0x2F/0x34/0x36/0x38 等: 内容为局部数据, 原样
            }
        } else {
            value.clone()
        };

        if skip_existing && db.get(&new_key).is_some() { stats.skipped += 1; continue; }
        if dry_run { stats.chunks += 1; continue; }
        db.put(&new_key, &new_value).unwrap_or_else(|e| { eprintln!("错误: DB 写入失败: {}", e); std::process::exit(1); });
        stats.chunks += 1;
        if let Some((_, _, _, tag, _)) = parsed { *stats.by_tag.entry(tag).or_insert(0) += 1; }
        written.push(new_key);
    }

    // ── Pass 3: digp（key 平移 + 值用新存储键重建） ──
    if reloc.is_some() {
        let (dx, dz) = reloc.unwrap();
        for entry in entries {
            let key_hex = entry["key_hex"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 key_hex"); std::process::exit(1); });
            let value_b64 = entry["value_base64"].as_str().unwrap_or_else(|| { eprintln!("错误: 条目缺少 value_base64"); std::process::exit(1); });
            let key = hex_decode(key_hex).unwrap_or_else(|e| { eprintln!("错误: key 解析失败: {}", e); std::process::exit(1); });
            if !key.starts_with(b"digp") { continue; }
            let value = general_purpose::STANDARD.decode(value_b64).unwrap_or_else(|e| { eprintln!("错误: base64 解码失败: {}", e); std::process::exit(1); });

            let new_key = match relocate_digp_key(&key, dx, dz, dim_override) {
                Some(k) => k,
                None => { stats.overflow += 1; continue; }
            };

            // 值 = 8 字节存储键拼接; 逐个映射为新键, 未知引用丢弃
            if value.len() % 8 != 0 {
                stats.digp_dropped += 1;
                eprintln!("  警告: digp 值长度 {} 非 8 的倍数，无法重建，跳过写入", value.len());
                continue;
            }
            let mut new_value = Vec::with_capacity(value.len());
            for chunk in value.chunks_exact(8) {
                let k: [u8; 8] = chunk.try_into().unwrap();
                match key_map.get(&k) {
                    Some(nk) => new_value.extend_from_slice(nk),
                    None => stats.digp_refs_dropped += 1,
                }
            }

            if skip_existing && db.get(&new_key).is_some() { stats.skipped += 1; continue; }
            if dry_run { stats.digp += 1; continue; }
            db.put(&new_key, &new_value).unwrap_or_else(|e| { eprintln!("错误: DB 写入失败: {}", e); std::process::exit(1); });
            stats.digp += 1;
            written.push(new_key);
        }
    }

    // ── 验证 ──
    let verified = if dry_run { written.len() } else {
        written.iter().filter(|k| db.get(k).is_some()).count()
    };
    let total = data["total"].as_u64().unwrap_or(0) as usize;

    println!("\n═══ 区块导入报告 ═══");
    println!("  文件总数: {} 条", total);
    if reloc.is_some() {
        let mut tag_desc: Vec<String> = stats.by_tag.iter()
            .map(|(t, n)| format!("0x{:02x}:{}", t, n)).collect();
        tag_desc.sort();
        println!("  区块键: {} 条 [{}]{}", stats.chunks, tag_desc.join(" "),
            if stats.unchanged > 0 { format!(" ({} 条值未变化)", stats.unchanged) } else { String::new() });
        println!("  实体 (actorprefix): {} 个{}", stats.actors,
            if stats.players_dropped > 0 { format!(" (丢弃玩家实体 {})", stats.players_dropped) } else { String::new() });
        println!("  实体摘要 (digp): {} 个{}", stats.digp,
            if stats.digp_refs_dropped > 0 { format!(" (丢弃未知引用 {})", stats.digp_refs_dropped) } else { String::new() });
        if stats.digp_dropped > 0 { println!("  警告: {} 个 digp 因格式异常未写入", stats.digp_dropped); }
        if stats.overflow > 0 { println!("  警告: {} 个键坐标溢出已跳过", stats.overflow); }
    } else {
        println!("  导入键值: {} 条", stats.chunks + stats.actors + stats.digp);
    }
    if stats.skipped > 0 { println!("  跳过已存在: {}", stats.skipped); }
    if dry_run {
        println!("\n  (dry-run 预览，未写入任何数据)");
    } else {
        println!("  验证: {} / {} 条写入确认", verified, written.len());
    }
}

/// Known Bedrock chunk key discriminator bytes at position 8.
/// Chunk keys: [cx:4][cz:4][discriminator:1][optional_data]
fn is_chunk_key(key: &[u8]) -> bool {
    key.len() >= 9
        && !key.starts_with(b"~")
        && !key.starts_with(b"player_")
        && !key.starts_with(b"actorprefix")
        && !key.starts_with(b"digp")
}

/// Extract the dimension id from a chunk key.
/// Overworld (dim=0): key = [cx:4][cz:4][tag:1][data]  (9-10 bytes, no dim field)
/// Nether/End  (dim=1/2): key = [cx:4][cz:4][dim:4][tag:1][data]  (13-14 bytes, dim at [8..12])
fn extract_dim_from_key(key: &[u8]) -> u8 {
    if key.len() >= 13 {
        let dim = i32::from_le_bytes(key[8..12].try_into().unwrap());
        if dim == 1 || dim == 2 { return dim as u8; }
    }
    0
}

/// Collect all chunk-related DB keys with their chunk coordinates and dimension.
/// Returns (key, cx, cz, dim).
fn collect_all_chunk_keys_from_db(db: &mut DB) -> Vec<(Vec<u8>, i32, i32, u8)> {
    let mut results = Vec::new();
    let mut iter = match db.new_iter() {
        Ok(it) => it,
        Err(_) => return results,
    };
    iter.seek_to_first();
    while let Some((key, _value)) = iter.next() {
        if is_chunk_key(&key) {
            let cx = i32::from_le_bytes(key[0..4].try_into().unwrap());
            let cz = i32::from_le_bytes(key[4..8].try_into().unwrap());
            let dim = extract_dim_from_key(&key);
            results.push((key, cx, cz, dim));
        }
    }
    results
}

// ── Chunk relocation (定点平移) ──

/// Parse a chunk key into (cx, cz, dim, tag, data_offset).
/// Chunk keys: [cx:4][cz:4][dim:4 if dim!=0][tag:1][data]
fn parse_chunk_key(key: &[u8]) -> Option<(i32, i32, u8, u8, usize)> {
    if key.len() < 9 { return None; }
    let cx = i32::from_le_bytes(key[0..4].try_into().unwrap());
    let cz = i32::from_le_bytes(key[4..8].try_into().unwrap());
    let (dim, tag_pos) = if key.len() >= 13 {
        let d = i32::from_le_bytes(key[8..12].try_into().unwrap());
        if d == 1 || d == 2 { (d as u8, 12usize) } else { (0u8, 8usize) }
    } else { (0u8, 8usize) };
    Some((cx, cz, dim, key[tag_pos], tag_pos + 1))
}

/// Rebuild a chunk key shifted by (dx_chunks, dz_chunks), optionally forcing a dimension.
/// 0x34 (BlockExtraData) additionally carries block coords in the key: [bx:4][by:4][bz:4].
/// Returns None on non-chunk key or coordinate overflow (caller skips with warning).
fn relocate_chunk_key(key: &[u8], dx_chunks: i32, dz_chunks: i32, dim_override: Option<u8>) -> Option<Vec<u8>> {
    let (cx, cz, dim, tag, data_off) = parse_chunk_key(key)?;
    let new_cx = cx.checked_add(dx_chunks)?;
    let new_cz = cz.checked_add(dz_chunks)?;
    let new_dim = dim_override.unwrap_or(dim);
    let mut out = Vec::with_capacity(key.len());
    out.extend_from_slice(&new_cx.to_le_bytes());
    out.extend_from_slice(&new_cz.to_le_bytes());
    if new_dim != 0 { out.extend_from_slice(&new_dim.to_le_bytes()); }
    out.push(tag);
    if tag == 0x34 && key.len() >= data_off + 12 {
        let bx = i32::from_le_bytes(key[data_off..data_off + 4].try_into().unwrap())
            .checked_add(dx_chunks.checked_mul(16)?)?;
        let by = i32::from_le_bytes(key[data_off + 4..data_off + 8].try_into().unwrap());
        let bz = i32::from_le_bytes(key[data_off + 8..data_off + 12].try_into().unwrap())
            .checked_add(dz_chunks.checked_mul(16)?)?;
        out.extend_from_slice(&bx.to_le_bytes());
        out.extend_from_slice(&by.to_le_bytes());
        out.extend_from_slice(&bz.to_le_bytes());
        out.extend_from_slice(&key[data_off + 12..]);
    } else {
        out.extend_from_slice(&key[data_off..]);
    }
    Some(out)
}

/// Parse a digp (actor digest) key into (cx, cz, dim, value_offset).
/// digp keys: "digp" + [x:4][z:4] (+[dim:4] only for dim != 0).
fn parse_digp_key(key: &[u8]) -> Option<(i32, i32, u8, usize)> {
    if !key.starts_with(b"digp") { return None; }
    match key.len() {
        12 => Some((i32::from_le_bytes(key[4..8].try_into().unwrap()),
                    i32::from_le_bytes(key[8..12].try_into().unwrap()), 0u8, 12usize)),
        16 => Some((i32::from_le_bytes(key[4..8].try_into().unwrap()),
                    i32::from_le_bytes(key[8..12].try_into().unwrap()),
                    i32::from_le_bytes(key[12..16].try_into().unwrap()) as u8, 16usize)),
        _ => None,
    }
}

fn relocate_digp_key(key: &[u8], dx_chunks: i32, dz_chunks: i32, dim_override: Option<u8>) -> Option<Vec<u8>> {
    let (cx, cz, dim, _) = parse_digp_key(key)?;
    let new_cx = cx.checked_add(dx_chunks)?;
    let new_cz = cz.checked_add(dz_chunks)?;
    let new_dim = dim_override.unwrap_or(dim);
    let mut out = Vec::with_capacity(key.len());
    out.extend_from_slice(b"digp");
    out.extend_from_slice(&new_cx.to_le_bytes());
    out.extend_from_slice(&new_cz.to_le_bytes());
    if new_dim != 0 { out.extend_from_slice(&new_dim.to_le_bytes()); }
    Some(out)
}

/// Actor storage key (8 bytes BE: [neg_hi32(uid)][lo32(uid)]) ←→ UniqueID.
fn decode_actor_storage_key(k: &[u8]) -> i64 {
    let hi = u32::from_be_bytes(k[0..4].try_into().unwrap());
    let lo = u32::from_be_bytes(k[4..8].try_into().unwrap());
    ((hi.wrapping_neg() as i64) << 32) | (lo as i64)
}

fn encode_actor_storage_key(uid: i64) -> [u8; 8] {
    let u = uid as u64;
    let hi = (u >> 32) as u32;
    let lo = u as u32;
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&hi.wrapping_neg().to_be_bytes());
    out[4..8].copy_from_slice(&lo.to_be_bytes());
    out
}

/// actorprefix 键 = "actorprefix"(11 字节) + 8 字节存储键(BE)。
/// 注意: "actorprefix" 以 'x'(0x78) 结尾, 并非额外的标记字节。
fn build_actorprefix_key(storage_key: [u8; 8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(19);
    k.extend_from_slice(b"actorprefix");
    k.extend_from_slice(&storage_key);
    k
}
/// 把实体的 internalComponents.EntityStorageKeyComponent.StorageKey 同步为新的存储键。
/// BDS 1.26 保存 actorprefix 时以该字段(而非仅 UniqueID)为准; Amulet-Core
/// put_chunk_data 每次写入实体都会重写它。键字节全部 <0x80, 可直接作为 ASCII 字符串。
fn sync_actor_storage_key(ct: &mut CompoundTag, storage: &[u8; 8]) {
    let sk = std::str::from_utf8(storage).expect("storage key bytes must be <0x80").to_string();
    let mut esc_comp = std::collections::HashMap::new();
    esc_comp.insert("StorageKey".into(), Tag::String(sk));
    if let Some(Tag::Compound(inner)) = ct.get_mut("internalComponents") {
        inner.insert("EntityStorageKeyComponent".into(), Tag::Compound(esc_comp));
    } else {
        let mut esc = std::collections::HashMap::new();
        esc.insert("EntityStorageKeyComponent".into(), Tag::Compound(esc_comp));
        ct.set("internalComponents", Tag::Compound(esc));
    }
}


/// Generate fresh actor UniqueIDs accepted & persisted by BDS 1.26.x.
/// 实测结论(本机 BDS 1.26.45.1): actorprefix 存储键 8 字节若含 >=0x80 的字节,
/// BDS 保存时会把记录当作“外来实体”处理(不写回 digp / 用 UTF-8 替换符改写), 重启后实体消失。
/// 实测存活的键形如 00 00 10 01..2a / 00 00 01 01..2a (全部字节 <0x80),
/// 即 key-hi = 0x1001..=0x102A, key-lo = 0x101..=0x12A; uid = -(hi<<32)+lo, 与 BDS 自身形态一致。
/// 在该区间内递增并做 DB 碰撞检查。
/// 可分配槽位 = 42(hi) × 42(lo) = 1764, 全部占用后继续导入会无法分配。
struct UidGen { counter: u32 }
impl UidGen {
    fn new() -> Self {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u128).unwrap_or(0);
        UidGen { counter: (t as u32) }
    }
    fn next(&mut self, db: &mut DB) -> i64 {
        for _ in 0..(42 * 42 * 4) {
            self.counter = self.counter.wrapping_add(1);
            let hi = 0x1001 + (self.counter % 42);   // 0x1001..=0x102A
            let lo = 0x101 + ((self.counter / 42) % 42); // 0x101..=0x12A
            let uid = -((hi as i64) << 32) + (lo as i64);
            let key = build_actorprefix_key(encode_actor_storage_key(uid));
            if db.get(&key).is_none() { return uid; }
        }
        eprintln!("错误: 可用的 BDS 兼容实体 UID 槽位已耗尽(存档中定点导入实体过多)");
        std::process::exit(1);
    }
}

// ── NBT 坐标改写（自适应压缩：zlib / 裸 NBT） ──

enum PayloadKind { Raw, Zlib }

fn unpack_value(value: &[u8]) -> (PayloadKind, Vec<u8>) {
    if value.len() > 2 && value[0] == 0x78 {
        if let Ok(d) = decompress_to_vec_zlib(value) { return (PayloadKind::Zlib, d); }
    }
    (PayloadKind::Raw, value.to_vec())
}

fn repack_value(kind: PayloadKind, data: &[u8]) -> Vec<u8> {
    match kind {
        PayloadKind::Zlib => compress_to_vec_zlib(data, CompressionLevel::DefaultLevel as u8),
        PayloadKind::Raw => data.to_vec(),
    }
}

/// 平移 compound 内 x/z 整数字段（方块实体、计划刻元素）。
fn shift_int_xyz(ct: &mut CompoundTag, dx: i32, dz: i32) -> bool {
    let mut changed = false;
    if let Some(v) = ct.get_mut("x") {
        if let Tag::Int(n) = v {
            if let Some(nn) = n.checked_add(dx) { *v = Tag::Int(nn); changed = true; }
        }
    }
    if let Some(v) = ct.get_mut("z") {
        if let Tag::Int(n) = v {
            if let Some(nn) = n.checked_add(dz) { *v = Tag::Int(nn); changed = true; }
        }
    }
    changed
}

/// 平移位置列表字段：List[3] 的 [0]=x, [2]=z（支持 Float/Double/Int）。
fn shift_pos_list_tag(tag: &mut Tag, dx: i32, dz: i32) -> bool {
    if let Tag::List(lv) = tag {
        if lv.elements.len() < 3 { return false; }
        let mut changed = false;
        let mut add = |idx: usize, delta: i32| -> bool {
            match &mut lv.elements[idx] {
                Tag::Float(f) => { *f += delta as f32; true }
                Tag::Double(d) => { *d += delta as f64; true }
                Tag::Int(i) => { *i += delta; true }
                _ => false,
            }
        };
        changed |= add(0, dx);
        changed |= add(2, dz);
        changed
    } else { false }
}

/// 0x31 方块实体值：逐个 NBT root 平移 x/z；失败返回 None（保持原值）。
fn shift_block_entity_value(value: &[u8], dx: i32, dz: i32) -> Option<Vec<u8>> {
    let (kind, data) = unpack_value(value);
    let mut out = Vec::with_capacity(data.len());
    let mut off = 0;
    let mut changed_any = false;
    while off < data.len() {
        let (mut ct, consumed) = CompoundTag::from_binary_nbt(&data[off..], true).ok()?;
        if consumed == 0 { break; }
        changed_any |= shift_int_xyz(&mut ct, dx, dz);
        out.extend_from_slice(&ct.to_binary_nbt(true, false));
        off += consumed;
    }
    if !changed_any { return None; }
    Some(repack_value(kind, &out))
}

/// 平移 HashMap 形式 compound 的 x/z 整数字段（计划刻元素等）。
fn shift_int_xyz_map(map: &mut std::collections::HashMap<String, Tag>, dx: i32, dz: i32) -> bool {
    let mut changed = false;
    for (k, delta) in [("x", dx), ("z", dz)] {
        if let Some(v) = map.get_mut(k) {
            if let Tag::Int(n) = v {
                if let Some(nn) = n.checked_add(delta) { *v = Tag::Int(nn); changed = true; }
            }
        }
    }
    changed
}

/// 0x33 计划刻值：root compound 的 tickList 数组，每项平移 x/z。
fn shift_pending_ticks_value(value: &[u8], dx: i32, dz: i32) -> Option<Vec<u8>> {
    let (kind, data) = unpack_value(value);
    let (mut ct, _) = CompoundTag::from_binary_nbt(&data, true).ok()?;
    let mut changed_any = false;
    if let Some(lv) = ct.get_mut("tickList").and_then(|t| {
        if let Tag::List(lv) = t { Some(lv) } else { None }
    }) {
        for elem in lv.elements.iter_mut() {
            if let Tag::Compound(map) = elem {
                changed_any |= shift_int_xyz_map(map, dx, dz);
            }
        }
    }
    if !changed_any { return None; }
    let bytes = ct.to_binary_nbt(true, false);
    Some(repack_value(kind, &bytes))
}

/// 0x32 实体值：平移位置字段 + 重生成 UniqueID；玩家实体不复制（丢弃）。
fn shift_entity_value(
    value: &[u8], dx: i32, dz: i32,
    uid_map: &mut std::collections::HashMap<i64, i64>,
    uid_gen: &mut UidGen, db: &mut DB,
) -> (Option<Vec<u8>>, usize) {
    let (kind, data) = unpack_value(value);
    let mut out = Vec::with_capacity(data.len());
    let mut off = 0;
    let mut changed_any = false;
    let mut players_dropped = 0;
    const POS_FIELDS: &[&str] = &["Pos", "HomePos", "BedPosition", "SleepingPos",
        "LastSleepPos", "RestingPos", "LastRestingPos", "PatrolTarget"];
    while off < data.len() {
        let (mut ct, consumed) = match CompoundTag::from_binary_nbt(&data[off..], true) {
            Ok(v) => v,
            Err(_) => break,  // 剩余字节无法解析则截断
        };
        if consumed == 0 { break; }
        if ct.get("identifier").and_then(|t| t.as_str()) == Some("minecraft:player") {
            players_dropped += 1;
            off += consumed;
            continue;
        }
        for f in POS_FIELDS {
            if let Some(t) = ct.get_mut(f) { changed_any |= shift_pos_list_tag(t, dx, dz); }
        }
        if let Some(uid) = ct.get("UniqueID").and_then(|t| if let Tag::Long(v) = t { Some(*v) } else { None }) {
            let new_uid = *uid_map.entry(uid).or_insert_with(|| uid_gen.next(db));
            ct.set("UniqueID", Tag::Long(new_uid));
            changed_any = true;
        }
        out.extend_from_slice(&ct.to_binary_nbt(true, false));
        off += consumed;
    }
    (if changed_any { Some(repack_value(kind, &out)) } else { None }, players_dropped)
}

// ── Chunk deletion ──

fn sort_pair(a: i32, b: i32) -> (i32, i32) {
    if a <= b { (a, b) } else { (b, a) }
}

fn cmd_delete_chunks(db: &mut DB, bx1: i32, bz1: i32, bx2: i32, bz2: i32, dim_id: u8) {
    let (cxa, cxb) = sort_pair(bx1.div_euclid(16), bx2.div_euclid(16));
    let (cza, czb) = sort_pair(bz1.div_euclid(16), bz2.div_euclid(16));
    let total_chunks = (cxb - cxa + 1) * (czb - cza + 1);

    let mut deleted = 0usize;
    let mut non_empty = 0usize;

    for cx in cxa..=cxb {
        for cz in cza..=czb {
            let keys = collect_chunk_keys(db, cx, cz);
            if keys.is_empty() { continue; }
            // Filter keys by dimension before deleting
            let to_delete: Vec<Vec<u8>> = keys.into_iter()
                .filter(|(key, _)| extract_dim_from_key(key) == dim_id)
                .map(|(key, _)| key)
                .collect();
            if to_delete.is_empty() { continue; }
            non_empty += 1;
            for key in &to_delete {
                let _ = db.delete(key);
                deleted += 1;
            }
        }
    }

    // Post-delete verification (within same write-mode session, MemTable is current)
    let mut remaining = 0usize;
    let mut remaining_chunks = 0usize;
    for cx in cxa..=cxb {
        for cz in cza..=czb {
            let keys = collect_chunk_keys(db, cx, cz);
            if keys.is_empty() { continue; }
            let dim_keys: Vec<_> = keys.into_iter()
                .filter(|(key, _)| extract_dim_from_key(key) == dim_id)
                .collect();
            if dim_keys.is_empty() { continue; }
            remaining_chunks += 1;
            remaining += dim_keys.len();
        }
    }

    println!("  区块删除完成: 范围 ({}, {}) ~ ({}, {}), 共 {} 区块",
        cxa, cza, cxb, czb, total_chunks);
    println!("  删除 {} 条键值, 涉及 {} 区块, {} 区块无数据",
        deleted, non_empty, total_chunks - non_empty as i32);
    if remaining == 0 && non_empty > 0 {
        println!("  验证: 所有区块数据已成功删除");
    } else if remaining > 0 {
        println!("  验证: {} 条键值在 {} 个区块中未清除 (仅 WAL 写入, 待 compaction)", remaining, remaining_chunks);
    }
}

// ── Batch chunk deletion ──

#[derive(Clone, Debug)]
struct ChunkRect {
    dim_id: u8,
    cx1: i32, cz1: i32,
    cx2: i32, cz2: i32,
}

fn parse_dimension(val: &serde_json::Value) -> Result<u8, String> {
    match val {
        serde_json::Value::Number(n) => {
            let d = n.as_i64().ok_or_else(|| "维度值不是整数".to_string())?;
            match d { 0 | 1 | 2 => Ok(d as u8), _ => Err(format!("维度值无效: {}", d)) }
        }
        serde_json::Value::String(s) => match s.to_lowercase().as_str() {
            "overworld" => Ok(0),
            "nether" => Ok(1),
            "end" => Ok(2),
            _ => Err(format!("未知维度名称: '{}'", s)),
        },
        _ => Err("维度必须是整数或字符串".to_string()),
    }
}

fn parse_and_validate_batch_file(input_file: &str) -> Result<Vec<ChunkRect>, String> {
    let json_str = std::fs::read_to_string(input_file)
        .map_err(|e| format!("读取文件失败: {}", e))?;
    let data: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("JSON 解析失败: {}", e))?;

    let regions = data["regions"].as_array()
        .ok_or_else(|| "JSON 缺少 regions 数组字段".to_string())?;

    if regions.is_empty() {
        return Err("regions 数组不能为空".to_string());
    }

    if let Some(total) = data["total"].as_u64() {
        if total as usize != regions.len() {
            return Err(format!("total ({}) 与 regions 数量 ({}) 不匹配", total, regions.len()));
        }
    }

    let mut chunk_rects = Vec::with_capacity(regions.len());

    for (i, region) in regions.iter().enumerate() {
        if !region.is_object() {
            return Err(format!("regions[{}] 必须是对象", i));
        }

        let dim_val = region.get("dimension")
            .ok_or_else(|| format!("regions[{}] 缺少 dimension 字段", i))?;
        let dim_id = parse_dimension(dim_val)?;

        let x1 = region["x1"].as_i64()
            .ok_or_else(|| format!("regions[{}] x1 格式无效", i))? as i32;
        let z1 = region["z1"].as_i64()
            .ok_or_else(|| format!("regions[{}] z1 格式无效", i))? as i32;
        let x2 = region["x2"].as_i64()
            .ok_or_else(|| format!("regions[{}] x2 格式无效", i))? as i32;
        let z2 = region["z2"].as_i64()
            .ok_or_else(|| format!("regions[{}] z2 格式无效", i))? as i32;

        let (cx1, cx2_chk) = sort_pair(x1.div_euclid(16), x2.div_euclid(16));
        let (cz1, cz2_chk) = sort_pair(z1.div_euclid(16), z2.div_euclid(16));

        chunk_rects.push(ChunkRect { dim_id, cx1, cz1, cx2: cx2_chk, cz2: cz2_chk });
    }

    Ok(chunk_rects)
}

/// Check if a chunk coordinate with given dimension falls within any of the rectangles.
fn is_chunk_in_rects(cx: i32, cz: i32, dim: u8, rects: &[ChunkRect]) -> bool {
    rects.iter().any(|r| r.dim_id == dim && cx >= r.cx1 && cx <= r.cx2 && cz >= r.cz1 && cz <= r.cz2)
}
fn cmd_batch_delete_chunks(world_path: &str, input_file: &str, invert: bool) {
    // Phase 1: parse and validate JSON
    eprintln!("  正在解析批量文件...");
    let rects = match parse_and_validate_batch_file(input_file) {
        Ok(r) => r,
        Err(e) => { eprintln!("错误: 批量文件校验失败: {}", e); std::process::exit(1); }
    };
    println!("  解析完成: {} 个区域", rects.len());

    // Phase 2: no merge needed - use raw rects with linear scan
    let merged = &rects;

    // Phase 3: resolve DB path (BE: all data in db/)
    let db_path = resolve_db_path(world_path, 0);
    if !std::path::Path::new(&db_path).is_dir() || !std::path::Path::new(&format!("{}/CURRENT", db_path)).exists() {
        eprintln!("错误: LevelDB 目录不存在: {}", db_path);
        std::process::exit(1);
    }
    println!("\n  DB: {}", db_path);

    // Phase 4: collect all chunk keys (read-only)
    let mut opt_ro = mcpe_options(CompressionLevel::DefaultLevel as u8);
    opt_ro.read_only = true;
    let mut db_ro = match DB::open(&db_path, opt_ro) {
        Ok(d) => d,
        Err(e) => { eprintln!("错误: DB 打开失败: {}", e); std::process::exit(1); }
    };

    let all_keys = collect_all_chunk_keys_from_db(&mut db_ro);
    println!("    DB 扫描: {} 条键值", all_keys.len());
    drop(db_ro);

    if all_keys.is_empty() {
        println!("    DB 中无区块数据");
        std::process::exit(0);
    }

    // Phase 5: parallel filter
    let mode_label = if invert { "区域外" } else { "区域内" };
    eprintln!("    正在并行过滤 ({}模式)...", mode_label);

    let keys_to_delete: Vec<Vec<u8>> = if invert {
        all_keys.par_iter()
            .filter(|(_, cx, cz, dim)| !is_chunk_in_rects(*cx, *cz, *dim, merged))
            .map(|(key, _, _, _)| key.clone())
            .collect()
    } else {
        all_keys.par_iter()
            .filter(|(_, cx, cz, dim)| is_chunk_in_rects(*cx, *cz, *dim, merged))
            .map(|(key, _, _, _)| key.clone())
            .collect()
    };

    let collect_count = keys_to_delete.len();
    if collect_count == 0 {
        println!("    无待删除键值");
        std::process::exit(0);
    }

    // Phase 6: delete (write mode)
    eprintln!("    正在删除 {} 条键值...", collect_count);
    let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
    opt.reuse_logs = false;
    opt.reuse_manifest = false;
    opt.read_only = false;
    let mut db = match DB::open(&db_path, opt) {
        Ok(d) => d,
        Err(e) => { eprintln!("    错误: 无法以写模式打开 DB: {}", e); std::process::exit(1); }
    };

    // Phase 6: delete
    for key in &keys_to_delete {
        let _ = db.delete(key);
    }

    // Phase 7: verify within same write session (MemTable is current)
    eprintln!("    正在验证删除结果...");
    let still_there = keys_to_delete.iter().filter(|k| db.get(k).is_some()).count();
    let _ = db.flush();
    drop(db);

    // Count unique chunks for reporting
    let mut chunks_set: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    for key in &keys_to_delete {
        if key.len() >= 8 {
            let cx = i32::from_le_bytes(key[0..4].try_into().unwrap());
            let cz = i32::from_le_bytes(key[4..8].try_into().unwrap());
            chunks_set.insert((cx, cz));
        }
    }

    let unique_chunks = chunks_set.len();
    println!("    ✓ 删除 {} 条键值, 涉及 {} 区块", collect_count, unique_chunks);

    if still_there == 0 {
        println!("    ✓ 验证: 目标区域数据已全部删除（同会话确认）");
    } else {
        println!("    验证: {} 条键值仍存留（仅 WAL 写入，BDS 启动后生效）", still_there);
    }

    println!("\n═══ 批量删除报告 ═══");
    println!("  总计删除: {} 条键值", collect_count);
    println!("    {}: {} 区块, {} 键值", db_path, unique_chunks, collect_count);
}

// ── Main ──

fn main() {
    let cmd = match parse_args() {
        Ok(c) => c,
        Err(e) => { eprintln!("错误: {}", e); eprintln!("使用 --help 查看帮助"); std::process::exit(1); }
    };

    let (world_path, dim_id, _dim_name) = match &cmd {
        Command::QueryBlock { world_path, dim_id, dim_name, .. } => (world_path.as_str(), *dim_id, dim_name.as_str()),
        Command::ShowInfo { world_path } | Command::ListPlayers { world_path } | Command::ListActors { world_path } | Command::ShowPlayer { world_path, .. }
            | Command::WipeActors { world_path, .. } | Command::ExportActors { world_path, .. } | Command::ExportChunks { world_path, .. }
            | Command::ImportActors { world_path, .. } | Command::ImportChunks { world_path, .. }
            | Command::EntityDensity { world_path, .. } | Command::BatchDeleteChunks { world_path, .. } => (world_path.as_str(), 0u8, "overworld"),
        Command::DeleteChunks { world_path, dim_id, dim_name, .. } => (world_path.as_str(), *dim_id, dim_name.as_str()),
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

    let db_path = resolve_db_path(world_path, dim_id);
    if !Path::new(&db_path).is_dir() || !Path::new(&format!("{}/CURRENT", db_path)).exists() {
        eprintln!("错误: LevelDB 目录不存在: {}", db_path);
        std::process::exit(1);
    }
    println!("  DB: {}", db_path);

    // ─── Write commands: open DB in write mode directly ───
    match cmd {
        Command::ImportActors { ref input_file, skip_existing, .. } => {
            let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
            opt.read_only = false;
            opt.reuse_logs = false;
            opt.reuse_manifest = false;
            let mut db = match DB::open(&db_path, opt) {
                Ok(d) => d,
                Err(e) => { eprintln!("错误: DB 打开失败: {}", e); std::process::exit(1); }
            };
            cmd_import_actors(&mut db, input_file, skip_existing);
            let _ = db.flush();
            println!();
            std::process::exit(0);
        }
        Command::WipeActors { include_players, .. } => {
            // Phase 1: collect keys to delete from read-only DB
            let mut opt_ro = mcpe_options(CompressionLevel::DefaultLevel as u8);
            opt_ro.read_only = true;
            let mut db_ro = match DB::open(&db_path, opt_ro) {
                Ok(d) => d,
                Err(e) => { eprintln!("错误: DB 打开失败: {}", e); std::process::exit(1); }
            };

            let mut to_delete: Vec<Vec<u8>> = Vec::new();

            // Collect actorprefix entries
            let actors = scan_actor_keys(&mut db_ro);
            for (key, value) in &actors {
                let is_player = CompoundTag::from_binary_nbt(value, true).ok()
                    .and_then(|(tag, _)| tag.get("identifier").and_then(|t| t.as_str().map(|s| s.to_string())))
                    == Some("minecraft:player".to_string());

                if include_players || !is_player {
                    to_delete.push(key.clone());
                }
            }

            if include_players {
                // Collect player_*, ~* keys (全量扫描, 规避 fork seek 缺陷)
                for (key, _) in scan_all_player_keys(&mut db_ro) {
                    to_delete.push(key);
                }
            }

            let collected = to_delete.len();
            drop(db_ro);

            // Phase 2: open write-mode and delete
            let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
            opt.read_only = false;
            let mut db = match DB::open(&db_path, opt) {
                Ok(d) => d,
                Err(e) => { eprintln!("错误: 无法以写模式打开 DB: {}", e); std::process::exit(1); }
            };

            let actual_deleted = to_delete.iter().filter(|k| db.get(k).is_some()).count();
            for key in &to_delete {
                let _ = db.delete(key);
            }

            let _ = db.flush();
            if include_players {
                println!("  已擦除全部实体(含玩家数据): 收集 {} 条, 实际删除 {}", collected, actual_deleted);
            } else {
                println!("  已擦除非玩家实体: 收集 {} 条, 实际删除 {}", collected, actual_deleted);
            }
            println!();
            std::process::exit(0);
        }
        Command::ImportChunks { ref input_file, skip_existing, to, dim_override, dry_run, .. } => {
            let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
            opt.read_only = false;
            opt.reuse_logs = false;      // 禁用日志复用: 写入进新日志, 下次打开/BDS 启动时可靠回放
            opt.reuse_manifest = false;
            let mut db = match DB::open(&db_path, opt) {
                Ok(d) => d,
                Err(e) => { eprintln!("错误: DB 打开失败: {}", e); std::process::exit(1); }
            };
            cmd_import_chunks_inner(&mut db, input_file, skip_existing, to, dim_override, dry_run);
            if !dry_run {
                let _ = db.flush();  // 确保 WAL 落盘 (process::exit 会跳过 Drop)
            }
            println!();
            std::process::exit(0);
        }
        Command::DeleteChunks { bx1, bz1, bx2, bz2, dim_id, .. } => {
            let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
            opt.reuse_logs = false;
            opt.reuse_manifest = false;
            opt.read_only = false;
            let mut db = match DB::open(&db_path, opt) {
                Ok(d) => d,
                Err(e) => { eprintln!("错误: DB 打开失败: {}", e); std::process::exit(1); }
            };
            cmd_delete_chunks(&mut db, bx1, bz1, bx2, bz2, dim_id);
            let _ = db.flush();
            drop(db);
            println!();
            std::process::exit(0);
        }
        Command::BatchDeleteChunks { ref input_file, invert, .. } => {
            cmd_batch_delete_chunks(world_path, input_file, invert);
            println!();
            std::process::exit(0);
        }
        _ => {}
    }

    // ─── Read-only commands: open DB in read-only mode ───
    let mut opt = mcpe_options(CompressionLevel::DefaultLevel as u8);
    opt.read_only = true;
    let mut db = match DB::open(&db_path, opt) {
        Ok(d) => d,
        Err(e) => { eprintln!("  DB 打开失败: {}", e); std::process::exit(1); }
    };

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

        Command::ExportActors { ref output_file, no_players, .. } => {
            cmd_export_actors(&mut db, output_file, no_players);
        }

        Command::ExportChunks { ref output_file, bx1, bz1, bx2, bz2, dim, .. } => {
            cmd_export_chunks(&mut db, output_file, bx1, bz1, bx2, bz2, dim);
        }

        Command::EntityDensity { group_size, .. } => {
            cmd_entity_density(&mut db, group_size);
        }

        Command::ImportActors { .. } | Command::WipeActors { .. } | Command::ImportChunks { .. }
            | Command::DeleteChunks { .. } | Command::BatchDeleteChunks { .. } => {
            unreachable!()
        }
    }

    println!();
    std::process::exit(0);
}
