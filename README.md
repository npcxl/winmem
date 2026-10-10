# winmem

Windows 内存管理工具（Rust，零第三方依赖）。包含图形界面 `winmem-gui.exe` 与命令行 `winmem.exe`。

> **v0.2 行为变更（重要）**
> 早期版本的「一键优化」会**清空系统待机列表并裁剪几乎所有进程的工作集**。现已改为
> **按真实内存压力执行、保护日常使用体验**的策略化流程：
> - 默认不再清空待机列表（改为显式的高级操作 `winmem standby`）；
> - 只在**可用物理内存低于 10% 且持续 30 秒**时才处理；
> - 自动模式**只处理配置 `allow` 名单中的进程**，其余一律跳过并给出原因；
> - 不结束、不挂起任何进程；`kill` 永远只是手动操作。

---

## 一、先弄清三个概念

| 指标 | 含义 | 容易误解的地方 |
| --- | --- | --- |
| **工作集**（Working Set） | 进程当前**驻留在物理内存**中的页面 | 数值大不等于「浪费」，很多是共享的代码页/缓存页 |
| **私有提交量**（Private Commit / PrivateUsage） | 进程已提交的**私有虚拟内存**，可能大部分不在物理内存中 | 它是「需要多少地址空间和分页文件」，**不等于**物理内存占用 |
| **待机缓存**（Standby List / Cached） | 系统缓存的、随时可回收的页面 | 清理它会**直接释放为空闲内存，但之后必须重新读磁盘**，可能造成卡顿 |

**必须澄清的几点（本工具不做虚假承诺）：**

- 工作集裁剪只是把不活跃页面换出到待机列表，**不会减少应用的私有提交需求**，也**不能修复内存泄漏**；
- 被换出的页面在程序再次访问时会重新读回，**可能造成轻微延迟**，不能宣称「完全无感」；
- 「工作集减少 X GB」**不等于**「系统可用物理内存增加了 X GB」，两者必须分开看；
- 页面文件、内存压缩、SysMain 等系统机制不应被本工具关闭或修改。

---

## 二、构建

```powershell
cd winmem
cargo build --release
# 产物：target\release\winmem.exe 与 target\release\winmem-gui.exe
```

要求：Windows 10/11 或 Windows 7+（64 位），Rust 1.82+。
涉及系统进程时需要**以管理员身份运行**；普通权限下会自动降级（受保护进程被跳过并说明原因）。

---

## 三、快速开始

```powershell
winmem status                 # 看内存状态与压力判断依据
winmem list --top 20          # 看谁在占内存
winmem optimize --dry-run     # 先预览：会处理谁、跳过谁、为什么
winmem monitor                # 监测模式（默认只观察，不执行）
```

图形界面：双击 `winmem-gui.exe`。工具栏为「刷新 / 智能优化 / 预览候选 / 释放选中内存 / 结束选中进程 / 全选 / 取消选择」，
列表右键菜单可打开配置文件、执行高级的待机列表清理。

---

## 四、命令

### status —— 系统状态与压力原因

显示物理内存、可用比例、提交量与提交上限，并用**与优化流程完全相同的状态机**给出压力判定，
同时显示低内存通知接口是否可用（`CreateMemoryResourceNotification`）。

### list —— 进程指标

```
winmem list [--top N] [--all] [--filter 名称]
```

列为 PID / 进程名 / 工作集 / 私有提交 / 会话 / CPU / 状态。
状态含义：`可优化`（当前策略下会被自动处理）、`仅手动`（自动跳过但可手动操作）、`禁止操作`（硬保护）。

### optimize —— 按策略执行一次压力缓解

```
winmem optimize [--dry-run] [--budget N]
```

- `--dry-run`：只输出候选、受保护对象与**跳过原因**，不执行任何操作；
- 未达到压力阈值时**什么都不做**，并明确告知原因。

### trim —— 手动裁剪工作集

```
winmem trim --pid 1234,5678
winmem trim --name chrome.exe
```

仍遵守硬保护；软保护原因会以警告形式提示。
注意 `EmptyWorkingSet` **可能一次移除该进程大量工作集页面**，这与「只释放指定大小」不同。

### monitor —— 监测模式

```
winmem monitor [--interval 5] [--auto]
```

