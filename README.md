# BatteryTray · 电量托盘

一个用 Rust 写的 Windows 原生托盘小程序：通过 USB HID **只读查询**无线鼠标、耳机等外设的电量，把结果显示在系统托盘图标和深色简洁面板里。单文件 exe 即可运行，不需要 Python、.NET 或常驻后台服务。

> **关于本项目**：这是我的第一个项目。由 Rust 原生实现。 代码和文档还在持续完善，若有疏漏欢迎提 Issue 交流。
>
> 内置的两份设备配置（瓦尔基里 VK M5、水月雨 GM01 Pro）来自作者自用设备，**不代表其他型号通用**。新设备请走下面的「新增设备」向导重新取证。

---

## 功能一览

- **HID 只读查询**：按 VID/PID、Usage Page/Usage、接口号、报告长度等条件匹配 HID 接口，发送查询报文并解析电量、在线状态、充电状态。
- **系统托盘**：每台设备一个图标，图标上直接显示百分比；右键菜单可打开面板、立即刷新、设置开机自启、调整刷新间隔（30 秒 ~ 30 分钟）。
- **简洁面板**：深色卡片列表，显示电量、状态与更新时间；点击已有设备直接进设置页，可改名、换图标、开关托盘显示或删除。
- **新增设备向导**：AI 分析 / JSON 导入两条路径，都要求「先实测读到电量，才允许保存」，避免把猜出来的协议写进配置。
- **托盘唯一数据源**：托盘负责读设备并发布读数快照，面板只订阅快照，避免两个进程同时抢同一接收器；托盘退出后面板自动回退到本地读取。
- **深色自绘界面**：圆角按钮、开关、图标按钮均由 GDI 自绘，支持 Per-Monitor V2 DPI 感知，高缩放不模糊。
- **命令行接口**：设备枚举、原始报文扫描、协议试跑与写入均可输出 JSON，方便脚本或 AI 助手调用。

---

## 快速开始

### 直接运行（已编译版本）

