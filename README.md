# CrazyDownloader

Rust 编写的多协议桌面下载工具，支持 **HTTP/HTTPS**、**FTP**、**BitTorrent**
（magnet 与 .torrent）三种协议，界面参照 **Internet Download Manager (IDM)** 的经典布局。

> 代码内部代号为 `rust-dm`（包名、可执行文件名），仓库名 CrazyDownloader。

![主界面](docs/screenshots/main-window.png)

## 功能

### HTTP / HTTPS

- 流式下载，边下边写，不占用大量内存
- **多连接分片加速**：服务器支持 `Range` 时自动开启（默认 8 连接）
- **断点续传**：`.part` 临时文件 + **按区间记录**的续传元数据，重启后继续  
  （多连接场景下各分片进度不同步，因此不能只记「已下载 N 字节」，  
  否则中间的空洞会被误认为已完成，续传后产出损坏文件）
- 令牌桶限速、暂停/继续、失败指数退避重试（默认 5 次）
- 自动识别 `Content-Disposition` / URL 中的文件名

### FTP

- 被动模式（PASV）传输，兼容性最好
- `REST` 命令断点续传
- 目录递归下载（`LIST` + `ListParser` 解析）
- 登录失败、连接超时的清晰错误提示
- URL 内嵌账号密码：`ftp://user:pass@host:port/path`，也支持在设置里填默认账号

### BitTorrent

- **bencode** 编解码（含单元测试）
- `.torrent` 单文件 / 多文件解析，info hash 基于**原始字节**计算
- `magnet:` 链接解析（hex 与 base32 两种 info hash 编码、`x.pe` 直连 peer）
- **ut_metadata**（BEP 9）扩展协议，从 peer 获取 magnet 元数据  
  （分片请求做了流水线，实测可在 1 分钟内取回 480 KB 元数据）
- **DHT**（BEP 5）：KRPC `ping` / `find_node` / `get_peers`，迭代式查找。  
  magnet 自带的 tracker 十有八九已失效，DHT 是找到 peer 的**主要**途径
- **Tracker**：HTTP(S)（BEP 3 / BEP 23 compact）+ UDP（BEP 15，带超时重传）
  - **并发**公告所有 tracker（串行时一个死 tracker 就要等满超时）
  - 同一域名的多个地址**并发竞速**，取最先响应的那个
  - `event` 只在首轮发 `started`，之后发 `none`；周期性公告不带 `event=`
  - 带上真实的 `left`/`downloaded`/`uploaded`（`left=0` 等于宣称自己做种，部分 tracker 因此不给 peer）
  - 连续失败的 tracker 进入 5 分钟冷却，不再每轮拖后腿
  - 内置默认 tracker 列表按**实测**筛过，只保留能返回 peer 的
- **Peer wire protocol**：握手（含 BEP 10 扩展位）、bitfield、have、
  request/piece/cancel、choke/unchoke
- 16 KiB 分块调度，**rarest-first** 策略，每 peer 流水线 32 块（512 KB）
- 每个分片 SHA1 校验后才计入进度，损坏自动重下
- **在途块回收**：peer 断线或"假活"（连着不回包）时，它领走的块会放回池子重新请求
  （不回收的话这些块永远停在 pending 里，所在分片就永远凑不齐）
- **只有分片全齐才算完成**：中途取消/中止会如实报"下载未完成 N/M 个分片"，
  不会再出现"界面显示已完成、文件却是半截"
- 启动时校验已有文件，支持续传；**新建的空文件跳过校验**（4.7 GB 的种子能省掉一分多钟的空转）
- peer 不足时自动用 DHT 补充来源（公网 swarm 里大量地址不可直连）
- 做种（完成后可继续上传）
- **种子文件选择**：工具栏「📄 种子文件」、菜单「文件 → 打开种子文件…」、
  添加对话框里的「📄 选择种子…」都可以调起系统文件对话框挑 `.torrent`；
  也可以把 `.torrent` 直接拖进窗口

#### 磁力链下载失败的排查

界面/日志会逐步打印 tracker 与 DHT 的结果。也可以用自带的命令行工具单独验证：

```bash
# 单独探测一批 tracker 能否公告成功（并发，按耗时排序，打印每个的 peer 数与失败原因）
cargo run --example tracker_probe -- <40 位十六进制 info hash> [tracker_url ...]

# 真正跑一次下载若干秒，打印 peer 数 / 已下载 / 分片校验情况后自动取消
cargo run --example bt_smoke -- "magnet:?xt=urn:btih:..." 60

# 只跑元数据获取，打印每一步（tracker / DHT / ut_metadata）
cargo run --example meta_fetch -- "magnet:?xt=urn:btih:..."

# 先看 peer 从哪来：分别打印 tracker 与 DHT 各拿到多少 peer
cargo run --example dht_probe -- "magnet:?xt=urn:btih:..."   # 或 xxx.torrent

# 单 peer 协议级调试（打印对端回的每条扩展消息）
cargo run --example meta_debug -- <ip:port> <40 位十六进制 info hash>
```

