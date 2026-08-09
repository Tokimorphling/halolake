# Halolake 编译与发布体积

本文说明仓库内已经落地的依赖与产物瘦身边界，以及如何复现检查结果。

## 1. 推荐构建命令

开发阶段继续使用普通 `dev` profile；正式分发使用 `dist`：

```bash
make dist
make size-report
```

`dist` 继承 `release`，并启用：

```toml
[profile.dist]
inherits = "release"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

这会增加最终链接时间，但会做跨 crate LTO、去除符号，并避免携带 unwind 路径，适合
发布产物；日常 `cargo check` 不受影响。`make dist` 和 `make dist-control-lite` 分别使用
`target/dist-workspace` 与 `target/dist-control-lite`，避免不同 feature 组合覆盖同名二进制。

只需要 control-api 核心能力时：

```bash
make dist-control-lite
```

lite 组合保留账号、Token、渠道、Gateway 内部接口和现有 SQL 存储兼容，关闭：

- 内嵌 Web（可由磁盘或反向代理提供）；
- Passkey/WebAuthn；
- admin extras 与 new-api 兼容壳。

默认 feature 是 Halolake 仓库内保留的完整产品组合。

### 本轮实测

同一台 macOS arm64、同一 Rust/Cargo 环境，外置 Web（与 Docker 发布模式一致）下
`ls -lh` 的四舍五入结果：

| 二进制 | 原 `release` 基线 | 完整 `dist` | 下降 |
|--------|------------------:|------------:|-----:|
| control-api | 39M | 12M | 约 69% |
| gateway-monoio | 13M | 7.7M | 约 41% |

control-lite 为 **10M**，比完整 control `dist` 再下降约 17%，比原基线下降约 74%。
fat-LTO 会明显增加最终链接时间，构建耗时不应跨机器或不同缓存状态比较。

## 2. 已处理的重复与隐式 feature

1. workspace 依赖只声明版本和最小公共能力；`axum`、`reqwest`、`sqlx`、`tokio`、
   `tokio-util` 的实际 feature 由成员按职责启用。
2. UUID 移除 `fast-rng`；不再为生成 v4 UUID额外引入另一套随机数实现。
3. Gateway 的 HTTPS proxy 从 `native-tls` 切到现有 Rustls 0.23 栈，workspace 不再依赖
   `native-tls`/OpenSSL；Docker builder 因此不再安装 `libssl-dev`。
4. 独立媒体服务及其 agent、hub、协议和 S3 SDK 已从本 workspace 移除。
5. lockfile package 数在本轮基线中由 **555 降到 473**。这不是二进制大小的直接等价物，
   但可作为依赖图收敛的回归指标。

Monoio HTTP 当前仍间接使用 Rustls 0.21，而 `monoio-transports`/应用使用 Rustls 0.23。
这是上游 crate 的 API 边界；强行 fork 会增加安全更新成本，因此本轮只移除了应用可控的
native-tls 第三套实现。

control-api 允许在同一二进制里按配置切换 SQLite、MySQL、PostgreSQL，因此三个 SQLx
driver 目前仍共同存在。若后续要进一步压缩 control-api，需要把各数据库实现拆到独立
backend crate/发行变体，而不是只改
`Cargo.toml`（源码中的静态枚举分派也必须同步隔离）。

## 3. 检查依赖来源

```bash
# 重复版本
cargo tree -d --workspace

# 某个成员最终打开了哪些 feature
cargo tree -e features -p halolake-control-api

# 验证 lite 不含 WebAuthn
cargo tree -p halolake-control-api \
  --no-default-features --features core,allocator-mimalloc \
  | rg 'webauthn|crypto-glue|p256|p384|p521'

# 验证 Gateway 不含 native-tls/OpenSSL
cargo tree -p halolake-gateway-monoio \
  | rg 'native-tls|openssl|monoio-native-tls'
```

后两条命令正常情况下应无输出。

## 4. Docker 产物

Docker 默认最后阶段仍是原来的全家桶，兼容现有 Compose。同时可以只发布一个服务：

```bash
docker build --target control-api -t halolake-control-api .
docker build --target gateway -t halolake-gateway .
```

前端只存在于 runtime 文件层，不再同时塞入 control-api executable。Cargo registry 与
编译目录使用 BuildKit cache；编译完成后只把 strip 后的二进制复制到最终镜像。

## 5. 评估注意事项

- 比较体积时必须使用同一平台、同一 Rust 版本和同一 profile。
- `du -h` 显示磁盘占用，`ls -lh` 显示逻辑文件长度，两者可能不同。
- macOS Mach-O 与 Linux ELF 不能横向直接比较。
- `target/` 包含增量产物、依赖 rlib 和多个 profile，不代表交付体积；交付时只看
  `target/dist/halolake-*` 或最终容器层。
- feature 组合变化后 Cargo 会保留两组中间产物。Make 目标已隔离各自的
  `CARGO_TARGET_DIR`；不要为了测冷编译而删除日常开发缓存。
- 测 standalone `embedded-web` 变体前必须先生成 `web/new-api/default/dist`，并把静态
  资源大小一并记录；缺少前端输入的二进制不能与完整 standalone 产物比较。
