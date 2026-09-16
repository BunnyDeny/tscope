# tscope

基于 [probe-rs](https://probe.rs) 库的嵌入式调试 / 监视工具。终极目标是 J-Scope 一类的
实时变量监视工具；当前 v1 只做一件事：**读取目标内存**（默认 `0x20000000`），
但架构按完整规划一次到位。

## 架构

库直接内嵌，无中间服务进程（与 VSCode 走 `dap-server` 的模式不同）：

```
tscope（本程序，内部链接 probe-rs 库） ←SWD→ J-Link → GD32F503RE
```

> 探针占用（实测）：`probe-rs dap-server` 仅监听、没有活动调试会话时，tscope 可以正常
> attach 并读写（Linux 的 hidraw 设备允许多进程打开）。但 VSCode **正在调试**（活动会话）
> 时按官方文档二者互斥、SWD 总线会互相干扰——运行 tscope 前请退出 VSCode 调试会话。

### 程序输入

| 输入 | 性质 | 作用 |
|---|---|---|
| 工具配置 YAML（`tscope.yaml`） | 必填，人写 | 探针参数、芯片型号、固件路径；作为每次执行的"环境变量" |
| 芯片描述 YAML（target-gen 产物） | 条件必填 | 仅当芯片不在 probe-rs 内置列表时；配置只引用其路径 |
| CMSIS-Pack | 条件输入 | 缺描述文件时用于拼 target-gen 指引命令（自动生成待 v2） |
| 调试探针 | 硬件 | 运行时枚举，多探针时按配置的 `probe.selector` 选定 |
| ELF 文件 | 保留字段 | v1 未使用；将来用于符号→地址解析与烧录 |

### 芯片解析：四级 fallback

芯片是否官方支持**不写进配置**，由运行时查询内置 Registry 判定：

```
chip.name → ① 内置 Registry 命中？──是→ 直接用
                │否
                ├─ ② chip.description 文件存在 → 加载 → 按名取变体
                │      └─ 变体不存在 → 报「描述文件里没有这个型号」
                ├─ ③ 描述文件缺失 → 打印 target-gen 指引（含 pack 时给现成命令）
                └─ ④ 都没有 → 报错，信息带方案 A / 方案 B
```

三种错误刻意分开：内置无此型号 / 描述文件不存在 / 描述文件里无此变体。

### 设计约定

- **两种 YAML 严格分开**：芯片描述 YAML 是 target-gen 生成的机器数据，工具配置
  只**引用**其路径，绝不合并
- **相对路径相对配置文件解析**，与运行目录无关
- `deny_unknown_fields`：配置拼错字段名直接报错，不静默忽略
- `version` 字段：格式演进时可友好迁移
- 危险操作默认关闭（v1 只读内存，天然安全；将来加烧录时整片擦除必须默认禁止）

## 目录结构

```
src/main.rs      CLI 入口：list / read 子命令
src/config.rs    YAML 配置结构、加载、校验、路径解析
src/session.rs   四级 fallback 的会话建立 + 探针选择
tscope.yaml      配置模板（GD32F503RE + J-Link 示例）
```

## 使用

```bash
cargo build --release

./target/release/tscope list                       # 列探针，排查连接问题
./target/release/tscope read                       # 读 0x20000000 一个 32 位字
./target/release/tscope read --address 0x20000000 --count 4
./target/release/tscope --config /path/to/tscope.yaml read
```

## 路线图

- v2：ELF 符号解析（变量名→地址），`watch` 周期采样 + 实时波形（J-Scope 雏形）
- v3：`chip.pack` 存在时自动调 target-gen 生成描述文件（幂等缓存）
- v4：固件侧 RTT 通道，`watch` 数据源从「读内存」升级为 RTT（高带宽正道）
- v5：GUI（egui）