默认**只观察不执行**。`--auto` 需要配置中 `auto_enabled = true` 才会生效，且仍只处理 `allow` 名单内的进程。

### standby —— 高级诊断（手动）

```
winmem standby
```

显式清空系统待机列表。需要 `SeProfileSingleProcessPrivilege`（按需启用，**用完即恢复原特权状态**），
失败时**保留原始 NTSTATUS** 而不是笼统地说「需要管理员权限」。**默认优化流程永远不会调用它。**

### kill —— 手动结束进程

```
winmem kill --pid 1234 [,5678]
winmem kill --name chrome.exe [--force] [--yes]
```

- 硬保护（核心系统进程、系统标记的关键进程、本程序）**任何情况下都无法绕过，`--force` 也不行**；
- 软保护（explorer 等外壳组件、用户保护名单、前台应用、最近使用过的应用）默认阻止，加 `--force` 才放行并给出警告；
- 与「工作集大小」「CPU 活跃度」无关的原因（如工作集过小）**不会**阻挡手动结束进程；
- 默认需要交互确认，`--yes` 用于脚本。

### config / history

```
winmem config --init          # 生成配置模板
winmem config                 # 查看当前生效值
winmem history                # 查看最近操作记录
```

---

## 五、优化流程与默认参数

```
采样 → 判断持续压力 → 构建保护集合 → 选择候选
     → 小批执行 → 观察 → 压力恢复或收益不足时停止 → 冷却
```

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| 系统采样间隔 | 2~5 秒 | monitor 模式 |
| `press_enter_free_ratio` | `0.10` | 可用物理内存低于 10% 才进入压力候选 |
| `press_hold_secs` | `30` | 需持续 30 秒才真正执行 |
| `press_exit_free_ratio` | `0.15` | 回升到 15% 才退出（滞回，防止抖动） |
| `commit_warn_ratio` / `commit_alert_ratio` | `0.85` / `0.90` | 提交量告警阈值 |
| `batch_size` | `2` | 每批处理 1~3 个候选 |
| `run_budget` | `12` | 单次运行最多处理多少个进程 |
| `cooldown_secs` | `1200` | 单个进程 20 分钟内不重复处理 |
| `observe_secs` | `30` | 每批执行后的观察时间 |
| `min_gain_bytes` | `64 MB` | 单批收益低于此值则提前停止 |
| `recent_foreground_secs` | `600` | 最近 10 分钟用过前台的应用受保护 |
| `min_candidate_ws` | `128 MB` | 工作集过小的进程不值得处理 |
| `busy_cpu_percent` | `5.0` | 活跃计算/I/O 的进程受保护 |
| `auto_allow_unlisted` | `false` | 是否允许自动处理未列入 `allow` 的进程 |

**这些是经验初始值，不是官方标准，必须按实际负载实测调整。**

两种压力被明确区分：

- **物理内存不足** → 允许对合格的后台候选做工作集裁剪；
- **提交量接近上限** → 只给出提示（检查异常增长的程序、应用退出/重启、分页文件设置），
  **禁止**通过反复裁剪工作集假装解决。

---

## 六、保护规则

**第一级 · 硬保护（不可绕过）**
`System`、`registry`、`idle`、`secure system`、`memory compression`、`smss`、`csrss`、`wininit`、
`winlogon`、`services`、`lsass`、`lsaiso`、`dwm`、PID ≤ 4、本程序自身，
以及系统通过 `IsProcessCritical` 标记为关键的进程（Win8.1+ 动态探测，不支持时降级）。

**第二级 · 软保护（默认跳过，手动指定时降级为警告）**
其他用户会话、身份未知、用户保护名单、系统外壳与安全组件（explorer / sihost / audiodg 等）、
当前前台应用、最近 10 分钟进入过前台的应用、活跃计算或 I/O 的进程、冷却期内的进程、工作集过小的进程、
不在 `allow` 名单内（仅自动模式）。

名称比较统一先转小写再去 `.exe` 后缀。**不确定身份或场景时默认跳过并说明原因。**

---

## 七、进程身份与安全

