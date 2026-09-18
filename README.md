# tscope

基于 [probe-rs](https://probe.rs) 库的嵌入式调试 / 监视工具。用 **YAML 配置文件 + ELF 调试信息**，
在命令行里按名字直接查看目标板上运行中的变量——标量、数组、结构体、联合体、枚举，
支持成员路径访问，全程**不暂停 CPU**。

```
$ tscope var ENC_1_POS_SENSOR.readAngleCmd
ENC_1_POS_SENSOR.readAngleCmd = 65535
类型: int    地址: 0x20007554

$ tscope var ENC_1_POS_SENSOR
ENC_1_POS_SENSOR (positionStruct) = {
    position: -3.03268 (float)
    ElecPosition: 1.5247769 (float)
    rotations: -1 (int)
    ...
}
```

## 特性一览

- `list`：列出本机调试探针（排查连接问题）
- `read`：读取任意地址内存（32 位字 / 按字节）
- `var`：按符号名读取全局变量，类型感知打印
  - 标量（float / int / uint8_t …）、数组（含多维）、结构体、联合体、枚举，任意嵌套
  - 成员路径：`ENC_1_POS_SENSOR.readAngleCmd`、`items[0].v.x`、`matrix[1][2]`
  - 数组默认显示前 16 个元素，`--count N` / `--all` 控制
  - 指针成员只显示地址（`NULL` 显示 NULL），位域明确提示暂不支持
- `watch`：Keil Watch 风格的实时刷新窗口（ratatui 交替屏表格，退出自动恢复终端）
  - 监视组定义在 `tscope.yaml` 的 `watch` 节，可定义多组（采样周期各自可调）
  - 值变化黄色高亮、读取失败红色显示，采样期间不暂停 CPU
- `flash`：把 `firmware.elf` 烧录到芯片（probe-rs 内置烧写算法，终端里多阶段进度条）
  - 默认**扇区擦除**：只擦 ELF 覆盖的扇区，bootloader 不受影响；烧写后自动回读校验
  - `--erase_all` 整片擦除：永久删除 bootloader，执行前必须输入 `yes` 确认（`--yes` 跳过，供脚本）
  - 烧完复位并暂停在复位向量；调试器断开后内核从复位向量继续运行，**新固件自动从头启动**
    （bootloader 跳转 APP），无需手动复位——未启动时重新上电或 `debug` 会话里 `rst` 兜底
- `debug`：交互式调试会话（提示符 `> `，readline 行编辑：左右光标、↑/↓ 历史命令且跨会话保存）
  - **持久断点集**（会话内有效）：`bp` 添加（地址 / 函数名 / **文件:行号** 三种写法）、`bl` 列出、`bc` 删除
  - 断点命中/暂停时显示 `文件:行号 + 函数名`（DWARF 行号表反查）；`reset` 后自动重新应用断点
  - 源码级单步 `s`（gdb 同款：逐指令步进直到行号变化或离开函数，自动走出
    中断并落在被打断的代码）；`si` 指令级；`finish` 一步运行到当前函数返回
  - `bt` 函数调用栈回溯（.debug_frame 展开 + VTOR 栈顶边界）；正常调用链完整，
    中断（ISR）内暂停时回溯到中断服务函数为止（不追溯被打断的任务），
    不依赖 FPU 或具体芯片型号
  - `list`（l）显示当前 PC 附近源码 ±5 行（gdb 风格 `=>` 标记当前行）；
    路径来自 ELF 编译时记录——源码在本机则直接显示，不在则明确报错（零强制依赖）
  - 全速运行、暂停、单步、寄存器、复位到 main；会话内可随时 `var` 读全局变量、`watch` 持续监视（q 返回提示符，会话不断）
  - 单条命令出错不影响会话；只支持硬件断点（8 个上限），不碰 flash，bootloader 安全
- 芯片由运行时查询 probe-rs 内置列表判定；内置没有的型号走「芯片描述 YAML」，
  缺文件时给出 target-gen 生成指引
- 库直接内嵌：单进程，无中间服务；一次内存读取完成整个变量（含结构体）的采集

## 架构一瞥

```
tscope（本程序，内嵌 probe-rs 库） ←SWD→ 调试探针 → 目标芯片
```

> 与 VSCode 调试走的 `probe-rs dap-server` 模式不同：tscope 自己就是探针的客户端。
> 一个探针同一时刻只应被一个进程使用，运行 tscope 前请退出 VSCode 调试会话。

---

# 一、部署（源码构建）

tscope 以源码方式构建；probe-rs 等依赖在构建时由 cargo 自动从 crates.io 下载，
**无需手动克隆任何其他仓库**。下文假设 `cargo` 已可用（Rust 工具链已安装）。

## 1. 获取源码

```bash
git clone git@github.com:BunnyDeny/tscope.git
cd tscope
```

## 2. 构建

```bash
cargo build --release
```

首次构建需下载并编译 probe-rs 及全部依赖（约 200 个 crate），耗时数分钟；
之后增量构建秒级。产物：`target/release/tscope`（Windows 下为 `tscope.exe`）。

> 依赖版本：`Cargo.toml` 锁定 `probe-rs = "0.32"`，与工具开发时使用的
> probe-rs 版本一致；上游发布破坏性变更时再显式升级。

可选安装方式（二选一）：

```bash
cargo install --path .              # 安装到 ~/.cargo/bin/
# 或
sudo cp target/release/tscope /usr/local/bin/
```

## 3. 平台差异

| 平台 | 说明 |
|---|---|
| Linux | 构建后还需配置 **udev 规则**（见下节），否则普通用户打不开探针 |
| Windows | 构建过程相同（PowerShell 里跑同样的命令）；J-Link 需要 **WinUSB 驱动**（见下节） |
| macOS | probe-rs 对 J-Link 原生支持，无需额外配置（未实测） |

---

# 二、Linux：USB 权限（udev 规则）

Linux 下调试探针默认只有 root 能访问。不配置这一步，tscope 要么列不出探针，
要么报 `Failed to open the debug probe: ... permission denied (errno 13)`。

## 1. 安装官方规则文件

```bash
sudo curl -L https://probe.rs/files/69-probe-rs.rules \
     -o /etc/udev/rules.d/69-probe-rs.rules
sudo udevadm control --reload
sudo udevadm trigger
```

## 2. ⚠️ 重新插拔调试器（实测关键步骤）

光执行 `udevadm trigger` 往往不够：已经打开/占用的设备节点未必会拿到新权限。
**必须把调试器从 USB 口拔下来再插回去**（换 USB 口或重启系统同样有效）。
这一点已在 J-Link 上实测验证：只 reload + trigger 仍报 `errno 13`，拔插后立刻正常。

## 3. 仍无法访问时的兜底方案

```bash
sudo groupadd --system plugdev
sudo usermod -a -G plugdev $USER
```

> systemd 版本高于 v258 时 `plugdev` 必须是**系统组**（`--system` 参数）。
> 改完同样需要**重新登录**并**重新插拔**调试器。

## 4. 验证

```bash
tscope list
```

应能看到你的探针，形如：

```
[0] J-Link_J-Link V11.00     vid=0x1366 pid=0x0105 sn=000601028364
```

---

# 三、Windows：J-Link 驱动切换

**probe-rs 与 SEGGER 官方 Windows 驱动不兼容**（官方文档明确说明），
必须把 J-Link 切换到通用 **WinUSB** 驱动。

**方法 A（官方推荐）：J-Link Configurator**

1. 下载 [J-Link Configurator](https://www.segger.com/products/debug-probes/j-link/tools/j-link-configurator/)
2. 连接 J-Link，启动 Configurator
3. 在设备列表中右键你的探针 → **Configure**
4. **USB Driver (Windows)** 选择 **WinUSB** → OK

**方法 B（A 的选项被禁用时）：Zadig**

用 [Zadig](https://zadig.akeo.ie/) 为你的探针安装 WinUSB 驱动。

> ⚠️ 切换到 WinUSB 后，SEGGER 官方工具（J-Flash、J-Link Commander 等）
> 可能无法再识别该探针，需要时可切换回官方驱动。

验证：

```powershell
.\target\release\tscope.exe list
```

---

# 四、配置 tscope.yaml

tscope 的每次执行都由一个 YAML 配置文件驱动（默认 `./tscope.yaml`，
可用 `--config` 指定）。配置只描述"环境"（探针、芯片、固件），
命令行参数只描述"操作"（读哪个地址、看哪个符号）。

**通用规则：**

- 配置里的相对路径（`chip.description` / `chip.pack` / `firmware.elf`）
  **一律相对配置文件所在目录**解析，与你在哪个目录运行无关
- 字段名拼错会直接报错（`unknown field`），不会静默忽略
- `version` 字段用于将来格式演进，当前固定为 `1`

## 完整模板（以 GD32F503RE + J-Link 为例）

```yaml
version: 1

probe:
  protocol: swd            # swd | jtag
  speed_khz: 1000          # SWD 时钟（kHz）；本板实测 >1000 不稳定，勿调高
  # 多个探针同时存在时才需要，用于选定目标：
  # selector:
  #   vid: 0x1366          # SEGGER
  #   # pid: 0x0101
  #   # serial: "000123456789"

chip:
  name: GD32F503RE
  description: ../uni_software/bsp/gd/targets/GD32F50x_Series.yaml
  # pack: ../path/to/GigaDevice.GD32F50x_DFP.1.0.1.pack

firmware:
  elf: ../uni_software/bsp/gd/build/Project.elf
```

## 各字段说明

### `probe` —— 调试器

| 字段 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `protocol` | 否 | `swd` | 调试协议：`swd` 或 `jtag`；写错直接报错 |
| `speed_khz` | 否 | `1000` | SWD 时钟，单位 kHz。**本板红线是 1000**：实测 4000 会丢数据导致烧录/读取失败，省略或调低才安全 |
| `selector` | 否 | 无 | 多探针时按 `vid` / `pid` / `serial` 选定目标（三项可只写部分，同时满足才匹配）。单探针留空即可，留空但发现多个探针时会报错并列清单 |

整个 `probe:` 节都可以省略，此时全部使用安全默认值（swd / 1000 kHz / 不筛选）。

### `chip` —— 芯片（重点）

**核心机制：芯片是否被 probe-rs"官方内置支持"，不由你写进配置**——
程序运行时拿 `chip.name` 去查内置列表，查不到时按下面流程逐级回退：

```
chip.name（如 "GD32F503RE"）
    │
    ├─ ① 内置列表里有 → 直接用（零额外文件）
    │
    └─ 没有（如 GD32F50x 整个系列）
        ├─ ② 配置里有 chip.description 且文件存在
        │      → 加载描述文件 → 按 name 取变体
        │      → 变体不存在 → 报「描述文件里没有这个型号」
        │
        ├─ ③ 描述文件缺失 → 报错并打印 target-gen 生成指引
        │      （配置了 chip.pack 时直接给出可复制的完整命令）
        │
        └─ ④ 什么都没配 → 报错，信息带方案 A / 方案 B
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | **是** | 芯片型号。内置支持时只用它；非内置时必须与**芯片描述文件里的变体名精确一致**（可用 `grep "^- name:" xxx_Series.yaml` 查看有哪些变体） |
| `description` | 视情况 | 「芯片描述 YAML」路径——由 probe-rs 的 `target-gen` 工具从厂商 CMSIS-Pack 生成，描述该芯片系列的内存布局与烧录算法。**仅当芯片不在内置列表时需要** |
| `pack` | 否 | 厂商 CMSIS-Pack 路径（`.pack` 文件或 Keil 已解压的目录）。当前仅用于拼错误指引；v3 起程序将自动调用 target-gen 生成描述文件 |

**两种典型用法：**

```yaml
# 场景 1：芯片官方内置支持（多数 STM32、nRF52 等）——只写一个名字
chip:
  name: STM32F407VG

# 场景 2：内置没有（如 GD32F50x）——名字 + 描述文件
chip:
  name: GD32F503RE
  description: targets/GD32F50x_Series.yaml
```

**描述文件从哪来（一次性操作）：**

```bash
# target-gen 是 probe-rs 源码仓库里的工具
cargo install --git https://github.com/probe-rs/probe-rs target-gen
target-gen pack GigaDevice.GD32F50x_DFP.1.0.1.pack ./targets/
# 产物 targets/GD32F50x_Series.yaml 就是 chip.description 要指的文件
```

**三种错误分别长这样（对应上面的 ①–④）：**

| 报错 | 含义 | 对策 |
|---|---|---|
| `芯片 xxx 不在 probe-rs 内置支持列表，且配置指定的芯片描述文件不存在：…` | 描述文件路径不对/文件没了 | 检查路径（相对配置文件目录！） |
| `芯片描述文件 xxx 里没有名为 xxx 的变体` | 文件加载成功，但 `chip.name` 拼写与变体名不一致 | `grep "^- name:"` 核对，或用正确的 pack 重新生成 |
| `芯片 xxx 不在 … 内置支持列表，且配置里没有 chip.description` + 方案 A/B | 非内置芯片但没配描述文件 | 按提示补 `description`，或先跑 target-gen |

### `firmware` —— 固件

| 字段 | 必填 | 说明 |
|---|---|---|
| `elf` | `var` / `watch` 需要 | 固件 ELF 路径。从它的调试信息（DWARF）解析符号地址与类型 |

**关键前提**：ELF 必须与**板上实际烧录的固件**是同一次构建的产物。
烧的是旧固件、指了新 ELF，符号地址会对不上，读出来的是别的数据。
另外符号的可见性受编译优化影响：`-O2` 下部分变量会被优化掉，
读不到时报错提示用 `-O0` 重新编译。

### `watch` —— 实时监视组

定义 Keil Watch 风格的实时刷新窗口。键是组名（`tscope watch <组名>` 的参数），
可定义任意多组：

```yaml
watch:
  watch1:                  # 组名自定
    interval_ms: 100       # 采样周期（毫秒），默认 100
    max_elems: 8           # 复合类型最多展开的项数（数组元素/结构体成员），默认 8
    symbols:               # 语法与 var 命令相同，支持成员路径/下标
      - theta_ref
      - led_ticker
      - ENC_1_POS_SENSOR
      - ENC_1_POS_SENSOR.readAngleCmd
  watch2:
    interval_ms: 500
    symbols:
      - cali_buff
```

| 字段 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `interval_ms` | 否 | `100` | 采样周期（毫秒）。每个基础变量每周期只做一次内存读，采样期间不暂停 CPU |
| `max_elems` | 否 | `8` | 复合类型（数组/结构体）展开成子项行的数量上限；超出部分显示「…余N」汇总行（修正 Keil 大数组全量刷屏的缺陷）。想全看就调大，或直接监视具体成员/下标（`cali_buff[20]`、`s.member`） |
| `symbols` | **是** | — | 要监视的符号表达式列表 |

运行 `tscope watch`（不带组名）可列出配置里定义的所有组。

组名**完全任意**：`watch1` / `watch2` 只是示例，改成 `w1`、`a` 等任意名字
（单个字母也行，`yes`/`no`/`on`/`off` 这类词同样可用，实测无任何格式限制）。
改名后 `tscope watch w1` 打开；debug 会话里 `w w1` 打开（`w` 是 `watch` 的同义词）。
个别严格的 YAML 1.1 工具会把 `yes` 之类当布尔值，若文件还要给别的工具读，
建议加引号（`"yes":`）——tscope 本身不受影响。

---

# 五、使用

```bash
# 探针自检
tscope list

# 读内存（默认 0x20000000，一个字）
tscope read
tscope read --address 0x08000000 --count 2      # bootloader 向量表

# 按符号读变量（标量）
tscope var theta_ref

# 复合类型
tscope var ENC_1_POS_SENSOR                      # 结构体，多行树形打印
tscope var cogging_current                       # 数组，默认前 16 个元素
tscope var cogging_current --all                 # 全部元素
tscope var cogging_current --count 4             # 指定个数

# 成员路径
tscope var ENC_1_POS_SENSOR.readAngleCmd         # 结构体成员
tscope var items[0].v.x                          # 下标 + 嵌套成员
tscope var matrix[1][2]                          # 多维下标

# 指定配置文件
tscope --config /path/to/tscope.yaml var theta_ref

# 实时监视（Keil Watch 风格，q / Esc / Ctrl-C 退出）
tscope watch                    # 列出配置里定义的所有监视组
tscope watch watch1             # 打开 watch1 组的实时刷新表格

# 烧录固件（firmware.elf → 芯片 flash）
tscope flash                    # 默认：扇区擦除 + 烧写 + 校验（终端里显示进度条）
tscope flash --erase_all        # 整片擦除（⚠️ 永久删除 bootloader，须输入 yes 确认）
tscope flash --erase_all --yes  # 跳过确认（危险，仅供自动化脚本）

# 交互式调试会话
tscope debug                    # 进入提示符 "> "，输入 help 查看命令
> bp foc.c:123                  # 断点：文件:行号（也可写函数名/十六进制地址）
> bp main                       # 再添一个；bl 列出全部；bc 1 / bc all 删除
> run                           # 全速运行，命中任断点停下并报 文件:行号+函数
> halt                          # 暂停；regs 寄存器；pc 看 PC/SP/LR
> s                             # 源码级单步（gdb 同款；-O2 下行号可能交错，-O0 最精确）
> si                            # 指令级单步；finish 一步运行到当前函数返回（f / fin 同义）
> bt                            # 函数调用栈回溯（backtrace 同义；bt 10 限制帧数）
> list                          # 显示当前 PC 附近源码（l 同义；也可 l port.c:244）
> reset                         # 复位并暂停在 main 开头（rst 同义；可指定函数）
> var theta_ref                 # 一次性读全局变量
> watch watch1                  # 全屏持续监视，q 返回提示符（会话不断；w watch1 同义）
> q                             # 退出会话
```

`--count` 对多维数组**逐维生效**（每一维都截断到 N 个）。

watch 表格四列：表达式 / 值 / 类型 / 地址。值发生变化的行黄色高亮
（本采样周期内变化），读取失败红色显示；**复合类型展开成子项行**——
数组展平为 `m[0][0]`、结构体展开为 `.member`（嵌套为 `.v.x`，结构体数组
为 `items[0].v.x`），最多 `max_elems` 项，超出显示「…余N」汇总行；
位域成员显示「〈位域暂不支持〉」占位行。行数超过终端高度时可用
**↑/↓（或 j/k）、PgUp/PgDn、Home/End、鼠标滚轮**滚动，选中行深色高亮。
退出时自动恢复终端，不留滚动垃圾。

`flash` 默认**扇区擦除**：只擦 ELF 数据覆盖到的扇区（本板 APP 在
0x08004000 起，bootloader 不受影响），擦除后写入并**回读校验**，
全程自动（本板实测约 7 秒）。终端里显示**多阶段进度条**（擦除/写入/校验
各一条，带字节数、百分比与预计剩余时间；输出被重定向或 TERM=dumb 时
自动退回分行打印，保证任何环境下都有输出）。

烧录收尾：probe-rs 烧完会「恢复现场继续运行」，但恢复的 PC 指向已被
覆盖的旧代码（等于跑乱码——内核报告在运行、固件却没起来）。因此 tscope
烧完后**复位并暂停**在复位向量，随后调试器断开、内核从复位向量继续
运行：**新固件自动从头启动**（bootloader 跳转 APP），无需手动复位。
若固件未启动（个别探针断连行为不同），重新上电或在 `tscope debug` 里
`rst` 即可。`--erase_all` 整片擦除会**永久删除 bootloader**（本仓库
没有备份），所以执行前必须交互输入 `yes` 确认；`--yes` 只用于自动化
脚本，请谨慎。

---

# 六、常见问题

| 现象 | 排查 |
|---|---|
| `没有发现调试探针` | USB 连接；Linux 是否配好 udev 并**拔插过**；Windows 是否切了 WinUSB 驱动；探针是否被 VSCode 调试 / dap-server 占用 |
| `打开探针 … 失败` / attach 报错 | 芯片是否上电；SWD 接线；`speed_khz` 是否 ≤ 1000 |
| flash 后 `debug` 里 `run` 提示「内核正在运行（未暂停）」 | 正常现象：烧录后新固件已自动从头启动，直接用 `watch` / `var` 观察即可；要重新从头跑用 `rst` |
| `在 ELF 里没有找到符号 xxx` | 拼写；是否局部变量（只支持全局/静态）；是否被 `-O2` 优化掉（`OPT=-O0` 重编）；ELF 与板上固件是否同一次构建 |
| `符号 xxx 在 ELF 里存在，但读不出值：被编译器优化掉了` | 同上，`make OPT=-O0` 重编固件 |
| `类型 … 里没有成员 xxx` / `下标 N 越界` | 成员名拼写 / 数组长度核对（`tscope var 数组名` 可看到长度） |
| Windows 下 `probe-rs list` 能列但连不上 | 驱动不是 WinUSB（见第三节方法 B Zadig） |

---

# 七、路线图

- v2：✅ ELF 符号解析（标量 / 数组 / 结构体 / 联合体 / 枚举，嵌套 + 成员路径）
- v3：✅ `watch` 实时监视（ratatui 交替屏表格，多组，Keil 风格高亮）；
  ✅ `debug` 交互式调试会话（断点/运行/单步/寄存器，watch 可嵌套）；
  ✅ `flash` 烧录（扇区擦除默认 + 校验，`--erase_all` 整片擦除带交互确认）；
  ⏳ 调试会话内查看局部变量；`chip.pack` 存在时自动调 target-gen
- v4：固件侧 RTT 通道，`watch` 数据源升级为 RTT（高带宽）
- v5：GUI（波形显示）

## 参考资料

- [probe-rs 官方文档](https://probe.rs/docs/)
- [probe-rs 探针配置（Linux udev / Windows 驱动）](https://probe.rs/docs/getting-started/probe-setup/)
- [probe-rs 源码仓库](https://github.com/probe-rs/probe-rs)
