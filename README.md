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

不想在本地装 Rust，可以直接用 GitHub 在线打包的产物：

- 每次推送到 `main`：打开仓库 **Actions → 最新一次 `CI` 运行 → Artifacts**，
  下载 `winmem-x86_64-pc-windows-msvc`（内含 `winmem.exe`、`winmem-gui.exe` 和 README）。
- 正式版本：推送 `v*` 标签（如 `git tag v0.2.0 && git push origin v0.2.0`），
  工作流会在 **Releases** 页自动生成同名发布并附上下载包。
- 工作流定义：`.github/workflows/ci.yml`（先跑 fmt / clippy / test，再在 `windows-latest` 上 `cargo build --release`）。

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

### config / update / history

```
winmem config --init          # 生成配置模板
winmem config --template      # 只把模板打印到标准输出（不写文件）
winmem config                 # 查看当前生效值
winmem update --check         # 检查更新（只读，不下载）
winmem update                 # 下载 → 校验 SHA-256 → 替换自身（重启后生效）
winmem history                # 查看最近操作记录
```

`update` 的完整说明见第十六节。

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
| `sample_interval_secs` | `5` | 后台监测采样间隔 |
| `baseline_gap_ms` | `600` | 一次性运行时建立 CPU / I/O 差分基线的等待时长 |
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
minimize_to_tray = true            # 关闭窗口时收进托盘而不是退出
auto_update_check = true           # 启动时后台检查更新（只提示，不安装）
update_repo = npcxl/winmem         # 更新源仓库（owner/name）
press_enter_free_ratio = 0.1
press_hold_secs = 30
batch_size = 2
cooldown_secs = 1200
min_gain_mb = 64
baseline_gap_ms = 600
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
│   ├── update.rs         # 热更新（版本比较、WinHTTP 下载、SHA-256 校验、自替换回滚）
│   ├── main.rs           # CLI
│   └── bin/winmem-gui/   # 图形界面（Win32 + GDI，零依赖）
├── tests/integration.rs  # Windows 集成测试（只操作自建测试子进程）
├── .github/workflows/    # CI（fmt/clippy/test/构建校验）与 Release（自动发版 + latest.json）
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
- 自绘的开关/状态卡片暂未提供完整的无障碍（UIA）语义；
- 深色主题下原生控件的外观由系统决定，本工具只让自绘区域跟随主题。

---

## 十三、智能自动优化

**默认关闭。** 开启后程序在后台监测内存，仅在**持续物理内存压力**出现时，对**你明确允许的**闲置后台应用执行小批工作集裁剪。

```powershell
winmem auto --on                  # 开启（首次会展示说明）
winmem auto --allow chrome.exe,blender.exe
winmem auto --clear-allow         # 清空允许名单（只监测不提示）
winmem auto --startup on          # 开机自动启动（独立选项，默认关闭）
winmem auto --off                 # 关闭（立即取消剩余任务）
winmem auto --run                 # 前台运行自动会话
```

- **允许名单为空 ⇒ 只监测和提示**，不做任何操作；
- 开关与名单会持久化到配置文件；开机启动是**独立选项**，默认关闭；
- 触发条件：可用物理内存 < 10% **持续 30 秒**；进入/退出使用不同阈值（滞回）；
- 提交量接近上限只提醒，**不靠重复裁剪工作集处理**；
- 自动保护：核心系统进程、自身、其他会话、系统服务、前台应用及其相关进程、最近 10 分钟用过的应用、用户保护名单、正在明显计算或 I/O 的进程；**最小化或低 CPU 都不能单独作为可优化依据**；
- 每批 1~3 个；每次操作后重新检查压力与取消状态；同一进程至少冷却 20 分钟；压力恢复、候选耗尽或达到预算即停止；
- **绝不**自动清空待机列表、结束进程、挂起进程或修改系统配置；
- 效果反馈：观察工作集**回涨**、系统可用内存；回涨超过减少量的 50% 时判定该次裁剪无效，延长该进程冷却并停止后续操作；指标缺失时采取更保守策略；
- 关闭开关立即取消剩余任务。**已发生的工作集裁剪无法撤销**，本工具不提供「一键恢复」。

界面状态：`监测中 / 压力确认中 / 优化中 / 观察中 / 暂缓`，显示在状态栏右侧与第二行。

### 托盘常驻

`winmem-gui.exe` 启动后会在通知区（托盘）常驻：

