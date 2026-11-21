# CodeGraph Rust（`cgr`）

**面向 AI 编码 Agent 的本地代码知识图谱，使用 Rust 构建。**

[English](README.md) · [简体中文](README.zh-CN.md) · [架构设计](docs/ARCHITECTURE.md) · [性能实测](docs/BENCHMARKS.md) · [更新记录](CHANGELOG.md)

CodeGraph Rust 将代码仓库索引为持久化的符号、引用和关系图谱，帮助开发者与编码 Agent 获取相关源码、
追踪跨文件调用、检查依赖关系，并分析修改可能影响的范围。项目通过一个命令行可执行文件提供这些能力，
也可以作为 MCP 服务接入编码工具。

引擎在本地运行，通过源码语法和确定性分析建立图谱。索引与查询不需要托管服务、向量嵌入 API、模型密钥、
Node.js 运行时或数据库服务。原生索引保存在项目的 `.cgr/` 目录，查询中的源码窗口从当前工作区读取。

本项目是 [colbymchenry/codegraph](https://github.com/colbymchenry/codegraph) 引擎的独立 Rust 重写：
复用上游 Tree-sitter 提取内核，沿用并适配面向 Agent 的工具定义与使用说明，MCP 工具名前缀为 `codegraph-rust_`。
存储、引用解析、增量同步、查询与服务集成由本 Rust workspace 实现。
目前与上游的功能覆盖仍有差异，具体见[当前限制](#当前限制)。

## 目录

- [核心能力](#核心能力)
- [实测性能](#实测性能)
- [安装](#安装)
- [快速开始](#快速开始)
- [语言与框架支持](#语言与框架支持)
- [命令行参考](#命令行参考)
- [Agent 与 MCP 集成](#agent-与-mcp-集成)
- [配置](#配置)
- [架构设计](#架构设计)
- [索引生命周期](#索引生命周期)
- [开发与验证](#开发与验证)
- [当前限制](#当前限制)
- [参与贡献](#参与贡献)
- [致谢与许可证](#致谢与许可证)

## 核心能力

- **基于真实源码的探索**：`explore` 在有输出预算的单次响应中组合符号检索、调用路径、相关源码与修改影响。
  查询可包含自然语言问题、符号名、限定名或文件名；提供具体符号有助于定位调用链。
- **跨文件代码导航**：查询调用方、被调用方、实现关系、导入和依赖，并覆盖部分框架路由与动态分派模式。
- **增量索引**：重新提取发生变化的文件，重新解析受影响的引用；大范围修改或配置变化可以触发全量重建。
- **高效持久化存储**：使用不可变 mmap 段、密集整数 ID、字符串池、双向 CSR 边，以及预建的 FST、trigram、BM25 索引。
- **CLI 与 MCP 双入口**：终端和 Agent 共享查询处理逻辑；内置 Claude Code、Cursor、Codex 配置安装器。
- **Agent 会话中的自动更新**：MCP 服务可监听项目变化，经过防抖后同步并发布新快照；没有监听器时可手动运行 `cgr sync`。
- **可检查的分析结果**：通过命令行检查索引状态与校验和、查看未解析引用、对比增量与全量图谱，或导出 SQLite 文件。

## 实测性能

**2026-11-02** 在合并后的 `main` **`cd69b83`** 上测量，对照原版
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph) **`d2e0389`**（加载原生提取内核），
覆盖 JavaScript、Go、Rust、Python、TypeScript、Kotlin/Java、C、C++、C#、PHP、Ruby、Swift
十二个仓库，所有数据来自同一台机器和同一套配置。等权几何平均加速比：全量索引 **6.63×**；
增量 CLI 同步（无修改 / 追加注释 / 单文件改名）**9.23× / 7.04× / 3.14×**；
热查询 P50 **3.92×**；新进程 `explore` **13.12×**。全量索引的峰值 RSS 中位数降低
**55.2%–87.6%**。并非每一项 `cgr` 都更快：Tokio 单文件改名同步因退回全量重建而更慢（0.84×），
cJSON 和 MediatR 的热查询 P95 略慢。相对原版的严格图谱一致性在 **12 个仓库中有 4 个**
达到 97% 精确率 / 95% 召回率门槛；原版只是参照而非语义真值，速度更快也不代表图谱相同。
全部样本、查询、同步报告和分类型边数见[机器可读结果](docs/benchmarks/2026-11-02-twelve-repo.json)。

| 配置项 | 取值 |
|---|---|
| 机器 | AMD Ryzen 9 8940HX（16 核 32 线程），Windows 11 外接电源；WSL2 Ubuntu 24.04（Linux 6.18.33.2），15 GiB 内存 |
| `cgr` | `cd69b83`，`cargo build --release --locked -p cgr`（fat LTO、单 codegen unit），rustc 1.99.1 |
| 原版 | `d2e0389` 生产构建 `dist/`，加载 `linux-x64` 原生内核预编译包；Node v24.18.0，以 `node --liftoff-only --disable-warning=ExperimentalWarning` 启动 |
| 工作线程 | `CGR_THREADS=2`、`CGR_MEMORY_MB=2048`；`CODEGRAPH_PARSE_WORKERS=2`、`CODEGRAPH_RESOLVE_WORKERS=2`；关闭遥测 |
| 计时 | 命令逐条执行，计时命令之间至少间隔 30 秒，每条执行前至少 6 GiB 可用内存，热文件缓存；耗时包含进程启动和退出，峰值 RSS 取自 `wait4` |
| 语料 | 下表固定提交；双方索引相同文件，源码 SHA-256 摘要一致（只有一方会索引的文件通过 `codegraph.json` 在双方同时排除） |

### 全量索引

每边先预热一次，再交替顺序计时三次完整 CLI 重建。原版先用 `CodeGraph.initSync` 创建空数据库，
再由 `index` 命令重建；`cgr index` 重建完整快照。样本取自 `f4b9685`，其源码与 `cd69b83`
相同，差异仅在本 README。

| 仓库 | 语言 | 提交 | 文件数 | 原版全量索引 | `cgr` 全量索引 | 加速比 | 峰值 RSS：原版 → `cgr` |
|---|---|---|---:|---:|---:|---:|---:|
| Express | JavaScript | [`1c6f1b0`](https://github.com/expressjs/express/tree/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824) | 142 | 990.8 ms | 162.3 ms | **6.10×** | 218.7 → 97.9 MiB |
| Gin | Go | [`0f09c3a`](https://github.com/gin-gonic/gin/tree/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646) | 99 | 1,242.5 ms | 165.0 ms | **7.53×** | 235.1 → 67.8 MiB |
| Tokio | Rust | [`b499257`](https://github.com/tokio-rs/tokio/tree/b499257ef62bd631b83a988d371f4330f545670c) | 814 | 4,637.9 ms | 882.0 ms | **5.26×** | 701.3 → 108.7 MiB |
| Django | Python | [`a05e16a`](https://github.com/django/django/tree/a05e16a7230fe33d3f43f4a3aaed103cebb42efc) | 2,984 | 18,467.9 ms | 2,392.8 ms | **7.72×** | 1,936.6 → 239.4 MiB |
| Zod | TypeScript | [`2554b70`](https://github.com/colinhacks/zod/tree/2554b70c3d4a80273381e61d527d820720626440) | 522 | 4,276.2 ms | 689.2 ms | **6.20×** | 598.2 → 142.0 MiB |
| OkHttp | Kotlin/Java | [`16fb6ae`](https://github.com/square/okhttp/tree/16fb6aef3aee1c4645fc8d24af01f8a4dc128ebf) | 690 | 6,677.3 ms | 939.8 ms | **7.10×** | 823.0 → 151.1 MiB |
| cJSON | C | [`6d9f244`](https://github.com/DaveGamble/cJSON/tree/6d9f2443ab071f86e5d9b43025a40929ec41c46c) | 68 | 1,046.5 ms | 125.5 ms | **8.34×** | 307.3 → 66.5 MiB |
| fmt | C++ | [`4ed3535`](https://github.com/fmtlib/fmt/tree/4ed3535092a96ac9f56fab80df83416d51bcc78e) | 80 | 4,731.2 ms | 497.9 ms | **9.50×** | 542.0 → 107.3 MiB |
| MediatR | C# | [`62010ba`](https://github.com/jbogard/MediatR/tree/62010baf12851770553f275ce4047e93f4471e79) | 159 | 1,046.3 ms | 171.3 ms | **6.11×** | 228.0 → 98.0 MiB |
| Slim | PHP | [`3675bf6`](https://github.com/slimphp/Slim/tree/3675bf6baac66b07032575b7bef4200b60b7974b) | 125 | 831.1 ms | 122.9 ms | **6.76×** | 209.2 → 61.6 MiB |
| Sinatra | Ruby | [`cb22afd`](https://github.com/sinatra/sinatra/tree/cb22afd7902b566b6eaba6c4ea89739494a65d12) | 147 | 806.5 ms | 137.9 ms | **5.85×** | 206.3 → 70.8 MiB |
| Alamofire | Swift | [`6c2892e`](https://github.com/Alamofire/Alamofire/tree/6c2892ea5adb5a03fa81c510933dd782c5ddd9cb) | 111 | 2,048.6 ms | 450.4 ms | **4.55×** | 403.9 → 107.3 MiB |

### 增量 CLI 同步

每类、每边三次计时 `sync` 命令的中位数，原版 → `cgr`（加速比）。双方对同一个核心源文件做相同编辑：
追加注释每次写入不同注释，改名在该文件内交替修改一个符号及其引用；每类结束后恢复原始字节并同步。
恢复后十二个仓库两边的严格边集合都与初始完全一致，`cgr status --verify --strict` 和
`cgr debug diff` 均通过。CLI 计时包含文件系统扫描，不代表监听器延迟，也不代表全仓库重构。

| 仓库 | 无修改同步 | 追加注释 | 单文件符号改名 |
|---|---:|---:|---:|
| Express | 258.3 → 22.2 ms (11.65×) | 478.2 → 69.6 ms (6.87×) | 481.1 → 198.9 ms (2.42×)† |
| Gin | 255.3 → 20.0 ms (12.74×) | 539.9 → 85.7 ms (6.30×) | 546.7 → 188.2 ms (2.90×)† |
| Tokio | 286.8 → 32.2 ms (8.91×) | 762.8 → 95.2 ms (8.01×) | 753.5 → 897.4 ms (0.84×)† |
| Django | 672.5 → 201.7 ms (3.33×) | 3,054.4 → 281.7 ms (10.84×) | 3,145.5 → 274.0 ms (11.48×) |
| Zod | 273.0 → 37.8 ms (7.21×) | 910.3 → 86.6 ms (10.52×) | 1,517.7 → 760.3 ms (2.00×)† |
| OkHttp | 364.9 → 55.6 ms (6.56×) | 1,158.1 → 141.2 ms (8.20×) | 1,265.5 → 1,037.9 ms (1.22×)† |
| cJSON | 271.6 → 22.7 ms (11.97×) | 601.1 → 99.3 ms (6.05×) | 607.9 → 76.0 ms (7.99×) |
| fmt | 278.2 → 23.1 ms (12.05×) | 1,445.0 → 155.1 ms (9.32×) | 1,405.3 → 552.9 ms (2.54×)† |
| MediatR | 256.3 → 24.3 ms (10.56×) | 467.7 → 79.8 ms (5.86×) | 480.8 → 197.3 ms (2.44×)† |
| Slim | 253.0 → 20.7 ms (12.25×) | 398.6 → 81.3 ms (4.90×) | 409.9 → 146.0 ms (2.81×)† |
| Sinatra | 245.6 → 23.4 ms (10.49×) | 516.0 → 109.3 ms (4.72×) | 508.9 → 81.0 ms (6.28×) |
| Alamofire | 298.7 → 32.0 ms (9.33×) | 702.0 → 116.7 ms (6.02×) | 657.4 → 103.6 ms (6.34×) |

† `cgr sync` 没有增量重解析，而是重建了完整索引：这次改名标记为受影响的引用超过 30% 上限
（例如 Tokio 65,414 个中有 62,419 个，Express 10,492 个中有 10,294 个），因此这些单元格计的是
全量重建加变更扫描的耗时。该模式来自对相同编辑的一次不计时重放：十二个仓库的追加注释都只重解析
1 个文件，Django、cJSON、Sinatra、Alamofire 的改名重解析 1–23 个文件。

<details>
<summary>各仓库编辑的文件和改名的符号</summary>

| 仓库 | 文件 | 符号 |
|---|---|---|
| Express | `lib/application.js` | `handle` |
| Gin | `gin.go` | `Default` |
| Tokio | `tokio/src/runtime/builder.rs` | `new_current_thread` |
| Django | `django/core/handlers/base.py` | `get_response` |
| Zod | `packages/zod/src/v4/core/parse.ts` | `_parse` |
| OkHttp | `okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt` | `newCall` |
| cJSON | `cJSON.c` | `cJSON_ParseWithOpts` |
| fmt | `include/fmt/format.h` | `format_decimal` |
| MediatR | `src/MediatR/Mediator.cs` | `Send` |
| Slim | `Slim/App.php` | `handle` |
| Sinatra | `lib/sinatra/base.rb` | `invoke` |
| Alamofire | `Source/Core/Session.swift` | `perform` |

</details>

### 查询延迟

热查询直接调用生产环境的进程内 explore 处理函数（`cgr bench explore`；原版
`ToolHandler.executeReadTool("codegraph_explore")`），两边使用同一组 20 个固定问题：先预热 8 个问题，
再运行 20 个问题 × 3 轮，共 60 个观测值，按最近秩取百分位，不包含 MCP 传输和排队。新进程一列
对第一个问题每边执行三次 CLI `explore`，计入启动成本。Express、Gin、Tokio、Django 沿用原有问题列表；
其余仓库按相同形式（单个名称、名称对、"how does A work with B"）以固定随机种子从生产代码的函数、
方法和类型名中抽取。两边返回内容不同，长度也不能直接比较：十二个仓库平均每个回答原版为
17,370 个 UTF-16 码元，`cgr` 为 14,247 个 UTF-8 字节。

| 仓库 | 热查询 P50：原版 → `cgr` | P50 加速比 | 热查询 P95：原版 → `cgr` | 新进程 CLI 中位数 |
|---|---:|---:|---:|---:|
| Express | 7.32 → 2.13 ms | 3.43× | 19.39 → 6.30 ms | 241.5 → 22.7 ms (10.62×) |
| Gin | 13.71 → 1.22 ms | 11.27× | 19.04 → 6.31 ms | 219.9 → 19.8 ms (11.10×) |
| Tokio | 32.55 → 6.83 ms | 4.77× | 44.41 → 16.82 ms | 323.6 → 18.8 ms (17.25×) |
| Django | 59.04 → 10.42 ms | 5.67× | 82.90 → 20.83 ms | 605.5 → 19.9 ms (30.37×) |
| Zod | 42.48 → 7.65 ms | 5.55× | 58.70 → 12.81 ms | 352.0 → 24.1 ms (14.62×) |
| OkHttp | 33.22 → 6.03 ms | 5.50× | 53.18 → 23.41 ms | 354.1 → 24.7 ms (14.34×) |
| cJSON | 14.36 → 9.40 ms | 1.53× | 19.60 → 21.37 ms | 256.2 → 17.8 ms (14.41×) |
| fmt | 23.36 → 5.03 ms | 4.64× | 34.04 → 9.68 ms | 263.4 → 22.3 ms (11.81×) |
| MediatR | 12.95 → 9.27 ms | 1.40× | 17.99 → 19.20 ms | 237.1 → 31.2 ms (7.61×) |
| Slim | 14.00 → 5.27 ms | 2.66× | 18.08 → 16.66 ms | 210.7 → 20.3 ms (10.39×) |
| Sinatra | 11.06 → 2.39 ms | 4.63× | 15.48 → 4.94 ms | 238.0 → 20.1 ms (11.85×) |
| Alamofire | 17.15 → 5.10 ms | 3.36× | 24.90 → 11.14 ms | 261.4 → 20.7 ms (12.65×) |

### 与原版的图谱一致性

边按 `(源节点稳定 ID, 目标节点稳定 ID, 类型, 行, UTF-16 列)` 比较，NULL 与 0 视为不同；
精确率 = 共有边 / `cgr` 边，召回率 = 共有边 / 原版边。所有边类型、路由节点和含语法错误的文件都计入分母。

| 仓库 | 语言 | 精确率 | 召回率 | 调用边精确率 | 调用边召回率 | 边数：`cgr` / 原版 / 共有 | 97% / 95% 门槛 |
|---|---|---:|---:|---:|---:|---:|---|
| Express | JavaScript | 64.18% | 87.75% | 56.76% | 84.61% | 3,191 / 2,334 / 2,048 | 未通过 |
| Gin | Go | 93.58% | 96.53% | 99.75% | 94.12% | 8,328 / 8,073 / 7,793 | 未通过 |
| Tokio | Rust | 78.84% | 88.30% | 81.95% | 73.46% | 44,445 / 39,682 / 35,040 | 未通过 |
| Django | Python | 94.21% | 89.82% | 91.38% | 77.43% | 185,067 / 194,105 / 174,351 | 未通过 |
| Zod | TypeScript | 92.18% | 92.86% | 91.53% | 93.35% | 35,954 / 35,693 / 33,144 | 未通过 |
| OkHttp | Kotlin/Java | 88.47% | 82.24% | 80.44% | 64.58% | 49,204 / 52,935 / 43,532 | 未通过 |
| cJSON | C | 99.84% | 99.60% | 99.94% | 99.59% | 3,757 / 3,766 / 3,751 | 通过 |
| fmt | C++ | 94.07% | 92.79% | 90.33% | 87.37% | 20,638 / 20,923 / 19,414 | 未通过 |
| MediatR | C# | 98.17% | 97.28% | 95.91% | 91.42% | 5,685 / 5,737 / 5,581 | 通过 |
| Slim | PHP | 98.21% | 95.39% | 92.98% | 83.31% | 4,527 / 4,661 / 4,446 | 通过 |
| Sinatra | Ruby | 98.62% | 96.30% | 98.27% | 89.29% | 2,613 / 2,676 / 2,577 | 通过 |
| Alamofire | Swift | 96.46% | 93.78% | 91.91% | 86.74% | 12,164 / 12,512 / 11,734 | 未通过 |

未达门槛的仓库中，除 Gin 外差异边最多的都是调用边（47%–87%）；Gin 的包含、引用和调用差异大致相当。
原版自身也有已知错误（见[方法说明](docs/BENCHMARKS.md)），因此差距不等于 `cgr` 的错误数，也不能靠照搬
原版的选择来弥合。Express 精确率最低：1,143 条 `cgr` 独有边中，805 条是文件级调用（如 `app.use` 557 条、
`app.set` 84 条、`app.listen` 40 条）被链接到 Express 自身的方法，279 条是文件到路由的包含边。详见
[第二阶段语言](docs/LANGUAGES-P5.md)和 [JavaScript/TypeScript 诊断](docs/GRAPH-DIAGNOSTICS-2026-11-01.md)。

### 性能工程更新

main 已包含按事件路径同步、静默期纯数据快照合并、提取缓冲临时映射、解析结果预分配、hook 融合扫描、
事件合成的局部/并行连接、BM25 倒排压缩、自适应基数排序和有界源码窗口。
MCP 增加排队上限、协作取消及项目/监听资源回收。

快照格式为 **version 2**，旧索引需运行 `cgr index /path/to/project` 重建。
上方全量索引复测的版本已单独标注。历史 **6.40×** 使用了不同线程参数和较早的上游版本，现保存在
[历史报告](docs/BENCHMARKS-HISTORICAL.md)，不能作为本轮代码优化的前后收益。
实现边界见[中英双语优化记录](docs/PERFORMANCE-IMPLEMENTATION.md)。

## 安装

### 从源码构建并安装

从源码构建需要 **Rust 1.89 或更新版本**、Cargo、Git，以及用于编译 Tree-sitter 语法的 C 编译工具链。

```sh
git clone https://github.com/NianJiuZst/CodeGraph-Rust.git
cd CodeGraph-Rust
cargo install --path crates/cgr --locked
cgr version --verbose
```

Cargo 将 `cgr` 安装到其可执行文件目录，通常为 `~/.cargo/bin`，请确认该目录已加入 `PATH`。
Rust 和 C 编译器是构建时依赖，生成的可执行文件运行时不需要 Rust 工具链或 Node.js。

只构建、不安装到系统目录：

```sh
cargo build --release --locked -p cgr
./target/release/cgr --help
```

Windows 下的可执行文件为 `target\release\cgr.exe`。

### Release 下载脚本

下载脚本会校验 [Releases 页面](https://github.com/NianJiuZst/CodeGraph-Rust/releases)上发布的归档文件的 SHA-256。

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.sh | sh
```

```powershell
# Windows，PowerShell 7+
irm https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.ps1 | iex
```

发布流程定义了以下构建目标：

| 平台 | 架构 | 打包方式 |
|---|---|---|
| Linux | x86_64、ARM64 | 静态 musl 可执行文件，`.tar.gz` |
| macOS | Intel x86_64、Apple Silicon ARM64 | 可执行文件，`.tar.gz` |
| Windows | x86_64、ARM64 | MSVC 可执行文件，`.zip` |

下载脚本支持通过 `CGR_VERSION` 指定版本、通过 `CGR_INSTALL_DIR` 指定安装位置。
默认目录为 Linux/macOS 的 `~/.local/bin`，以及 Windows 的 `%LOCALAPPDATA%\Programs\cgr`。

## 快速开始

在需要分析的代码仓库中运行：

```sh
cd /path/to/your-project
cgr init
cgr status --verify
```

使用仓库中的符号名探索代码：

```sh
cgr explore "AuthService loginUser SessionStore"
cgr query loginUser --json
cgr callers loginUser
cgr callees loginUser
cgr impact loginUser --depth 2
```

`AuthService`、`loginUser`、`SessionStore` 是示例名称，需要替换为项目实际符号。
也可以使用自然语言，例如 `cgr explore "登录是如何走到数据库的"`。
响应基于索引图谱与相关源码，包含行号，并受输出预算约束。

将 MCP 服务注册到支持的 Agent：

```sh
cgr install --dry-run
cgr install
```

安装器负责连接 Agent，`cgr init` 负责为每个项目建立索引。仅使用 CLI 时，编辑后运行 `cgr sync`；
MCP 服务运行且启用监听时，会在防抖后自动同步源码变化。

## 语言与框架支持

### 源码语言

| 语言 | 已支持的文件扩展名 |
|---|---|
| TypeScript / TSX | `.ts`、`.mts`、`.cts`、`.tsx` |
| JavaScript / JSX | `.js`、`.mjs`、`.cjs`、`.jsx`、`.xsjs`、`.xsjslib` |
| Python | `.py`、`.pyw` |
| Go | `.go` |
| Java | `.java` |
| Rust | `.rs` |
| Kotlin | `.kt`、`.kts` |
| C | `.c`、`.h`（含 C++ 语法的 `.h` 按 C++ 解析） |
| C++ | `.cpp`、`.cc`、`.cxx`、`.hpp`、`.hxx`、`.metal`、`.cu`、`.cuh` |
| C# | `.cs` |
| PHP | `.php`、`.module`、`.install`、`.theme`、`.inc` |
| Ruby | `.rb`、`.rake` |
| Swift | `.swift` |

提取阶段记录源码结构、符号和引用；引用解析阶段使用可获得的作用域、导入、接收者类型、模块及包信息。
覆盖度受语言构造和项目配置影响，当前实现是静态代码图谱，不能替代完整编译器的类型检查。

TypeScript/TSX 工厂调用支持传播函数声明、变量初始化的箭头/函数表达式和方法的显式命名返回类型，
按工厂的外层声明及导入查找目标，函数体内部声明的类型不会遮蔽返回注解。
联合/交叉类型、泛型替换、数组和 async/generator 包装仍保持未解析；不支持的注解不会作为猜测同名方法的依据。
普通语句块中的类型遮蔽尚未建模。这些返回规则使用引擎版本 10，同步旧索引时会刷新 facts 和调用边。

### 框架与动态分派规则

已实现 **Express、Flask、FastAPI、Django、Gin、Spring、Axum、Next.js、NestJS、React Router 和 TanStack Router** 的路由提取 hook。
框架检测依赖项目清单，因此缺失依赖声明或索引框架自身仓库时，路由覆盖可能不同；实测文档记录了已观察到的差异。

合成边规则覆盖接口与实现分派、Go 结构化接口、事件发射器处理函数、React 重新渲染、JSX 父子组件关系等模式。
这些规则带有来源信息和数量限制，属于针对特定模式的启发式分析，不覆盖任意运行时行为。

NestJS 中，可静态读取的 `RouterModule.register` 路径和由 `NestFactory.create` 创建的应用上的字面量
`setGlobalPrefix`（包括可读取的排除项）会参与 HTTP 客户端匹配；路由节点名称仍使用控制器内的路径。
React Router 支持通过导入和转导出使用其他文件中的导出路由数组，包括字面量父路由和 `children` 数组。
索引会保留导出数组的候选路由节点，但只有被已识别的路由器调用使用的数组才参与导航匹配。
动态路由配置不在此跨文件规则的覆盖范围内。
Go 接口分派包含无歧义的提升方法；方法集推断仍是有界的静态近似。

引擎规则版本 7 补充了以下解析规则与合成边（均为有界的静态规则，歧义时不连）：

- **Go**：嵌入的标准库类型参与接口判定（`io.Reader`、`*os.File`、`error` 等方法表移植自上游），
  其他外部嵌入类型按"不透明"处理；按书写的包限定绑定内核丢掉限定的项目内嵌入；
  func 字段分派（`d.refreshf()` 连到写入该字段的函数，含一层字段间复制）；
  protoc 生成的 `Unimplemented*Server` 连到手写实现；Gin `Context.Next` 连到注册的中间件。
- **TS/JS**：`const local = other` 与 `export const alias = realImpl` 的调用转发到真实函数，
  对象字面量成员（`api.run()`）同样转发；模块级常量的条件 / `??` / `||` 各分支（`const-alias`）；
  ES5 构造函数及其 `prototype` 方法、`util.inherits` / `Object.create` 继承；
  `.d.ts` 中 `declare module 'x'` 声明的环境模块；Redux thunk 的 `dispatch` 链与 RTK Query 生成的 hook。
- **Rust**：泛型参数按 trait 约束解析（无约束的类型参数不再误连同名项目类型）；
  `impl Deref` 目标类型、`type X = Y` 别名、`Mutex` / `RwLock` 的锁 guard 自动解引用；`self.字段` 调用链求值。
- **Python**：分支返回不同类的工厂函数按最近公共基类类型化；`self.x = self.make()` 跨方法字段流；
  pytest fixture 参数（同文件或上级目录 `conftest.py`）取 fixture 的返回类型。
- **跨语言 / JVM**：Spring 测试请求（MockMvc、WebTestClient、TestRestTemplate、RestAssured）连到路由；
  Kotlin Multiplatform `expect` 连到各平台 `actual`；Tauri 前端 `invoke('cmd')` 连到已注册的 `#[tauri::command]`；
  React Native `NativeModules.X.m()` 连到 Java / Kotlin 模块的 `@ReactMethod`。

尚未覆盖：MyBatis XML、Angular 模板、Vue / Svelte 生态的 store 与路由、React Native 事件与 Fabric/Expo 原生侧、
Tauri 事件、GoFrame，以及 Python 参数类型流（上游仅对 Ruby 实现）。

C/C++、C#、PHP、Ruby、Swift 使用上游的提取 walker，并配有本项目自己的解析规则（头文件包含、命名空间、`require`、SwiftPM 模块、隐式 `this` 成员）；其解析质量只在样例仓库上做过检查，尚未与上游 oracle 对比，也还没有框架路由规则。Dart、Scala、Lua、Luau、R 只做语言识别，不做提取。当前也不包含 Vue/Svelte 专用提取逻辑。

## 命令行参考

完整选项见 `cgr --help` 或 `cgr <command> --help`。使用 `--path` / `-p` 指定项目，例如
`cgr --path /path/to/project explore "UserService save"`。查询命令可以在当前目录或其祖先目录查找已有索引。

| 命令 | 用途 |
|---|---|
| `cgr init [path]` | 创建 `.cgr/` 并建立全量索引 |
| `cgr index [path] --timings` | 全量重建并输出各阶段耗时 |
| `cgr sync [path]` | 将新增、修改、删除的文件同步到索引 |
| `cgr status --json --verify` | 查看数量、变更和快照校验结果 |
| `cgr status --strict` | 索引存在待同步变更时返回非零状态 |
| `cgr explore <query…>` | 获取相关源码、调用路径与修改影响 |
| `cgr query <text> --kind function --json` | 检索符号，可按类型过滤 |
| `cgr node <symbol-or-file> --code` | 查看符号或文件及其源码 |
| `cgr files [--tests\|--no-tests] [--format package]` | 按目录树、语言或包清单查看已索引文件 |
| `cgr callers <symbol>` / `cgr callees <symbol>` | 查询调用方或被调用方 |
| `cgr impact <symbol> --depth 2` | 查看修改影响的图谱邻域 |
| `cgr implementations <symbol> --depth 10` | 查找实现或子类型 |
| `cgr affected <files…> [--cross-tier] --json` | 查找与变更文件关联的测试文件 |
| `cgr cycles --json` | 查看导入环；存在环时退出码为 1 |
| `cgr export --sqlite graph.sqlite` | 导出可供 SQL 分析的 SQLite 图谱 |
| `cgr serve --mcp` / `cgr serve --http ADDR` | 启动 MCP 服务 |
| `cgr install` / `cgr uninstall` | 添加或移除 Agent 集成 |
| `cgr uninit [path] --yes` | 删除项目索引 |
| `cgr version --verbose` | 查看构建、格式与内核版本信息 |

正常成功返回 0；执行失败以及检测到循环依赖等情况返回 1；命令行参数错误返回 2。
源码窗口来自当前工作区，因此编辑后需要及时同步图谱。

## Agent 与 MCP 集成

### 自动配置

`cgr install` 检测已支持的 Agent，并在保留注释和无关配置的基础上编辑已有文件。
用户固定的命令与设置会被保留，安装器补充缺失的集成键；`uninstall` 移除本工具的注册项与标记指令块。

```sh
cgr install --agent claude-code
cgr install --agent cursor
cgr install --agent codex
cgr install --local --agent claude-code
cgr install --no-hook
cgr uninstall --agent cursor
```

| Agent | 全局注册 | 项目级行为 |
|---|---|---|
| Claude Code | MCP、使用说明、可选提示钩子与 explore 权限 | `.mcp.json`、`CLAUDE.md`、`.claude/settings.json` |
| Cursor | `~/.cursor/mcp.json`，同时传递工作区路径 | `.cursor/mcp.json`，传入项目路径 |
| Codex | `~/.codex/config.toml` 中的 MCP 注册与使用说明 | `--local` 只修改项目 `AGENTS.md`，服务需要另行全局注册 |

Claude Code 支持 `CLAUDE_CONFIG_DIR`，Codex 支持 `CODEX_HOME`。
`--dry-run` 预览修改，`--bin` 指定另一处可执行文件，`--no-hook` 跳过可选的 Claude Code 提示钩子，
`--no-permissions` 跳过 explore 权限条目。

### 手动配置 stdio

对于使用 `mcpServers` JSON 配置的 MCP 客户端，可填写当前机器上的绝对路径：

```json
{
  "mcpServers": {
    "codegraph-rust": {
      "command": "/absolute/path/to/cgr",
      "args": ["serve", "--mcp", "--path", "/absolute/path/to/project"]
    }
  }
}
```

先为该项目运行 `cgr init`。客户端可以提供 MCP roots，各工具也接受 `projectPath` 来选择其他已索引项目。
客户端不提供项目根目录时，可显式使用 `--path`。如果服务只需要读取快照而不监听源码变化，可添加 `--no-watch`。

默认只公开 **`codegraph-rust_explore`**。在服务进程环境中设置 `CGR_MCP_TOOLS` 可启用其他工具：

```sh
CGR_MCP_TOOLS=explore,node,callers,callees,impact,files,search,status cgr serve --mcp
```

| 工具 | 用途 |
|---|---|
| `codegraph-rust_explore` | 相关源码、指定符号间的调用路径与影响范围 |
| `codegraph-rust_node` | 符号或文件详情与源码 |
| `codegraph-rust_callers` / `codegraph-rust_callees` | 调用关系 |
| `codegraph-rust_impact` | 修改影响邻域 |
| `codegraph-rust_files` | 已索引文件树 |
| `codegraph-rust_search` | 符号位置 |
| `codegraph-rust_status` | 索引状态与待同步变更 |

### HTTP 传输

支持 HTTP 的客户端可以连接 `/mcp`：

```sh
export CGR_HTTP_TOKEN="replace-with-your-token"
cgr serve --http 127.0.0.1:7878 --path /absolute/path/to/project
```

该示例监听 `http://127.0.0.1:7878/mcp`；配置 Token 后，请求需要携带 `Authorization: Bearer <token>`。
也可以用 `--token` 传入。请根据需要访问源码的客户端选择监听地址和访问凭据。

## 配置

### 项目配置

在项目根目录放置 `cgr.json`。兼容读取上游的 `codegraph.json`；两个文件同时存在时，
`cgr.json` 提供的字段优先。

```json
{
  "exclude": ["**/generated/**", "**/fixtures/**"],
  "include": ["packages/shared/src/**"],
  "deprioritize": ["third_party/**"],
  "includeIgnored": false,
  "extensions": {
    ".customts": "typescript"
  }
}
```

| 字段 | 含义 |
|---|---|
| `exclude` | 使用 gitignore 风格排除文件，也可排除已跟踪文件 |
| `include` | 重新纳入被用户忽略规则排除的路径，不是源码白名单 |
| `deprioritize` | 保留索引，但在检索中降低相对自有代码的排名 |
| `includeIgnored` | 在文件发现阶段纳入嵌套 Git 仓库 |
| `extensions` | 将额外扩展名映射到已有语言解析器 |
| `webRoot` | 指定 PHP `$_SERVER['DOCUMENT_ROOT']` 根目录（未配置时自动推断） |

显式 `exclude` 优先于 `include`；重新纳入路径时仍受内置目录跳过规则约束。
无效配置会产生警告。扩展名映射只选择已有解析器，不会增加新的语言支持。

文件发现遵循忽略规则，默认跳过常见依赖和构建目录；超过 1 MiB 的源码文件不参与提取。
生成文件与测试路径会被记录为标记，供后续排名和分析使用。

### 环境变量

| 变量 | 行为 |
|---|---|
| `CGR_THREADS` | 索引工作线程数，检测到的内存预算可能降低实际数量 |
| `CGR_MEMORY_MB` | 用于线程、批次、kernel 临时映射和解析缓存的调优预算，不是 RSS 硬上限 |
| `CGR_MCP_TOOLS` | 逗号分隔的 MCP 工具列表，兼容回退到 `CODEGRAPH_MCP_TOOLS` |
| `CGR_EXPLORE_DEDUP=1` | 在同一个持久 MCP 对话中跳过已返回源码 |
| `CGR_NO_PROMPT_HOOK=1` | 运行时禁用可选的 Claude Code 提示钩子 |
| `CGR_HTTP_TOKEN` | 要求 HTTP 请求使用 Bearer Token |
| `CGR_LOG` | 服务日志过滤条件，默认 `warn`，写入 stderr |
| `CGR_TIMINGS` | 启用 CLI 索引阶段耗时输出 |
| `CGR_FSYNC` | 默认使用平台同步；`full` 选择最强同步，`off` 跳过同步 |

默认根据可用内存和 CPU 并行度选择索引资源。
例如 `CGR_THREADS=4 CGR_MEMORY_MB=2048 cgr index --timings` 可请求较保守的资源配置；
实际内存占用还取决于图谱规模和中间数据。

## 架构设计

```text
源码文件 → 扫描 → 并行提取与 facts → 合并 → 引用解析与合成边
                                               ↓
CLI / MCP ← 查询处理 ← base + delta 视图 ← mmap 段 ← 发布快照
```

快照中保存冷热节点记录、字符串池、双向 CSR 边、引用解析结果与搜索索引。
发布阶段写入新的不可变文件，再切换 manifest；读者持有稳定快照，通过共享锁防止所用文件被 GC 提前回收。

增量更新比较时间戳、大小及内容哈希，提取变化文件，并重新解析受定义、导入、导出或继承关系影响的引用。
一个 delta 始终针对 base 计算，不会形成无限叠加的层；静默期合并直接重写存活快照，不重新提取。
受影响引用过多或配置变化时仍可全量重建。
正确性目标是增量视图与全量重建一致，通过 canonical 图谱对比验证。

| Crate | 职责 |
|---|---|
| `cg-core` | ID、节点/边类型、配置、资源调优及公共错误 |
| `cg-kernel` | vendoring 的上游提取内核与适配补丁 |
| `cg-scan` | 文件发现、语言检测、忽略规则 |
| `cg-extract` | 并行提取、facts、错误隔离与 hook |
| `cg-store` | 段格式、索引、快照、发布与垃圾回收 |
| `cg-resolve` | 模块/导出表、引用解析、路由 hook 与合成边 |
| `cg-query` | 检索、遍历、源码窗口及共享工具处理 |
| `cg-sync` | 变更检测、增量更新与监听器协调 |
| `cg-mcp` | JSON-RPC、stdio 与 HTTP 传输 |
| `cg-install` | Agent 配置与使用说明块编辑 |
| `cgr` | CLI、提示钩子、SQLite 导出与 benchmark 命令 |

存储与算法的更多细节见[架构文档](docs/ARCHITECTURE.md)。

## 索引生命周期

- `cgr init` 创建 `.cgr/` 并建立索引，内部 `.gitignore` 避免索引内容进入版本控制。
  `init --local-only` 还会在可用时使用 `.git/info/exclude`。
- CLI 查询直接打开已有快照；MCP 进程随客户端保持运行，并可监听源码变化，无需独立数据库服务。
- 编辑后通过 `cgr sync` 更新；`cgr index` 执行全量重建，也可用于刷新依赖新引擎分析规则的索引。
  `status` 显示索引版本和待同步变更。
- `cgr export --sqlite graph.sqlite` 生成供分析使用的独立 SQLite 文件，不会替换原生快照存储；
  覆盖已有导出时使用 `--force`。
- `cgr uninstall` 移除 Agent 集成；`cgr uninit --yes` 单独删除项目索引，保留源码。

索引与导出图谱包含由代码派生的信息，应按所属仓库的访问与保留策略管理。

## 开发与验证

```sh
cargo test --workspace --locked
cargo clippy --workspace --exclude cg-kernel --all-targets -- -D warnings
cargo build --release --locked -p cgr
```

需要更快的优化开发构建时，可使用 `cargo build --profile fastrel -p cgr`。
release 使用 fat LTO；`fastrel` 减少链接工作，属于独立的性能构建配置。

常用的正确性检查与测量命令：

```sh
cgr status --verify --strict
cgr debug unresolved
cgr debug diff
cgr bench index /path/to/project --runs 3 --json
cgr bench sync /path/to/project --edits 30 --seed 1 --verify --json
cgr bench explore /path/to/project --count 100 --runs 3 --json
scripts/bench-local.sh --help
```

`bench sync` 默认编辑临时副本，只有指定 `--in-place` 才会修改被测目录。
`debug diff` 比较当前图谱和全量重建结果。benchmark 包装脚本提供内存、磁盘、Swap 前置检查及进程树监控，
其运行条件可能需要先关闭其他应用。

本轮性能实现已在 macOS 通过 **137 项测试、1 项忽略**，以及 release 编译、Clippy、格式和 CLI/MCP/提示钩子 smoke 检查。
四个真实项目的初始/编辑/恢复图谱均与全量一致，图谱输出也与旧引擎相同。历史 benchmark 版本曾通过 116 项测试。
实现验证细节见[优化记录](docs/PERFORMANCE-IMPLEMENTATION.md)。`cd69b83` 的十二仓库复测中，
十二个仓库的恢复、`cgr status --verify --strict` 和 `cgr debug diff` 检查全部通过，详见[实测性能](#实测性能)
和[机器可读结果](docs/benchmarks/2026-11-02-twelve-repo.json)。
本机测量与远程 CI 属于不同验证证据。

代码版本 `f82506e` 已通过全部 9 项[远程 CI 检查](https://github.com/NianJiuZst/CodeGraph-Rust/actions/runs/41286327676)：
Linux/macOS/Windows 测试与端到端 smoke、格式和 Clippy、内核来源复现、静态 Linux 构建。
远程正确性检查不作为性能测量结果。

CI 配置包含格式检查、Clippy、内核可重复同步、多平台测试与发布构建检查。
Smoke 脚本覆盖 CLI、MCP 握手与提示钩子。更新内核时使用
`scripts/sync-kernel.sh <upstream-checkout> <commit>`，并验证补丁和提取测试；vendoring 代码保留上游格式，
与 workspace 自有代码的格式检查分开处理。

## 当前限制

- **覆盖度对齐**：语言范围、框架路由、合成边和 explore 返回内容与上游仍有差异，实测文档提供逐仓库对照。
- **静态分析边界**：歧义引用可能保持未解析，动态行为仅覆盖已实现的模式；重要调用链仍应结合源码核查。
- **内存调优**：kernel 临时映射和解析缓存上限可以释放堆内存，但 facts、图谱构建数组和共享表仍随项目增长；
  调优预算不能保证进程 RSS 不超过该值。
- **增量范围**：文件系统事件探测已局部化，内存清单和相对 base 的 delta 仍有较广的处理范围；
  结构变化可能扩大解析/合成。后台合并与 delta 变基仍属于 M6。
- **执行期限**：查询和提示钩子采用协作取消，阻塞的文件系统调用不能被线程期限强制中断。
- **性能测试范围**：当前数据覆盖受限档的十二个仓库；标准档、并发读者延迟、大仓库 RSS 和 Agent 任务效果
  不在本轮测量范围内。

## 参与贡献

建议先阅读[架构文档](docs/ARCHITECTURE.md)和对应 crate。
图谱逻辑改动应聚焦具体行为，为解析或增量规则的变更提供回归用例，并对比增量与全量结果。
扩展语言或框架规则时，请说明支持的语法与观察到的覆盖度。
提交前执行相关测试、格式检查和 Clippy；内核更新应能通过固定的上游提交和补丁重现。

## 致谢与许可证

本项目基于 [Colby Mchenry 的 CodeGraph](https://github.com/colbymchenry/codegraph) 及其 Tree-sitter 提取工作。
vendoring 的上游内核使用 MIT 许可证，固定版本记录在 `crates/cg-kernel/UPSTREAM_COMMIT` 中。

本项目采用 [MIT License](LICENSE)。上游归属与 vendoring 组件说明见
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