常见结论：

- tracker 全失败但 DHT 有 peer → 正常，DHT 会兜住
- tracker 与 DHT 都是 0 → 种子已无人做种，或本机 **UDP 被防火墙/NAT 阻断**
  （DHT 与 UDP tracker 都依赖 UDP 出站）
- 有 peer 但元数据超时 → 对端不支持 ut_metadata，换一批 peer 即可
- 有 peer、也连上了，但**进度一直是 0%、日志刷"分片校验失败"** → 落盘偏移算错
  （历史上就是 `write_block` 把"文件起始偏移"写成了当前游标，所有块都写到文件开头）。
  这类问题用 `bt_smoke` 最直观：正常应看到"分片校验通过 1/N"逐条递增

### 剪贴板监听

复制到剪贴板里的下载链接会被自动识别，并弹窗询问是否下载：

- 支持 `http(s)://`、`ftp://`、`magnet:`、以及本地 `.torrent` 路径
- 后台线程每 700 ms 轮询一次，**不阻塞界面**
- 识别规则保守，避免误打扰：
  - 整段剪贴板内容就是一条链接 → 直接识别
  - 链接夹在短文本里（如「下载地址：https://…，谢谢」）→ 必须像"文件"
    （路径带扩展名、或带查询串）才认，单纯网站首页不认
  - 一段文本里出现多条链接 → 语义不明确，不弹窗
- 程序启动时剪贴板里已有的内容不算"新复制"，不会一打开就弹窗
- 同一条链接不会反复弹；点「忽略」后本次运行内不再提示
- 弹窗里三个选择：**立即下载**（用默认目录直接开始）、**选择目录…**（打开
  预填好的添加对话框）、**忽略**；右下角可取消「继续监视」
- 「选项」菜单里也能随时开关剪贴板监视

> 剪贴板读取直接用 `user32`/`kernel32`（`OpenClipboard` / `GetClipboardData`），
> 不引入额外 crate。原因：`windows-sys 0.61` 的 raw-dylib 需要 dlltool，
> 在本机 GNU 工具链上会失败（`dlltool could not create import library`）。

### 中文显示（重要）
egui 自带字体只有拉丁字形，**必须挂载一份含 CJK 字形的系统字体**，否则界面
所有中文都会变成空白方块（看起来像乱码）。程序启动时按以下顺序自动探测：

```
C:\Windows\Fonts\msyh.ttc     微软雅黑（推荐）
C:\Windows\Fonts\msyhl.ttc
C:\Windows\Fonts\Deng.ttf     等线
C:\Windows\Fonts\simhei.ttf   黑体
C:\Windows\Fonts\simsun.ttc   宋体
/usr/share/fonts/.../wqy-zenhei.ttc     (Linux)
/System/Library/Fonts/PingFang.ttc      (macOS)
```

找到后插入 `Proportional` / `Monospace` 两族的**首位**，默认字体保留作兜底
（emoji 与符号仍能正常渲染）。若全部探测失败，会在 stderr 打印提示，
界面中文将显示为方块。

### 界面（仿 Internet Download Manager）

整体参照 IDM 6.x 的经典布局：浅灰窗口 + 大图标工具栏 + 左侧分类树 + 白色文件列表。

- **菜单栏**：任务 / 文件 / 下载 / 视图 / 帮助（带 `(T)(F)…` 助记符）
- **大图标工具栏**：图标在上、文字在下，12 个按钮（新建任务 / 继续 / 暂停 /
  全部暂停 / 删除 / 清除完成 / 设置 / 打开目录 / 种子文件 / 全部开始 / 日志 / 关于），
  悬停有浅蓝高亮，未选中任务时相关按钮自动置灰
- **左侧分类树**：`全部下载`（可展开为 压缩包 / 文档 / 音乐 / 程序 / 视频）、
  `未完成`、`已完成`、`BT 任务`、`失败`，点击即筛选右侧列表
- **文件列表**（列顺序与 IDM 一致）：
  | 列 | 说明 |
  | --- | --- |
  | 文件名 | 按类型显示矢量图标（压缩包/文档/音乐/程序/视频/种子） |
  | 大小 | 右对齐 |
  | 状态 | 彩色状态图标 + 文字（已完成 / 百分比 / 排队 / 失败…） |
  | 剩余时间 | 按当前速度估算 |
  | 传输速率 | **迷你面积折线图** + 速率数字 |
  | 上次尝试 | 任务添加时间（`MM-DD HH:MM`） |
  | 描述 | 最近一条状态消息 / 错误原因 |
  - 表头点击排序，排序列显示强调色的 ▲/▼
  - 选中行蓝底白字；下载中的行有一层很淡的进度底色
  - 单击选中、双击打开目录、右键菜单（开始/暂停/打开目录/移除）