- **右键托盘图标**：打开主窗口 / **暂停**智能自动优化 / **恢复**智能自动优化 / 退出 winmem；
- **左键单击或双击**图标：唤醒主窗口；
- 点击窗口关闭按钮默认**收进托盘**而不是退出（配置 `minimize_to_tray = false` 可改回直接退出）；
 托盘图标注册失败时不会隐藏窗口，避免程序变成无法操作的状态；
- 开机启动时带 `--minimized`：只驻留托盘，不弹窗口、不抢焦点；若配置中 `auto_enabled = true` 则自动开始监测；
- 窗口不可见（最小化 / 收进托盘）时刷新间隔从 400ms 降到 2s；
- 正常工作时不弹窗、不抢焦点、不频繁通知。

### 名单编辑

界面右键菜单 → **「编辑保护名单 / 自动优化允许名单…」**，可直接编辑两份名单（逗号分隔，不区分大小写，`.exe` 可省略）；
保存后正在运行的自动会话会通过配置指纹变化自动读取新配置。也可以直接改配置文件，或用命令行 `winmem auto --allow a.exe,b.exe`。

### 一次性运行的采样基线

CPU / I/O 活动指标由**两次采样差分**得到。一次性运行（`winmem optimize`、GUI 一键、预览）没有历史样本，
因此会先采一次基线，再等待 `baseline_gap_ms`（默认 600ms）后才判定活动状态。
若跳过这一步，所有进程都会是「活动指标未知」并被**保守跳过**——这是有意为之：指标未知时不冒充「闲置」。

---

## 十四、系统内存配置检查与变更

这是**独立的系统配置诊断**，与自动优化开关互不相干。

```powershell
winmem sysmem                              # 只读诊断（默认）
winmem sysmem compression --enable|--disable   # 内存压缩（需管理员，可能需重启）
winmem sysmem combining --enable|--disable     # 页面合并（高级设置，默认不启用）
winmem sysmem pagefile --enable|--disable      # 系统自动管理分页文件
winmem sysmem restore [compression|combining|pagefile]   # 恢复本工具上次修改前的配置
winmem sysmem history                          # 变更记录
```

诊断内容与边界：

| 项目 | 数据来源 | 说明 |
| --- | --- | --- |
| 内存压缩 | `Get-MMAgent`（PowerShell，结构化 JSON） | 部分页面压缩后留在物理内存，可减少磁盘换入，但需要 CPU 处理 |
| 页面合并 | `Get-MMAgent` | 合并内容相同的物理页面；收益取决于重复页面与负载，存在处理开销 |
| 分页文件 | `Win32_ComputerSystem` / `Win32_PageFileSetting` / `Win32_PageFileUsage` | 是否自动管理、路径、已配置与已分配大小、使用量与峰值 |
| 卷空间 | `Win32_LogicalDisk` | 分页文件所在卷的可用空间 |
| 提交容量 | `GetPerformanceInfo`（不走 PowerShell） | 当前提交量、上限、峰值 |

**状态语义严格区分**：`已开启` / `已关闭` / `不支持` / `不可用` / `查询失败`。
后四种**绝不**当作「已关闭」；查询失败会给出原因。

本工具**不声称**：

- 磁盘空间等于同容量内存；
- 开启压缩后立即释放固定数量的内存；
- 能指定 Windows 压缩某个进程；
- 配置调整一定改善性能或完全无影响。

变更流程：记录原状态 → 使用受支持的接口执行 → **重新查询验证** → 写变更记录（时间、项目、原值、目标值、结果、验证结果、是否需要重启）。
需要管理员权限时**只在实际变更时请求**，不自动提权、不安装常驻服务、不自动重启。
**「配置已改变」与「运行效果已确认」是两件事**：重启前会明确标注需要重新验证。

恢复：只有在**当前状态仍与工具上次修改结果一致**时才回滚；若已被其他软件修改，只显示差异、**不覆盖**。
关闭自动优化开关与恢复系统配置**互相独立**。

不实现自定义分页文件大小的自动调参，不迁移/删除/直接操作 `pagefile.sys`，
不会因为一次压力尖峰就修改系统配置；也不使用「RAM 的 1.5 倍/2 倍」这类未经验证的一刀切规则。

---

## 十五、界面与系统适配

