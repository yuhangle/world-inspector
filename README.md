# World Inspector

读取和检视 Minecraft 基岩版存档数据 — 玩家背包、方块、实体 — 直接从 LevelDB 中读取。

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
world-inspector <世界路径>                               存档概要
world-inspector <世界路径> <x> <y> <z> [dimension]      查询方块
world-inspector <世界路径> --players                     列出玩家
world-inspector <世界路径> --actors                      列出实体
world-inspector <世界路径> --player <key>                查看玩家数据
world-inspector <世界路径> --player <key> --dump         完整 NBT 转储
world-inspector <世界路径> --player <key> --json         JSON 输出背包
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
```

### 玩家 key

LevelDB 中的玩家 key 有三种格式：

| 格式 | 说明 |
|---|---|
| `~local_player` | 单人模式本地玩家 |
| `player_<UUID>` | 玩家身份数据（含 ServerId 指向 player_server_） |
| `player_server_<UUID>` | 玩家完整游戏数据（背包、位置、生命等） |

CLI 自动跟随 `ServerId` 链接显示关联数据。

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
    ├─ wi_get_inventory_json()   → JSON string（调试/外部接口）
    │
    ├─ wi_get_inventory()        → WiInventoryResult（结构化 C struct）
    │
    └─ wi_get_encoded_inventory() → WiEncodedItem[]（预编码 binary NBT）
                                          │
                                          ▼
                               预编码物品可直接供 UI 系统使用
```

预编码路径消除了 JSON 序列化/反序列化的冗余，保留了物品的全部 NBT 信息。
