# World Inspector

读取和检视 Minecraft 基岩版存档数据 — 方块、玩家、实体、区块 — 直接从 LevelDB 中读取。支持跨存档导入/导出。

## 编译

```bash
# CLI 工具 + 静态库
cargo build --release

# 仅静态库
cargo build --release --lib
```

产物：
- `target/release/world-inspector` — CLI 工具
- `target/release/libworld_inspector.a` — C FFI 静态库

## CLI 用法

```
world-inspector <世界路径>                                   存档概要
world-inspector <世界路径> <x> <y> <z> [dimension]          查询方块
world-inspector <世界路径> --players                         列出玩家 
world-inspector <世界路径> --actors                          列出实体
world-inspector <世界路径> --player <key>                    查看玩家数据
world-inspector <世界路径> --player <key> --dump             完整 NBT 转储
world-inspector <世界路径> --player <key> --json             JSON 输出背包

实体管理：
world-inspector <世界路径> --wipe-actors                            擦除非玩家实体
world-inspector <世界路径> --wipe-actors --include-players          擦除全部实体（含玩家数据）
world-inspector <世界路径> --export-actors <file>                   导出实体到 JSON（含玩家）
world-inspector <世界路径> --export-actors <file> --no-players      导出仅非玩家实体
world-inspector <世界路径> --import-actors <file>                   从 JSON 导入实体（覆盖）
world-inspector <世界路径> --import-actors <file> --skip-existing   导入实体（跳过已存在）

区块管理：
world-inspector <世界路径> --export-chunks <file> <bx> <bz>                导出单区块
world-inspector <世界路径> --export-chunks <file> <bx1> <bz1> <bx2> <bz2>  导出区块范围
world-inspector <世界路径> --import-chunks <file>               从 JSON 导入区块（覆盖）
world-inspector <世界路径> --import-chunks <file> --skip-existing  导入区块（跳过已存在）
```

### 示例

```bash
# 存档概要
world-inspector /bedrock_server/worlds/Bedrock\ level

# 查询方块 (-3, -60, -3) 主世界
world-inspector /bedrock_server/worlds/Bedrock\ level -3 -60 -3

# 查看下界方块
world-inspector /bedrock_server/worlds/Bedrock\ level 0 100 0 nether

# 列出所有玩家
world-inspector /bedrock_server/worlds/Bedrock\ level --players

# 查看玩家背包 (JSON)
world-inspector /bedrock_server/worlds/Bedrock\ level --player player_<UUID> --json

# 完整数据转储
world-inspector /bedrock_server/worlds/Bedrock\ level --player player_<UUID> --dump

# 导出所有实体 + 玩家数据
world-inspector /world --export-actors entities.json

# 仅导出非玩家实体（不含玩家数据）
world-inspector /world --export-actors entities.json --no-players

# 导入实体到目标存档（覆盖模式）
world-inspector /target-world --import-actors entities.json

# 擦除非玩家实体
world-inspector /world --wipe-actors

# 擦除全部实体（含玩家数据）
world-inspector /world --wipe-actors --include-players

# 导出区块（方块坐标 0, 64 所在的单区块）
world-inspector /world --export-chunks chunks.json 0 64

# 导出区块范围（矩形区域内的所有区块）
world-inspector /world --export-chunks chunks.json -100 64 200 128

# 导入区块到目标存档
world-inspector /target-world --import-chunks chunks.json
```

## 跨存档数据迁移

通过组合使用导出/导入命令，可以在存档之间迁移数据：

```bash
# 1. 从源存档导出实体 + 玩家数据
./wi /source --export-actors entities.json

# 2. 从源存档导出区块（覆盖目标存档需要替换的区域）
./wi /source --export-chunks chunks.json -100 64 200 128

# 3. 清空目标存档（按需选做）
./wi /target --wipe-actors --include-players

# 4. 导入实体到目标存档
./wi /target --import-actors entities.json

# 5. 导入区块到目标存档
./wi /target --import-chunks chunks.json
```

实体和区块分开管理，可按需选择性迁移。

### 导出文件格式

所有导出的 JSON 文件通用结构：

```json
{
  "total": 100,
  "entries": [
    {
      "key_hex": "6163746f72707265666978...",
      "value_base64": "eJzEyz...",
      "identifier": "minecraft:zombie"
    }
  ],
  "chunks": ["0,4", "1,4"]
}
```

- `total` — 条目总数
- `entries` — 键值对列表（key 为 hex 编码，value 为 base64 编码）
- `identifier` — 实体类型（仅实体导出时存在）
- `chunks` — 已导出区块列表（仅区块导出时存在）

## 玩家 key

LevelDB 中的玩家 key 有三种格式：