- 继续使用 **Rust + 原生 Win32 控件**（按钮、输入框、列表、菜单、对话框均为系统控件），保留系统窗口边框、标题栏、缩放与吸附行为；
- 通过 manifest 激活 **Common Controls v6**，控件使用系统当前视觉样式；
- 声明并处理 **Per-Monitor V2 DPI**（`WM_DPICHANGED` 时重建字体、调整列宽并重新布局），支持 100% / 125% / 150% / 200% 与跨显示器拖动；
- **跟随系统主题**：浅色 / 深色两套配色用于自绘的状态卡片与压力条；检测到**高对比度**时改用系统颜色以保证可读性（不使用 undocumented 的暗色钩子）；
- 刷新列表时**保留选中项与滚动位置**；最小化后降低刷新频率（400ms → 2s）；
- 所有采样与优化都在后台线程执行，UI 线程只处理消息与展示结果；同一时刻只允许一个优化任务；
- 自绘范围仅限状态卡片、压力条与进度；不使用装饰性动画或持续重绘。

### 两个页面

主界面用**系统原生 Tab 控件**分成两页：

| 页面 | 内容 |
| --- | --- |
| **进程** | 进程列表（工作集 / 私有提交 / 线程 / 策略状态）、筛选、排序、多选后手动释放内存或结束进程 |
| **系统内存** | 物理内存、提交量与剩余、内存压缩、页面合并、分页文件与所在卷空间、诊断建议、**最近配置变更与恢复入口** |

「系统内存」页是**只读诊断**：查询在后台线程执行，不会阻塞界面，也不会每秒启动 PowerShell。
页面上的「打开 Windows 虚拟内存设置」「恢复上次配置…」是**需要用户手动确认**的系统配置变更（需管理员，可能需重启）。

界面上的三件事保持清晰区分：**只读诊断**（系统内存页）、**需要手动执行的系统配置变更**（sysmem 命令 / 需管理员）、**自动优化的运行状态**（开关与状态标签）。

---

## 十六、热更新

程序可以自己检查新版本、下载、校验并替换自身，用户不需要手动下载安装包。

```text
winmem update --check              # 只看有没有新版本，不下载
winmem update                      # 检查 → 询问确认 → 下载 → 校验 → 替换
winmem update --yes                # 跳过确认（脚本用）
winmem update --yes --restart      # 装完自动重启
winmem update --repo owner/fork    # 换一个更新源仓库
```

图形界面：**右键菜单 → 「检查更新…」**。启动时还会按配置做一次**静默检查**——发现新版本只在状态栏留一行提示，不弹窗、不抢焦点、不打断工作；是否安装完全由你决定。

### 工作原理

版本清单 `latest.json` 随每个 Release 一起发布，客户端通过

```text
https://github.com/<repo>/releases/latest/download/latest.json
```

获取。走的是 Release 资源直链而不是 GitHub API，因此**无需令牌、不受 API 限流影响**，并且天然只会拿到**正式版本**（预发布版不会成为 latest）。

清单里给出了每个文件的下载地址与 SHA-256：

```json
{
  "schema": 1,
  "tag": "v0.2.0",
  "version": "0.2.0",
  "notes": "本次更新说明",
  "assets": [
    { "name": "winmem.exe", "url": "...", "sha256": "...", "size": 488448 }
  ]
}
```

因此热更新**不需要解压 zip**：直接下载与自身同名的 exe 即可。

### 安全设计

| 环节 | 做法 |
| --- | --- |
| 传输 | 系统自带 **WinHTTP**（TLS、证书校验、超时、自动跟随重定向），不引入第三方网络库 |
| 完整性 | 下载后**必须**校验 SHA-256；清单缺失校验和或校验不通过时**直接放弃安装**，不写任何文件 |
| 原子性 | 新文件先写到目标同目录的 `*.exe.new`（同一卷内改名是原子操作） |
| 可回滚 | 旧文件改名为 `*.exe.old`；新文件就位失败会**自动把备份改回原名** |
| 可恢复 | `*.old` 在下次启动时自动清理；旧进程还没退出导致删不掉时留到下次再试 |
| 不静默 | 下载与安装**必须**经用户确认；自动检查只提示不安装（`auto_update_check = false` 可完全关闭） |
| 不提权 | 目录不可写时报错并提示「以管理员身份运行」，不会偷偷弹 UAC |

