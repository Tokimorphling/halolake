# Halolake vs new-api 渠道管理行为对照

目标：halolake control-api 兼容 new-api 前端（`web/new-api`），后端用更高性能的 Rust 实现。
对照基准：`ref/new-api`。

本文记录 **渠道列表 / 搜索 / 分组** 相关行为差异。状态分为：
- **已对齐**：已按 new-api 语义修复
- **部分对齐**：可用，但细节不同
- **未对齐**：前端可能表现异常或能力缺失

---

## 1. 已修复：按分组过滤（本次）

### 现象
前端 URL：`/channels?group=%5B%22ttt%22%5D`（query 中 group 数组会被序列化为 `group=ttt`）过滤后，列表仍出现 **default** 分组渠道。

### new-api 行为
- `GET /api/channel` / `GET /api/channel/search` 读取 `group` query。
- `NormalizeChannelGroupFilter`：空 / `all` / `null` → 不过滤。
- `ApplyChannelGroupFilter`：对逗号分隔的 `channel.group` 做 **完整 token 匹配**
  （SQL 类似 `',' || group || ',' LIKE '%,ttt,%'`），
  即 `group="ttt,default"` **会**命中 `ttt`，`group="default"` **不会**。

### 修复前 halolake
- HTTP 层 `PageQuery` / `ChannelSearchQuery` **不接收** `group` / `status` / `type` / `model`。
- `ListChannelsRequest` / `SearchChannelsRequest` 只有分页 + keyword。
- 管理 store 返回全部渠道，因此筛选 `ttt` 时仍能看到 default 渠道。

### 修复后
- `ChannelListQuery` / `ChannelSearchQuery` 解析 `group`、`status`、`type`、`model`。
- `MemoryManagementStore` 列表/搜索按 new-api 语义过滤：
  - group：精确分组成员匹配
  - status：`enabled|1` → 仅启用；`disabled|0` → 非启用；其它 → 全部
  - type：渠道类型
  - model：models 子串（search）
  - keyword：id 精确 / name 子串 / key 精确 / base_url 子串（search）
- 单元测试：`filters_channels_by_group_exact_token`

涉及文件：
- `crates/control-plane/src/management.rs`
- `apps/control-api/src/lib.rs`
- `apps/control-api/src/api_channel.rs`

---

## 2. 部分对齐 / 剩余差异

### 2.1 `type_counts`
| | new-api | 当前 halolake |
|--|---------|---------------|
| 含义 | 在 **group+status** 过滤后、**type 过滤前** 的全量计数 | **已对齐**：按 group+status 全量计数（不含 type 过滤） |

### 2.2 排序 `sort_by` / `sort_order` / `id_sort`
| new-api | 支持多列排序（priority/weight/id/...） |
| 当前 | 固定按 `id` 降序 |

前端可传 `sort_by`/`sort_order`，halolake 目前忽略。

### 2.3 `tag_mode`
| new-api | 先按 tag 聚合分页，再拉每个 tag 下渠道 |
| 当前 | 忽略 `tag_mode`，始终返回扁平渠道列表 |

前端 tag 模式展示可能不正确。

### 2.4 Search 分页语义
| new-api SearchChannels | DB 查询 **不分页**（先查全量再内存过滤 status/type），`total` 语义偏「过滤后全部」 |
| 当前 | 标准 offset/limit 分页 |

大数据量时页数/总数可能与 new-api 不完全一致。

### 2.5 keyword 匹配字段
| new-api | id / name LIKE / **key 精确** / base_url LIKE + models LIKE |
| 当前 | 已对齐上述字段（key 精确；models 由独立 `model` 参数过滤） |

说明：new-api 在 SQL 里即使 `model` 为空也会 `models LIKE '%%'`（恒真）。行为等价。

### 2.6 `/api/group` 列表
| | new-api | 当前 |
|--|---------|------|
| 数据源 | 选项 + 用户/令牌/渠道等 | 同样聚合，且 **总是包含 `default`** |

因此筛选器下拉里始终有 `default` 是预期的；问题在于 **选中 ttt 后列表未过滤**，不是 group 列表多了 default。

### 2.7 响应字段
- 双方都返回 `items/total/page/page_size`。
- new-api 额外 `type_counts`：halolake 已返回，但统计口径见 2.1。
- 单条 channel 的 `channel_info` 等：halolake 有兼容 enrich，部分高级字段可能仍是占位。

---

## 3. 未在本次处理的相关能力（清单）

以下不直接导致「ttt 过滤仍见 default」，但影响完整兼容：

1. **渠道 tag 模式**（list/search + tag CRUD 细节）
2. **排序参数**完整落地
3. **type_counts 全量统计**（非当前页）
4. **search 在 key 上精确匹配**（已修）以外的 multi-key 状态筛选
5. **Windows 原生编译 gateway-monoio**  
   - 失败点：`monoio-transports 0.5.3` 无条件 `use monoio::net::UnixStream`  
   - `monoio` 本体有 experimental Windows（TCP），但 **Unix domain socket API 仅 `cfg(unix)`**  
   - `control-api`（tokio）可在 Windows 正常 release 编译  
   - 建议：生产 gateway 用 Linux/Docker；Windows 本地可只跑 control-api，或 WSL/交叉编译

---

## 4. 建议回归用例

1. 创建渠道 A：`group=default`；渠道 B：`group=ttt`；渠道 C：`group=ttt,default`
2. `GET /api/channel?group=ttt` → 仅 B、C
3. `GET /api/channel?group=default` → 仅 A、C
4. `GET /api/channel?group=all` 或无 group → 全部
5. `GET /api/channel/search?keyword=B名&group=ttt` → 仅 B（若名命中）
6. UI：`/channels?group=["ttt"]` 列表不出现纯 default 渠道

---

## 5. 变更摘要（代码）

| 区域 | 变更 |
|------|------|
| control-plane | `List/SearchChannelsRequest` 增加 group/status/type/model；过滤 helper + 单测 |
| control-api HTTP | `ChannelListQuery`/`ChannelSearchQuery` 透传过滤参数；响应带 `type_counts` |