双击 exe 即启动托盘常驻，托盘图标右键 →「打开面板」进入主界面。配置与运行时数据都放在 `%APPDATA%\BatteryTray\`，目录和文件在**第一次写入时**才创建；在此之前程序使用默认间隔和上面提到的两份内置设备定义。

### 从源码编译

环境要求：

| 项目 | 要求 |
| --- | --- |
| 操作系统 | Windows 10 / 11，x86_64（代码大量使用 Win32 API，不支持其他平台） |
| Rust | 1.85 及以上（在 1.99.1 `stable-x86_64-pc-windows-gnu` 上验证） |
| 链接器 | GNU 工具链需 MinGW-w64 的 `gcc`/`ld`；换 MSVC 工具链则需 Visual Studio Build Tools |
| 字体 | 微软雅黑 UI（界面默认字体，缺失时回退系统字体） |

```bash
cargo build --release
```

产物在 `target\release\battery-tray.exe`。把它复制到任意目录即可单独运行；仓库里的 `.gitignore` 会排除 `target\`、编译产物和本机配置，不会误提交。

安装入口（三选一，按需执行）：

```cmd
battery-tray.exe --install
battery-tray.exe --install --no-startup
battery-tray.exe --install-desktop
```

依次对应：创建桌面入口并设置开机自启、只建桌面入口不动自启、只建桌面快捷方式。开机自启通过「启动」文件夹的快捷方式实现，托盘右键菜单里可以随时开关。

> **注意**：exe 以 Windows GUI 子系统编译，**直接双击不会弹出控制台**。命令行模式的输出需要重定向到文件或用管道读取，例如 `battery-tray.exe --catalog > catalog.json`，或在 Git Bash / Python 里捕获 stdout。

---

## 界面与操作

### 托盘

右键菜单项：打开面板、立即刷新、开机自启、刷新间隔（30 秒 / 60 秒 / 2 分钟 / 5 分钟 / 15 分钟 / 30 分钟）、退出。默认间隔 60 秒，配置值会被限制在 30–1800 秒之间。

### 主面板

顶部工具栏有「新增」和「刷新」。卡片按启用顺序排列，显示状态文字：正常、充电中、离线、未找到、休眠、不支持、占用中、错误、已关闭、等待读取。

- 点「新增」打开添加设备向导（子进程，独立窗口）。
- 点某张设备卡片打开同一个窗口的设置页，可修改或删除该设备。
- 托盘在后台运行时，面板每 1.5 秒读取一次托盘发布的快照；点「刷新」只是投递一个请求标记，由托盘自己去读设备。托盘没运行时面板会自行读取。

### 新增设备向导（一窗四页）

1. **导入方式**：二选一 —— 「AI 分析」或「JSON 导入」。
2. **选择设备**（仅 AI 分析）：列出当前电脑的 HID 设备，按序列号聚合，每 2 秒在后台线程刷新一次；新接入的设备标为「刚接入」，拔掉再插回也会重新标记。页面提示可「通过拔插设备确认目标设备」。选中的设备信息会作为证据写进提示词。
3. **粘贴 JSON**：AI 分析路径可「复制提示词」（含设备型号、VID/PID、接口列表等取证信息）交给 AI，拿回协议 JSON 后粘贴；JSON 导入路径直接粘贴已有协议 JSON。点「读取并导入」后程序会按协议实测一次，**只有成功读到电量**才进入设置页，失败会留在当前页并给出原因。
4. **设备设置**：填设备名，从四个内置图标里选一个或上传本地图片，用开关决定是否「托盘显示」，然后「保存并添加」；编辑已有设备时按钮变为「保存」，并多出「删除设备」。

保存只合并协议 JSON 里的 `match` / `request` / `response`，本地的 `key`、`uid`、`name`、`icon`、`tray` 等身份与显示信息保持不变，避免 AI 返回的内容改错目标设备。

---

## 命令行参数

不带参数 = 托盘。以下命令均以 `battery-tray.exe` 为例（实际文件名可能是 `电量托盘 Rust.exe`）。

| 命令 | 作用 |
| --- | --- |
| `--help` / `-h` | 打印用法 |
| `--panel` | 打开简洁面板 |
| `--editor [--key KEY]` | 打开新增向导；带 `--key` 时直接编辑指定设备 |
| `--debug-ui` | 打开 HID 调试窗口（枚举接口、读取 Feature/Input、「粘贴并测试」协议 JSON） |
| `--catalog` | 输出 JSON：配置、设备与全部 HID 接口 |
| `--scan --vid 0x374A --pid 0xA322 [--mi N] [--passive-ms N]` | 只读收集指定设备的原始响应 |
| `--send --hex "00 20 01" [--mode feature\|output] [--mi N] [--settle-ms N] [--timeout-ms N] [--read-len N]` | 发送一次查询报文并读取响应 |
| `--apply-profile device.json --dry-run` | 试读电量但不写配置（推荐先跑这步） |
| `--apply-profile device.json --test` | 正式写入配置并实测 |
| `--apply-profile -` | 从标准输入读取协议 JSON |
| `--once [--debug]` | 打印一次全部设备读数后退出 |
| `--probe` | 逐个读取启用设备并打印状态行 |
| `--list-hid` | 以可读文本列出 HID 接口 |
| `--install [--no-startup]` | 创建桌面入口、设置开机自启 |
| `--install-desktop` | 仅创建桌面快捷方式 |

`--catalog` / `--scan` / `--send` / `--apply-profile` 输出结构化 JSON，顶层 `ok` 为 `true` 时退出码 0，否则为 1，便于脚本判断。

示例：

```cmd
battery-tray.exe --catalog > catalog.json
battery-tray.exe --scan --vid 0x374A --pid 0xA322 > scan.json
battery-tray.exe --apply-profile device.json --dry-run > dryrun.json
battery-tray.exe --apply-profile device.json --test > applied.json
```

> 发送自定义报文前请确认该命令只是查询，不会改变设备持久状态（固件、配对、存储等）。程序内置的 AI 提示词也把「只读」列为硬性约束。

---

## 配置与数据文件

全部位于 `%APPDATA%\BatteryTray\`：

| 文件 | 说明 |
| --- | --- |
| `config.json` | 主配置：`interval`（秒）、`devices`（设备数组）、可选的 `calibration`（内置协议的偏移微调） |
| `readings.json` | 托盘发布的读数快照（含递增序号与时间戳），面板订阅它；托盘退出时删除 |
| `refresh.request` | 面板/外部投递的「请求刷新」标记，托盘每 500 ms 检查一次；与快照一样在托盘退出时清理 |
| `config.json.corrupt-<时间戳>` | 配置文件损坏时自动备份的原始内容，不会被覆盖 |

配置采用「临时文件 + 原子替换」写入；进程内还有基于 `mtime + size` 的缓存，文件没变就不重复读盘和解析，外部改动会立即使缓存失效。

一个最小的设备配置（字段含义见下节）：

```json
{
  "interval": 60,
  "devices": [
    {
      "key": "mouse",
      "name": "瓦尔基里 VK M5",
      "protocol": "vk_mouse",
      "enabled": true,
      "show_in_tray": true,
      "uid": 1,
      "icon": "mouse",
      "icon_path": "",
      "match": {
        "vid": 14154,
        "pid": 41762,
        "usage_page": 65535,
        "usage": 2,
        "interface_number": -1,
        "product_contains": "",
        "input_len": 0,
        "output_len": 0,
        "feature_len": 0
      },
      "request": {
        "bytes_hex": "00 F7",
        "settle_ms": 50,
        "timeout_ms": 1000,
        "feature_len": 0,
        "read_len": 0
      },
      "response": {
        "battery_offset": 3,
        "online_offset": 5,
        "online_equals": 0,
        "report_id": 0
      }
    }
  ]
}
```

示例里的 `vid` / `pid` 是十进制（这里对应 `0x374A` / `0xA322`）；JSON 不支持十六进制写法，命令行参数则两种都接受。字段可只写需要覆盖的部分，缺失项会由程序补默认值。手工改配置建议在托盘退出后进行；不确定时优先用向导或 `--apply-profile --dry-run`。

### 协议 JSON 字段

向导与 `--apply-profile` 接受单个对象或对象数组，关键字段：

| 分组 | 字段 | 含义 |
| --- | --- | --- |
| 设备 | `name` `key` `protocol` `enabled` `show_in_tray` `icon` `icon_path` | 名称、稳定键、协议类型、是否启用、是否显示托盘图标、内置图标或本地图片绝对路径 |
| `match` | `vid` `pid` `usage_page` `usage` `interface_number` `product_contains` `input_len` `output_len` `feature_len` | HID 接口匹配条件；`-1` / `0` / 空值表示不限制 |
| `request` | `bytes_hex` `settle_ms` `timeout_ms` `feature_len` `read_len` `cmd` | 查询报文（十六进制，含 Report ID）、写后等待、读取超时等 |
| `response` | `battery_offset` `battery_bytes` `battery_endian` `battery_mask` `battery_shift` `battery_scale` `battery_adjust` `valid_offset` `valid_equals` `online_offset` `online_equals` `charging_offset` `charging_mask` `charging_equals` `report_id` | 从响应中提取电量与状态；**所有偏移都相对包含 Report ID 的完整响应**，`-1` 表示禁用该检查 |

支持的 `protocol`：`vk_mouse`、`darkrim`（两个内置专用协议），以及通用的 `hid_feature`、`hid_output_input`。前者用固定的查询流程，后两者完全由 `request` / `response` 驱动。

---

## 运行逻辑速览

- 托盘是唯一数据源：按 `interval` 定时刷新，结果既用于更新托盘图标，也写入 `readings.json`；面板不再自己起 HID worker。
- 跨进程设备锁按**实际接口路径**（找不到接口时回退 VID:PID + Usage + MI）命名，两个配置指向同一接收器会共用一把锁，等待超时 2.5 秒。
- 设备离线时查询会等待超时，结果以「离线 / 未找到 / 占用中」等状态体现在读数里，读设备的工作不会阻塞界面线程。
- 新增向导的设备枚举跑在后台线程，UI 线程只负责把结果填进列表，避免 2 秒一次卡顿。

## 项目结构

| 路径 | 职责 |
| --- | --- |
| `src/main.rs` | 程序入口，GUI 子系统，启动分发 |
| `src/lib.rs` | 库模块声明；协议、配置、策略等可测试部分与 Windows 实现分离 |
| `src/platform/tray.rs` | 托盘主循环、右键菜单、图标刷新、请求轮询 |
| `src/platform/gui/panel.rs` | 简洁面板（卡片、新增、刷新） |
| `src/platform/gui/editor.rs` | 新增设备向导，四页流程与协议合并 |
| `src/platform/gui/theme.rs` | 深色主题、圆角控件与开关的 GDI 自绘 |
| `src/platform/gui/debug.rs` | HID 调试窗口 |
| `src/platform/gui/task.rs` `mod.rs` | 后台任务封装、DPI 与窗口模式分发 |
| `src/platform/hid.rs` | HID 接口枚举、报文读写、设备锁 |
| `src/platform/sync.rs` `shell.rs` | 进程单例 / 互斥；快捷方式、开机自启、注册表 |
| `src/battery.rs` | 各协议的具体读取实现与调度 |
| `src/candidates.rs` | 设备候选聚合、取证信息、种子配置 |
| `src/profiles.rs` | 设备配置读写、校验、AI 提示词模板 |
| `src/config.rs` | `config.json` 原子读写与进程内缓存 |
| `src/protocol.rs` `refresh.rs` `policy.rs` | 读数结构、刷新策略、状态判定 |
| `src/snapshot.rs` | 跨进程读数快照与刷新请求 |
| `src/icon.rs` `error.rs` `device_api.rs` | 图标绘制、错误类型、命令行接口 |
| `tests/integration.rs` | 集成测试 |

## 开发

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

当前有 32 个单元测试 + 6 个集成测试，覆盖配置读写与损坏备份、协议字段归一化、缓存失效、快照往返、锁名硬件身份、向导保存落盘等。项目启用了较严格的 lint：禁止 `unwrap` / `expect` / `panic`，所有 `unsafe` 块必须写明安全理由。界面代码依赖真实窗口环境，主要靠手工验证（托盘启动、面板订阅、`--catalog` / `--once`、向导保存）。

---

## 常见问题

**面板数据一直不更新？** 先确认托盘进程在跑；托盘没运行时面板会回退本地读取，且只在打开时读一次。也可以点面板的「刷新」。

**设备显示「未找到」？** 多半是 VID/PID 或 Usage 不匹配、接收器没插、或被别的程序占用了 HID 接口。用 `--debug-ui` 枚举接口，或 `--catalog` 导出完整清单核对。

**命令行执行后什么都没打印？** exe 是 GUI 子系统程序，输出需要重定向：`battery-tray.exe --once > once.txt`，再打开该文件查看。

**配置改坏了？** 程序会把损坏的原文件另存为 `config.json.corrupt-<时间戳>` 后回退默认值，不会静默覆盖；把备份改回来即可。

**图标能自定义吗？** 可以，在设备设置页上传本地图片（支持 PNG / ICO / BMP），路径会写进配置，换机器时需要把图片一起带走。

---

## 许可证

`Cargo.toml` 中声明为 `MIT OR Apache-2.0`。仓库目前尚未附 `LICENSE` 文件，正式分发前建议补上一份，与元数据保持一致。