**为什么可以直接替换正在运行的程序？** Windows 允许**重命名**正在运行的 exe（只是不允许删除），所以流程是「旧文件改名 → 新文件就位」，不需要任何常驻的「更新器」辅助进程。

命令行与图形界面共享同一套实现：更新 `winmem.exe` 时会顺带更新同目录的 `winmem-gui.exe`，两者始终来自同一个 Release，不会出现版本错配。

### 相关配置

```ini
auto_update_check = true      # 启动时后台检查（只提示，不安装）
update_repo = npcxl/winmem    # 更新源仓库
last_update_check = 0         # 上次检查时间戳，程序自己维护
```

### 已知限制

- 更新源仓库必须是**公开**的（匿名读取 Release 资源）；私有仓库需要自行提供带认证的下载通道。
- 需要替换的是 `winmem.exe` / `winmem-gui.exe` 本身，因此安装目录必须可写（放在 `Program Files` 下需管理员权限）。
- 替换后需**重启程序**才生效——本工具不做「进程内自我升级」这种不稳定的事。

---

## 十七、发布流程（CI/CD）

### 你只需要改版本号然后 push

```text
1. 改 Cargo.toml 里的 version（例如 0.1.0 → 0.2.0）
2. git commit -am "chore: 发布 v0.2.0"
3. git push origin main
```

推送到 `main` 后 GitHub Actions 会自动：

1. 读取 `Cargo.toml` 的版本号，得到目标标签 `v0.2.0`；
2. 校验标签与版本号一致（不一致直接失败，避免发错版本）；
3. 若该标签不存在则创建并推送；
4. 用**该标签对应的提交**构建 release；
5. 组装发布包（两个 exe、README、示例配置、使用说明）；
6. 为 zip 与两个 exe 各生成 SHA-256；
7. 创建 GitHub Release（自动生成分类变更说明）；
8. 生成并上传 `latest.json`。

客户端下次检查更新时就能看到新版本。**标签已存在且发布过时会自动跳过**，所以重复推送是安全的，发布失败后重推也能自动补发。

### 三种触发方式

| 方式 | 场景 |
| --- | --- |
| 改版本号 push 到 `main` | 日常发布（推荐） |
| `git tag v0.2.0 && git push origin v0.2.0` | 指定提交发布、补发历史版本 |
| 手动触发 workflow 并填写 `tag` | 重发已有标签；产物先作为**草稿**，人工确认后再公开 |

### 工作流文件

```text
.github/workflows/ci.yml       每次 push / PR：fmt → clippy(-D warnings) → test → release 构建校验，
                               并跑一次 --bench 自检，把实测开销留在构建日志里
.github/workflows/release.yml  resolve（版本与标签判定）+ release（构建、打包、发布、生成 latest.json）
.github/release.yml            Release 说明的分类规则（按 PR 标签分组）
```

### 自定义更新源

`latest.json` 是唯一约定的接口。若你想自建分发（内网镜像、私有对象存储）：

```text
winmem update --repo your/name                      # 换仓库
set WINMEM_UPDATE_BASE=https://mirror.example       # 换基址（替代 https://github.com）
```

只要在对应地址提供同样 schema 的 `latest.json` 与可下载文件即可，客户端无需改动。

---

## 十六、热更新

程序可以自己检查新版本、下载、校验并替换自身，用户不需要手动下载安装包。

```text
winmem update --check              # 只看有没有新版本，不下载
winmem update                      # 检查 → 询问确认 → 下载 → 校验 → 替换
winmem update --yes                # 跳过确认（脚本用）
winmem update --yes --restart      # 装完自动重启
winmem update --repo owner/fork    # 换一个更新源仓库
```

图形界面：**右键菜单 → 「检查更新…」**。启动时还会按配置做一次**静默检查**——发现新版本只在状态栏留一行提示，不弹窗、不抢焦点、不打断工作；是否安装完全由你决定。

### 工作原理

版本清单 `latest.json` 随每个 Release 一起发布，客户端通过

```text
https://github.com/<repo>/releases/latest/download/latest.json
```

获取。走的是 Release 资源直链而不是 GitHub API，因此**无需令牌、不受 API 限流影响**，并且天然只会拿到**正式版本**（预发布版不会成为 latest）。

清单里给出了每个文件的下载地址与 SHA-256：

```json
{
  "schema": 1,
  "tag": "v0.2.0",
  "version": "0.2.0",
  "notes": "本次更新说明",
  "assets": [
    { "name": "winmem.exe", "url": "...", "sha256": "...", "size": 488448 }
  ]
}
```