- **底部状态栏**：全局上下行速度、活动任务数、连接数、剪贴板监视与限速状态
  （速度用 **EWMA 平滑**，是缓升缓降的连续曲线，不会 0 和峰值来回跳）
- **日志面板**：可折叠，按级别着色
- 所有图标都是 `ui/icons.rs` 里用 `egui::Painter` **现画的矢量图形**，
  不依赖任何图片资源，因此缩放清晰、也不需要打包图标文件

> 只想看排版效果时可以设置 `RUST_DM_DEMO=1` 启动，会塞一批假任务（不会真的下载），
> 方便截图核对界面。

## 构建

### 环境要求

- Rust stable（`rustup`）
- Windows：本项目**不需要** Visual Studio 或 MinGW，直接用 rustc 自带的  
  `x86_64-pc-windows-gnu` 自包含工具链即可（见下方说明）。

### 编译运行

```bash
cd rust-dm
cargo build --release
./target/release/rust-dm.exe
```

首次编译会下载依赖（egui/wgpu 体积较大，约 10~60 分钟，取决于网络）。

系统文件对话框用 [`rfd`](https://crates.io/crates/rfd)：Windows/macOS 的平台后端是内置的，
所以 Cargo.toml 里对 Windows 关掉了默认特性（避免拖进 Linux 专用的 xdg-portal / async-std）。
**注意** `rfd` 选的是 0.15（`windows-sys 0.59`，走预编译 import lib）；
0.17 依赖 `windows-sys 0.61` 的 raw-dylib，需要构建期调用 `dlltool`，
在只带 rustc 自包含工具链的机器上会报 `dlltool could not create import library`。

### 运行测试

```bash
cargo test
```

内置 **92 个测试**（87 单元 + 5 集成）：

- 单元测试：bencode 编解码、torrent 解析、magnet 解析（`+` 空格、  
  base32 内嵌元数据、btmh v2 hash、`x.pe`）、info hash hex/base32 解码、  
  peer 消息帧布局、**握手偏移与 BEP 10 扩展位**、**ut_metadata 消息类型与扩展 ID 解析**、  
  bitfield 位序、tracker 响应解析（HTTP compact / 非 compact / UDP 错误响应）、  
  **HTTP tracker URL 参数分隔符**、**udp:// 地址前缀判断**、**DHT KRPC 报文编解码**、  
  令牌桶限速、FTP URL 解析（大小写保留）、rarest-first 调度策略、URL 工具函数、
  **剪贴板链接识别**（整段链接 / 夹杂在文字中 / 多条链接 / 尾部标点 / `.torrent` 路径）；
  另有 `cjk_font_covers_chinese_glyphs` 用 ttf-parser 解析所选字体，
  逐个断言「中/文/下/载/速/度/任/务」都有字形，防止字体回归导致界面变方块。
- 集成测试（`tests/http_download.rs`）：起一个支持 Range 的本地 HTTP 服务器，  
  端到端验证
  1. 8 连接分片下载结果与源字节**完全一致**，且完成后清理残留；
  2. 服务端不支持 Range 时正确退化为单流；
  3. 中途取消后续传，最终文件**无空洞**；
  4. 续传元数据的空洞规划不重不漏，总大小变化时元数据作废；
  5. 限速开启时仍能正常完成。

### 本机编译环境说明（Windows / 无 MSVC / 无 MinGW）

rustup 的 `x86_64-pc-windows-gnu` 工具链自带 `rust-lld`、`dlltool` 及大部分  
MinGW import lib。本项目在其基础上补了两样东西：

1. **补齐 `dlltool` 需要的汇编器 `as`**  
   `windows-sys` / `parking_lot_core` / `mio` 使用 `#[link(kind = "raw-dylib")]`  
   生成导入库，rustc 会调用 self-contained 目录里的 `dlltool.exe`；  
   而 GNU `dlltool` 又会调用同目录下的 `as`。rustc 没有附带 `as`，需自行放入：
   ```bash
   SELF=~/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained
   cp <msys2>/mingw64/x86_64-w64-mingw32/bin/{as.exe,ar.exe} "$SELF/"
   export PATH="$SELF:$PATH"
   ```
2. **补齐 `libshlwapi.a`**  
   self-contained 目录里没有它，而 egui 依赖的 `windows 0.52` 需要：
   ```bash
   LIB=~/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/lib/self-contained
   cp <msys2>/mingw64/lib/libshlwapi.a "$LIB/"
   ```

> 上述 `as.exe` / `libshlwapi.a` 可从 MSYS2 仓库的  
> `mingw-w64-x86_64-binutils` 与 `mingw-w64-x86_64-crt` 包中取得。  
> 若机器上已装 Visual Studio Build Tools 或 MinGW-w64，则无需任何额外步骤。

## 使用

1. 点击「添加」，粘贴地址：
   - `https://example.com/file.zip` — HTTP 直链
   - `ftp://user:pass@host/path/file.tar` — FTP
   - `magnet:?xt=urn:btih:...` — BT
   - `D:\downloads\x.torrent` — 本地种子文件
2. 选择保存目录，点「开始下载」。
3. 任务列表中可随时暂停/继续；双击行打开所在文件夹。

### 快捷操作

| 操作 | 说明 |
| --- | --- |
| 单击行 | 选中任务 |
| 双击行 | 在资源管理器中打开保存目录 |
| 点击表头 | 按该列排序 |
| 悬停行 | 查看详细状态 |
| `Ctrl+N` | 新建任务（打开添加对话框） |
| `Ctrl+O` | 打开种子文件对话框 |
| `Delete` | 移除选中任务 |
| `Space` | 暂停 / 继续选中任务 |
| 拖入 `.torrent` | 自动填入添加对话框 |

## 项目结构

```
rust-dm/
├── Cargo.toml
├── examples/
│   ├── dht_probe.rs        tracker / DHT peer 发现排查工具
│   ├── meta_fetch.rs       只跑 magnet 元数据获取（BEP 9）
│   ├── meta_debug.rs       单 peer 的 ut_metadata 协议级调试
│   └── file_picker.rs      单独验证系统文件/目录对话框
├── scripts/
│   ├── screenshot.py       启动程序并截屏（可选注入剪贴板文本）
│   ├── capture_window.py   截取已存在窗口（PrintWindow，支持裁剪放大）
│   └── ui_click.py         模拟点击/按键（纯 ctypes，用于界面交互核对）
├── tests/
│   └── http_download.rs    HTTP 引擎端到端测试（内置 mock HTTP 服务器）
└── src/
    ├── lib.rs              库入口：暴露 common / engine / ui，供集成测试调用
    ├── main.rs             二进制入口：创建引擎 + 启动 eframe
    ├── common/
    │   ├── task.rs         任务数据模型（Task/Source/TaskState/Config/SortKey）
    │   ├── clipboard.rs    剪贴板监听 + 下载链接识别（Win32 API 直读）
    │   ├── limits.rs       令牌桶限速器（异步 + 阻塞两套接口）
    │   └── util.rs         人类可读格式化、文件名处理
    ├── engine/
    │   ├── mod.rs          引擎调度：任务管理、协议分派、速率统计
    │   ├── http.rs         HTTP 下载器（分片 + 续传）
    │   ├── ftp.rs          FTP 下载器（PASV + REST）
    │   └── bt/
    │       ├── bencode.rs      bencode 编解码
    │       ├── dht.rs          DHT（BEP 5）：KRPC + 路由表 + 迭代 get_peers
    │       ├── metainfo.rs     .torrent / magnet 解析
    │       ├── peer.rs         peer wire protocol（含 BEP 9/10 扩展）
    │       ├── tracker.rs      HTTP/UDP tracker
    │       └── session.rs      下载会话：调度、校验、做种
    └── ui/                  egui 界面（仿 IDM 布局）
        ├── mod.rs            App 状态、菜单栏、状态栏、日志面板、各类对话框
        ├── theme.rs          IDM 配色与尺寸常量
        ├── icons.rs          全矢量图标（工具栏大图标 / 文件类型 / 状态）
        ├── toolbar.rs        大图标工具栏（图标在上、文字在下）
        ├── sidebar.rs        左侧分类树 + 按分类筛选
        └── filelist.rs       文件列表（表头排序、进度底色、迷你速率折线）
```

## 已知限制

这是一个功能完整的下载工具，但为保持实现聚焦，部分高级特性未包含：

- **DHT 只实现了下载侧**：`get_peers` / `find_node` 已接入，但本程序不向 DHT 网络
  宣告自己做种（`announce_peer` 未实现），也不响应其他节点的查询
- **BT 做种上传**：状态机支持 `Seeding`，但当前版本下载完成后即结束会话，未实现主动响应他人请求
- **跨文件边界的块写入**：BT 分块若跨越文件边界会被跳过（绝大多数 torrent 不会触发）
- **PEX**：`ut_pex` 消息构造已实现，但未接入调度循环（peer 来源为 tracker + DHT）
- FTP 目录浏览需手动输入路径，未提供交互式远程文件选择器

## 许可

MIT