- 进程身份 = **PID + 创建时间**，避免 PID 复用导致误操作；
- 执行前用**同一个已核验句柄**完成校验与操作，不在校验后按 PID 重新打开；
- 进程退出或身份变化时跳过，**不计入收益**；
- 句柄与动态加载模块使用 RAII 管理，`GetLastError` 在首个可能覆盖它的调用之前立即读取；
- 读取失败的指标显示为**未知**，绝不当作 0；系统采样失败同理；
- 失败原因区分：权限不足 / 进程已退出 / 接口不支持 / 读取失败 / 其他系统错误。

---

## 八、配置

配置位于 `%APPDATA%\winmem\config.ini`（扁平 `key = value` 文本）：

```ini
protect = explorer.exe, dwm.exe    # 用户保护名单
allow = mygame.exe, blender.exe    # 自动优化允许名单
auto_enabled = false               # 是否允许 monitor --auto 执行
auto_allow_unlisted = false        # 是否允许自动处理未列入 allow 的进程
press_enter_free_ratio = 0.1
press_hold_secs = 30
batch_size = 2
cooldown_secs = 1200
min_gain_mb = 64
```

未知键、非法值都会给出警告并保留默认（不会导致启动失败）。
历史记录写在 `%APPDATA%\winmem\history.log`，采用有上限的环形缓存，仅在操作结束时整文件重写，不做高频刷盘。
**不上传任何进程信息。**

---

## 九、如何验证效果

### 场景 1：正常负载（不应有任何动作）

```powershell
winmem monitor --interval 5
```

预期：持续输出「压力=None」或「疑似压力」，**不会执行任何裁剪**。这是最重要的一条——
工具在内存充足时应当**安静**。

### 场景 2：真实低内存

在虚拟机中把内存限制到 2~4 GB，同时打开若干大型应用：

```powershell
winmem status
winmem optimize --dry-run     # 确认候选与跳过原因合理
winmem optimize               # 实际执行
```

预期：压力需持续 30 秒才执行；每批 2 个；执行后观察 30 秒；可用内存回升后提前停止；
同一进程 20 分钟内不会被重复处理。

### 场景 3：提交量接近上限

用程序大量提交内存（保留但不写入）使 `commit_ratio` 超过 85%：

预期：工具**只提示**提交压力，**不执行**反复的工作集裁剪。可用内存的净变化允许为负。

---

## 十、项目结构

```
winmem/
├── src/
│   ├── lib.rs            # 库入口
│   ├── win.rs            # Windows API 封装（RAII 句柄、进程身份、错误分类）
│   ├── metrics.rs        # 系统/进程指标采集（含差分类 CPU 与 I/O）
│   ├── policy.rs         # 保护规则、压力状态机、冷却（纯逻辑，可单测）
│   ├── engine.rs         # 统一执行引擎（采样→判定→小批执行→观察→冷却）
│   ├── config.rs         # 用户配置、保护/允许名单
│   ├── report.rs         # 报告格式化与历史
│   ├── term.rs           # 终端渲染
│   ├── main.rs           # CLI
│   └── bin/winmem-gui/   # 图形界面（Win32 + GDI，零依赖）
├── tests/integration.rs  # Windows 集成测试（只操作自建测试子进程）
├── winmem.manifest       # 启用 Common Controls 6.0（跟随系统视觉样式）
└── build.rs
```

CLI 与 GUI **共用同一套** `metrics` / `policy` / `engine` 逻辑，不存在两份「一键优化」。

运行检查：

```powershell
cargo fmt --check
cargo clippy --all-targets
cargo test          # 单元测试 + Windows 集成测试
```

---

## 十一、明确不做的事

- 不关闭分页文件、内存压缩、SysMain；
- 不清空 Modified List 或全局系统工作集；
- 不自动停用服务、不改注册表、不改启动项、不安装内核驱动；
- 不自动结束或挂起应用；
- 不把「内存增长」直接认定为泄漏；
- 不宣称所有页面只进入待机列表、也不宣称始终无感恢复。

---

## 十二、已知限制

- 分页读取、磁盘延迟等指标尚未接入 PDH，暂以低内存通知 + 提交量 + I/O 差分近似判断；
- 应用分组（多进程浏览器等）目前只做到「父进程关系 + 前台窗口」，未做完整的应用级归并；
- 阈值均为经验值，需要按你的负载实测调整；
- 高分屏下界面按系统 DPI 缩放，但未处理多显示器 DPI 切换。
