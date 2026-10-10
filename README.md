# winmem

面向 Windows 的内存优化与「进程内存栈」管理工具，用 Rust 编写，**零第三方依赖**（直接绑定 `kernel32` / `psapi` / `advapi32` / `ntdll`）。

适用场景：内存不够用（内存占用 80%~95%）、想找出谁在吃内存、一键回收内存、按需选择性关闭进程。

---

## 功能

| 命令 | 说明 |
| --- | --- |
| `winmem` / `winmem status` | 内存总览：物理内存、提交内存、系统缓存、内核占用、内存压力条 |
| `winmem list` | 列出所有进程的内存栈（工作集 / 私有内存 / 线程数），可按工作集、私有内存、名称、PID 排序 |
| `winmem trim` | **释放**指定进程的工作集（不结束进程），立即回收内存 |
| `winmem optimize` | **一键优化**：清空系统待机列表（Standby List）+ 释放全部可操作进程的工作集 |
| `winmem kill` | **选择性关闭**进程，带关键进程保护与二次确认 |
| `winmem interactive` | 交互模式：列出内存栈 → 输入编号选择 → 选择「关闭」或「释放内存」 |

---

## 构建

```powershell
cd winmem
cargo build --release
# 产物：target\release\winmem.exe
```

要求：Windows 10/11，Rust 1.82+（本机使用 1.97.1 编译通过）。

建议以**管理员身份**运行（可操作系统进程、可清空待机列表），非管理员也能使用，只是能处理的范围更小。

---

## 用法

```powershell
# 1) 看内存总览
winmem status

# 2) 找出吃内存最多的进程（默认按工作集降序显示前 30 个）
winmem list
winmem list --top 20 --sort private      # 按私有内存排序
winmem list --all                        # 全部进程
winmem list --filter chrome              # 只看 chrome

# 3) 一键内存优化（不关进程，只是回收内存）
winmem optimize

# 4) 只释放某些进程的内存（不结束进程）
winmem trim --pid 1234,5678
winmem trim --name chrome.exe
winmem trim --top 10

# 5) 选择性关闭进程
winmem kill --pid 12345                  # 会先列出目标并要求确认
winmem kill --name "chrome.exe"          # 按名称匹配（可能命中多个）
winmem kill --pid 12345 --yes            # 跳过确认（脚本用）

# 6) 交互式（推荐日常使用）
winmem interactive
```

### 交互模式示例

```
 编号  PID     进程名                工作集      私有内存     线程  状态
  1    1508    idea64.exe             1.79 GB     2.24 GB     136  可操作
  2    21492   CodeBuddy CN.exe       880.0 MB    814.2 MB     31  可操作
  3    7512    Weixin.exe             441.8 MB    298.7 MB    122  可操作
  ...

请选择进程编号（如 1,3,5-8）／ t 一键优化 ／ q 退出：1,3
操作：[k] 结束进程   [t] 释放内存(不结束)   [c] 取消 ：
```

编号支持 `1,3,5-8` 这种混合写法；输入 `t` 直接做一键优化。

---

## 命令选项

| 选项 | 适用命令 | 说明 |
| --- | --- | --- |
| `--pid <PID[,PID...]>` | `trim` `kill` | 指定进程 PID，支持逗号分隔 |
| `--name <名称>` | `trim` `kill` | 指定进程名（忽略 `.exe` 后缀、忽略大小写） |
| `--top <N>` | `list` `trim` | 只处理内存占用最高的 N 个 |
| `--sort mem\|private\|name\|pid` | `list` | 排序方式，默认 `mem`（工作集） |
| `--filter <关键字>` | `list` | 按进程名过滤 |
| `--all` | `list` | 显示全部进程 |
| `--force` | `kill` | 允许结束非致命但重要的进程（系统关键进程仍会被拦截） |
| `--yes` | `kill` | 跳过二次确认 |

---

## 内存优化的原理（为什么安全）

本工具**不会**通过结束进程来「优化内存」，它做两件事：

1. **释放工作集**（`EmptyWorkingSet` / `SetProcessWorkingSetSize`）
   把进程当前不活跃的内存页从物理内存换出到**待机列表**。这些页面并没有丢失，进程再次访问时由系统按需换回，因此不会影响程序运行，只是可能有极短的再次访问延迟。
2. **清空系统待机列表**（`NtSetSystemInformation` + `SystemMemoryListInformation`）
   把待机列表中的缓存页释放为真正的空闲内存（需要 `SeProfileSingleProcessPrivilege`，即管理员权限）。

二者合起来就是常见的「内存优化 / 内存清理」效果：立即可用内存上升，同时不丢失工作状态。

> 实测（32 GB 内存、日常办公负载下）：可用物理内存 4.77 GB → 10.45 GB，累计释放工作集约 26.9 GB，处理 283 个进程，耗时数秒。

## 安全设计

- **关键进程保护**：`System`、`csrss.exe`、`winlogon.exe`、`services.exe`、`lsass.exe`、`wininit.exe`、`smss.exe`、`dwm.exe`、`audiodg.exe` 等以及 PID ≤ 4 的系统进程会被自动跳过，`kill` 时也会明确提示已跳过哪些。
- **自身保护**：不会结束当前运行的 `winmem` 进程。
- **默认二次确认**：`kill` 与交互模式的「结束进程」都需要手动确认（`--yes` 可跳过，用于脚本）。
- **优化不关进程**：`optimize` / `trim` 只回收内存，绝不结束进程。
- **权限不足只降级不失败**：以普通用户运行时，受限进程会被计入「失败」或跳过，不影响其余操作。

---

## 项目结构

```
winmem/
├── Cargo.toml          # 零依赖，release 开启 LTO / strip
└── src/
    ├── main.rs         # CLI 解析、各命令实现、交互模式、单元测试
    ├── win.rs          # Windows API 纯 FFI 绑定（进程枚举/内存计数/特权/待机列表）
    ├── ops.rs          # 业务层：内存栈枚举、排序过滤、内存释放、一键优化
    └── ui.rs           # 终端 UI：ANSI 颜色、容量格式化、表格、压力条
```

运行测试：

```powershell
cargo test
```

---

## 常见问题

**Q：为什么有些进程显示「无权读取」或优化失败？**
A：这些通常是系统进程或其他用户的进程，需要以管理员身份运行才能访问。即使提权，部分受保护进程（PPL）依然不可操作。

**Q：优化后内存又涨回去了？**
A：正常现象。被释放的是缓存性质的内存，程序继续运行会重新占用。日常建议只在内存吃紧时执行，或定期执行。

**Q：`kill` 会不会误杀系统？**
A：不会。系统关键进程被硬编码保护并自动跳过；PID ≤ 4 的进程绝对不允许结束（加了 `--force` 也不行）。

**Q：输出乱码 / 没有颜色？**
A：工具会自动尝试开启 ANSI 支持。若在老旧控制台中出现乱码，可换用 Windows Terminal。
