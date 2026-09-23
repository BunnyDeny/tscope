# 从零构建 Windows 单文件 exe（零依赖交付）

> ⚠️ **已废弃（仅供参考）**：本项目已定位为 **Linux-only**（见 README「平台定位」）。
> Windows 实测中 J-Link 的 USB 传输偶发超时且粘死，已放弃 Windows 支持；
> 本文档仅保留交叉编译技术记录，不再维护与验证。

目标：在 Linux 上用交叉编译产出**一个 `tscope.exe`**，把全部依赖静态打进 exe，
Windows 用户拿到 exe + 配置文件即可使用，不用自己装任何软件依赖。

本文假设你**只有 Rust 工具**（rustup + cargo），其余从零开始。
所有命令均为本仓库在 Arch Linux + rustc 1.98 上的实测记录。

---

## 一、原理（三分钟版）

交叉编译三要素：

| 要素 | 作用 | 安装方式 |
|---|---|---|
| target `x86_64-pc-windows-gnu` | 让 rustc 为 Windows 生成代码 | `rustup target add`（纯用户级） |
| mingw-w64 链接器 | gcc 把目标文件链成 exe | 发行版软件包（见下） |
| 目标平台 Rust 标准库 | rustup 自动附带 | 无需操作 |

选 `gnu` 而非 `msvc`：msvc 需要微软 SDK，Linux 上不现实；gnu（MinGW）是社区标准路线。

静态链接三层结构（为什么能打成单 exe）：

1. **Rust crate 层**：天然静态链接进二进制；
2. **C 库层**：本仓库依赖树审计结果——**没有任何需要交叉编译的 C 库**
   （nusb 纯 Rust USB 库、hidapi 2.6 在默认配置下不拉 C 绑定、flate2 走纯 Rust 压缩）；
3. **系统 DLL 层**：kernel32/user32 等 Windows 自带，不算依赖。

唯一的坑是 MinGW 运行时（libgcc / libwinpthread 的 dll），但当前 Rust 工具链
默认已静态处理，实测无需额外参数（排障见第六节）。

## 二、安装工具链

### 1. 添加 Windows 编译目标（rustup，用户级）

```bash
rustup target add x86_64-pc-windows-gnu
```

### 2. 安装 mingw-w64 链接器（发行版软件包，需要系统包管理权限）

| 发行版 | 命令 |
|---|---|
| Arch / Manjaro | `sudo pacman -S mingw-w64-gcc` |
| Ubuntu / Debian | `sudo apt install gcc-mingw-w64-x86-64` |
| Fedora | `sudo dnf install mingw64-gcc` |

验证：

```bash
x86_64-w64-mingw32-gcc --version
```

### 3. （可选）wine：Linux 上冒烟测试 exe

```bash
# Arch
sudo pacman -S wine
```

## 三、构建

在仓库根目录：

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

产物：`target/x86_64-pc-windows-gnu/release/tscope.exe`

实测：全依赖树首次编译约 7 分钟（增量后秒级）。

### （可选）strip 减小体积

```bash
x86_64-w64-mingw32-strip target/x86_64-pc-windows-gnu/release/tscope.exe
```

实测 45M → 33M。

## 四、验证依赖清单（关键一步）

```bash
x86_64-w64-mingw32-objdump -p target/x86_64-pc-windows-gnu/release/tscope.exe \
  | grep "DLL Name"
```

判定标准：

- ✅ **允许出现**：`kernel32.dll` `ntdll.dll` `user32.dll` `gdi32.dll` `advapi32.dll`
  `ws2_32.dll` `shell32.dll` `ole32.dll` `oleaut32.dll` `combase.dll` `opengl32.dll`
  `dxgi.dll` `uxtheme.dll` `dwmapi.dll` `setupapi.dll` `cfgmgr32.dll`
  `winusb.dll`（Win8+ 自带）、`api-ms-win-crt-*.dll`（UCRT，Win10/11 自带）
  ——全部是每台 Windows 都有的系统组件；
- ❌ **不允许出现**：`libgcc_s_seh-1.dll`、`libwinpthread-1.dll` 及任何
  非系统 DLL（出现则按第六节处理）。

本仓库实测清单即上述系统 DLL，无任何非系统依赖 ✅。

## 五、交付物与目录结构

Windows 用户机器上只需要：

```
你的目录/
├── tscope.exe                     # 单文件，零软件依赖
├── tscope.yaml                    # 用户配置
├── GD32F50x_Series.yaml           # 芯片描述（GD32F50x 不在 probe-rs 内置列表，
│                                  #   必须随 exe 交付；内置芯片如 STM32 则不需要）
└── Project.elf                    # 用户自己的固件（符号解析/烧录）
```

⚠️ 相对路径：`tscope.yaml` 里的 `description` / `firmware.elf` 一律相对
**配置文件所在目录**解析，所以把上面文件平铺同目录即可，yaml 无需改动。

Windows 侧两个不可避免的现实：

1. **J-Link 驱动**：SEGGER 官方驱动装一次（硬件驱动，不是软件依赖；
   任何调试工具都免不了。CMSIS-DAP 免驱探针则完全无安装）。
2. **系统版本**：Win10/11 自带 UCRT，exe 开箱即用；若需支持 Win7，
   构建时加 `RUSTFLAGS="-C target-feature=+crt-static"` 把 UCRT 也静态打进
   exe（体积变大，且本仓库未实测）。

## 六、排障

| 现象 | 原因 | 解决 |
|---|---|---|
| `error: linker 'x86_64-w64-mingw32-gcc' not found` | 没装 mingw | 见第二节 |
| DLL 清单出现 `libgcc_s_seh-1.dll` / `libwinpthread-1.dll` | 工具链默认动态链运行时 | `RUSTFLAGS="-C link-arg=-static" cargo build --release --target x86_64-pc-windows-gnu` |
| 构建报某 crate 找不到 C 头文件 | 该依赖有 C 代码（本仓库当前没有） | 需交叉编译对应 C 库，或换纯 Rust 替代 crate |
| exe 能生成但双击无反应 | 先排除 yaml 缺失/路径错误 | 在 cmd 里运行看报错；确认交付清单齐全 |

## 七、Linux 上的验证边界（诚实说明）

Linux 交叉编译只能保证：**二进制生成 + 依赖清单干净**。以下必须 Windows 实机验证一次：

- GUI 窗口弹出与渲染（eframe/egui 的 Windows 后端）；
- 探针连接（J-Link 驱动装好后 `tscope list`）；
- 各子命令冒烟（`--help` 可用 wine 在 Linux 先跑一遍）。

有 wine 时可先做零硬件冒烟：

```bash
wine target/x86_64-pc-windows-gnu/release/tscope.exe --help
```

## 附：本仓库实测记录（2026-09-22，Arch Linux + rustc 1.98.1）

- 全依赖树交叉编译：7m12s，0 警告 0 错误
- 产物：`tscope.exe` 45M（strip 后 33M）
- DLL 清单：仅 Windows 系统组件，无 MinGW 运行时 DLL
- 未在 Windows 实机运行验证