| 格式 | 说明 |
|---|---|
| `~local_player` | 单人模式本地玩家 |
| `player_<UUID>` | 玩家身份数据（含 ServerId 指向 player_server_） |
| `player_server_<UUID>` | 玩家完整游戏数据（背包、位置、生命等） |

CLI 自动跟随 `ServerId` 链接显示关联数据。

## 注

- 只读命令以 `read_only` 模式打开 LevelDB，不修改数据
- 写命令（wipe/import）以读写模式独立打开 DB，不影响只读功能
- delete 操作写入 LevelDB 删除标记（tombstone），BDS 启动时自动识别
- 导出的 JSON 文件支持 `--skip-existing` 可重入安全地增量导入

## C FFI 接口

将 `world_inspector` 链接为静态库后，C/C++ 可调用以下函数。

### 基础接口

```c
#include "world_inspector.h"

// 打开/关闭世界
WiWorld* wi_open(const char* world_path);
void wi_close(WiWorld* world);

// 查询背包（结构化 C struct 返回）
typedef struct { int32_t slot; char* name; int32_t count; int32_t damage; char* tag_json; } WiItem;
typedef struct { WiItem* items; int32_t count; } WiItemArray;
typedef struct { WiItemArray inventory; WiItemArray armor; WiItem* offhand; } WiInventoryResult;

WiInventoryResult* wi_get_inventory(WiWorld* world, const char* player_key);
void wi_free_inventory(WiInventoryResult* result);

// 一键 JSON 查询（每次调用独立打开/关闭数据库）
char* wi_get_inventory_json(const char* world_path, const char* player_key);
void wi_free_string(char* s);

// 列出玩家
char** wi_list_player_keys(WiWorld* world, int32_t* out_count);
void wi_free_string_array(char** arr, int32_t count);
```

### 预编码背包接口

返回预序列化为二进制 NBT 的物品数据（LE 格式，无头），可直接用于需要预编码物品的 UI 系统，保留物品的全部 NBT 属性。

```c
typedef struct {
    int32_t slot;
    char* type_id;       // "minecraft:diamond_sword"
    int32_t count;
    int32_t damage;
    uint8_t* nbt_bytes;  // tag 子化合物的二进制 NBT（LE 无头格式）
    int32_t nbt_len;
} WiEncodedItem;

typedef struct {
    WiEncodedItem* items;
    int32_t count;
} WiEncodedInventory;

WiEncodedInventory* wi_get_encoded_inventory(const char* world_path, const char* player_key);
void wi_free_encoded_inventory(WiEncodedInventory* inv);
```

### C++ 示例

```cpp
#include "world_inspector.h"
#include <vector>
#include <string>
#include <cstdio>

struct InventorySlot {
    int slot;
    std::string type_id;
    int count;
    int damage;
    std::vector<uint8_t> nbt;
};

std::vector<InventorySlot> load_offline_inventory(const char* world_path, const char* player_uuid) {
    std::vector<InventorySlot> slots;
    WiEncodedInventory* inv = wi_get_encoded_inventory(world_path, player_uuid);
    if (!inv) return slots;

    for (int i = 0; i < inv->count; i++) {
        auto& item = inv->items[i];
        slots.push_back(InventorySlot{
            item.slot,
            item.type_id,
            item.count,
            item.damage,
            {item.nbt_bytes, item.nbt_bytes + item.nbt_len}
        });
    }
    wi_free_encoded_inventory(inv);
    return slots;
}
```

## Rust 库接口

```rust
use world_inspector::WorldHandle;

let mut handle = WorldHandle::open("worlds/Bedrock level")?;

// 查询玩家背包（结构化数据）
let inv = handle.get_player_inventory("player_<UUID>")?;
for item in &inv.inventory {
    println!("Slot {}: {} x{}", item.slot, item.name, item.count);
}

// 查询预编码物品（二进制 NBT）
let encoded = handle.get_player_encoded_items("player_<UUID>")?;
for item in &encoded {
    println!("Slot {}: {} nbt_len={}", item.slot, item.name, item.nbt_bytes.len());
}

// 列出所有玩家 key
let keys = handle.list_player_keys();
```

## 架构

```
LevelDB (NBT binary)
    │
    ├─ CLI: 方块查询、实体管理、区块操作
    │
    ├─ wi_get_inventory_json()   → JSON string（调试/外部接口）
    │
    ├─ wi_get_inventory()        → WiInventoryResult（结构化 C struct）
    │
    └─ wi_get_encoded_inventory() → WiEncodedItem[]（预编码 binary NBT）
                                          │
                                          ▼
                               预编码物品可直接供 UI 系统使用
```