因此热更新**不需要解压 zip**：直接下载与自身同名的 exe 即可。

### 安全设计

| 环节 | 做法 |
| --- | --- |
| 传输 | 系统自带 **WinHTTP**（TLS、证书校验、超时、自动跟随重定向），不引入第三方网络库 |
| 完整性 | 下载后**必须**校验 SHA-256；清单缺失校验和或校验不通过时**直接放弃安装**，不写任何文件 |
| 原子性 | 新文件先写到目标同目录的 `*.exe.new`（同一卷内改名是原子操作） |
| 可回滚 | 旧文件改名为 `*.exe.old`；新文件就位失败会**自动把备份改回原名** |
| 可恢复 | `*.old` 在下次启动时自动清理；旧进程还没退出导致删不掉时留到下次再试 |
| 不静默 | 下载与安装**必须**经用户确认；自动检查只提示不安装（`auto_update_check = false` 可完全关闭） |
| 不提权 | 目录不可写时报错并提示「以管理员身份运行」，不会偷偷弹 UAC |

**为什么可以直接替换正在运行的程序？** Windows 允许**重命名**正在运行的 exe（只是不允许删除），所以流程是「旧文件改名 → 新文件就位」，不需要任何常驻的「更新器」辅助进程。

命令行与图形界面共享同一套实现：更新 `winmem.exe` 时会顺带更新同目录的 `winmem-gui.exe`，两者始终来自同一个 Release，不会出现版本错配。

### 相关配置

```ini
auto_update_check = true      # 启动时后台检查（只提示，不安装）
update_repo = npcxl/winmem    # 更新源仓库
last_update_check = 0         # 上次检查时间戳，程序自己维护
```

### 已知限制

- 更新源仓库必须是**公开**的（匿名读取 Release 资源）；私有仓库需要自行提供带认证的下载通道。
- 需要替换的是 `winmem.exe` / `winmem-gui.exe` 本身，因此安装目录必须可写（放在 `Program Files` 下需管理员权限）。
- 替换后需**重启程序**才生效——本工具不做「进程内自我升级」这种不稳定的事。

---

## 十七、发布流程（CI/CD）

### 你只需要改版本号然后 push

```text
1. 改 Cargo.toml 里的 version（例如 0.1.0 → 0.2.0）
2. git commit -am "chore: 发布 v0.2.0"
3. git push origin main
```

推送到 `main` 后 GitHub Actions 会自动：

1. 读取 `Cargo.toml` 的版本号，得到目标标签 `v0.2.0`；
2. 校验标签与版本号一致（不一致直接失败，避免发错版本）；
3. 若该标签不存在则创建并推送；
4. 用**该标签对应的提交**构建 release；
5. 组装发布包（两个 exe、README、示例配置、使用说明）；
6. 为 zip 与两个 exe 各生成 SHA-256；
7. 创建 GitHub Release（自动生成分类变更说明）；
8. 生成并上传 `latest.json`。

客户端下次检查更新时就能看到新版本。**标签已存在且发布过时会自动跳过**，所以重复推送是安全的，发布失败后重推也能自动补发。

### 三种触发方式

| 方式 | 场景 |
| --- | --- |
| 改版本号 push 到 `main` | 日常发布（推荐） |
| `git tag v0.2.0 && git push origin v0.2.0` | 指定提交发布、补发历史版本 |
| 手动触发 workflow 并填写 `tag` | 重发已有标签；产物先作为**草稿**，人工确认后再公开 |

### 工作流文件

```text
.github/workflows/ci.yml       每次 push / PR：fmt → clippy(-D warnings) → test → release 构建校验，
                               并跑一次 --bench 自检，把实测开销留在构建日志里
.github/workflows/release.yml  resolve（版本与标签判定）+ release（构建、打包、发布、生成 latest.json）
.github/release.yml            Release 说明的分类规则（按 PR 标签分组）
```

### 自定义更新源

`latest.json` 是唯一约定的接口。若你想自建分发（内网镜像、私有对象存储）：

```text
winmem update --repo your/name                      # 换仓库
set WINMEM_UPDATE_BASE=https://mirror.example       # 换基址（替代 https://github.com）
```

只要在对应地址提供同样 schema 的 `latest.json` 与可下载文件即可，客户端无需改动。
