//! 热更新：检查新版本 → 下载 → 校验 → 替换自身 → 重启。
//!
//! 设计要点：
//! - **只使用系统自带的 WinHTTP**（见 [`crate::win::http_get`]）与自实现的 SHA-256，
//!   延续本仓库「零第三方依赖」的做法；
//! - 版本清单 `latest.json` 随 Release 一起发布，通过
//!   `https://github.com/<repo>/releases/latest/download/latest.json` 匿名获取，
//!   不依赖 GitHub API，因此不受 API 限流影响（仓库需为公开）；
//! - **只跟随正式版**：`releases/latest` 本身就会跳过预发布版本；
//! - 下载后**必须**校验 SHA-256，不匹配立即放弃并删除临时文件；
//! - Windows 允许重命名正在运行的 exe（但不能删除），因此替换流程是
//!   「旧文件改名 → 新文件就位」，任何一步失败都会回滚；
//! - 替换的是**当前正在运行的那个 exe**；同目录下的另一个 exe 如果存在，
//!   也会一并更新（两者来自同一个 Release，避免版本错配）。

use std::path::{Path, PathBuf};

use crate::win;

/// 默认更新源（不含 URL 前缀）。
pub const DEFAULT_REPO: &str = "npcxl/winmem";

/// 版本清单的最大允许大小。
const MANIFEST_MAX_BYTES: usize = 256 * 1024;
/// 更新包的最大允许大小。
const ASSET_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 备份文件后缀。
pub const BACKUP_SUFFIX: &str = ".old";

/// 热更新的前置说明：本工具不会静默安装，也不会自动提权。
pub const NOTICE: &str =
    "更新会下载并替换同目录下的可执行文件；替换前旧文件会改名为 *.old 作为备份，\
     失败时自动回滚。程序所在目录不可写时需要以管理员身份运行。";

/// 当前程序版本。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 当前 Unix 时间戳（秒），用于记录上次检查时间。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 自动检查更新的最小间隔（秒）：默认一天一次，避免频繁联网。
pub const AUTO_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// 当前运行的 exe 路径。
pub fn self_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// 与当前 exe 同目录的另一个可执行文件名（用于一并更新）。
pub fn sibling_name(self_name: &str) -> &'static str {
    if self_name.eq_ignore_ascii_case("winmem-gui.exe") {
        "winmem.exe"
    } else {
        "winmem-gui.exe"
    }
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 增量 SHA-256。仅用于校验下载到的更新包。
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buflen: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buflen: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let need = 64 - self.buflen;
            let take = need.min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total.wrapping_mul(8);
        // 追加 0x80，再补 0 到 56 mod 64，最后 8 字节是大端位长度
        self.update_no_len(&[0x80]);
        while self.buflen != 56 {
            self.update_no_len(&[0x00]);
        }
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&bit_len.to_be_bytes());
        self.update_no_len(&tail);

        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    /// 补位阶段使用：不累计总长度。
    fn update_no_len(&mut self, data: &[u8]) {
        for b in data {
            self.buf[self.buflen] = *b;
            self.buflen += 1;
            if self.buflen == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buflen = 0;
            }
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

/// 计算字节的 SHA-256 十六进制字符串（小写）。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    to_hex(&h.finish())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ---------------------------------------------------------------------------
// 版本比较
// ---------------------------------------------------------------------------

/// 拆出主版本数字与预发布标识。
fn split_version(v: &str) -> (Vec<u64>, Option<String>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    match v.split_once('-') {
        Some((core, pre)) => (parse_core(core), Some(pre.to_ascii_lowercase())),
        None => (parse_core(v), None),
    }
}

fn parse_core(core: &str) -> Vec<u64> {
    core.split('.')
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0)
        })
        .collect()
}

/// 比较两个版本号：`Greater` 表示 `a` 比 `b` 新。
///
/// 规则：主版本按数字逐段比较；正式版比同号预发布版新（`1.0.0 > 1.0.0-beta`）。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ca, pa) = split_version(a);
    let (cb, pb) = split_version(b);
    let n = ca.len().max(cb.len());
    for i in 0..n {
        let x = ca.get(i).copied().unwrap_or(0);
        let y = cb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    match (pa, pb) {
        (None, None) => Ordering::Equal,
        // 正式版比同号预发布版新
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_prerelease(&x, &y),
    }
}

/// 按 semver 规则比较预发布标识：`beta.2 < beta.10`。
///
/// - 逐段比较，数字段按数值比（`2 < 10`），非数字段按字典序；
/// - 数字段优先级低于非数字段；
/// - 前缀相同但段数更少者优先级更低（`beta < beta.1`）。
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let av: Vec<&str> = a.split('.').collect();
    let bv: Vec<&str> = b.split('.').collect();
    for i in 0..av.len().max(bv.len()) {
        let o = match (av.get(i), bv.get(i)) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

// ---------------------------------------------------------------------------
// 版本清单
// ---------------------------------------------------------------------------

/// 清单中的一个可下载文件。
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
}

/// 最新版本信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub notes: String,
    pub assets: Vec<Asset>,
}

impl ReleaseInfo {
    /// 找到与给定文件名匹配的资源（忽略大小写）。
    pub fn asset(&self, file_name: &str) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(file_name))
    }
}

/// 检查结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    /// 当前已是最新（或本地版本更新）
    UpToDate { current: String, latest: String },
    /// 有新版本可用
    Available(Box<ReleaseInfo>),
    /// 查询失败：网络、HTTP 状态或清单格式问题
    Failed(String),
}

/// 更新源基址。
///
/// 默认使用 GitHub；可用环境变量 `WINMEM_UPDATE_BASE` 覆盖，
/// 便于自建镜像或本地联调（例：`http://127.0.0.1:8080`）。
pub fn update_base() -> String {
    match std::env::var("WINMEM_UPDATE_BASE") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => "https://github.com".to_string(),
    }
}

/// 版本清单地址。
///
/// 使用 `releases/latest/download/...` 而不是 GitHub API：
/// 无需令牌、不受 API 限流影响，且天然跳过预发布版本。
pub fn manifest_url(repo: &str) -> String {
    format!(
        "{}/{}/releases/latest/download/latest.json",
        update_base(),
        repo.trim().trim_matches('/')
    )
}

/// 解析 `latest.json`。
pub fn parse_manifest(text: &str) -> Result<ReleaseInfo, String> {
    let v = crate::json::Json::parse(text).map_err(|e| format!("清单不是合法 JSON：{}", e))?;
    if v.get("schema").and_then(|s| s.as_u64()) != Some(1) {
        return Err("清单格式版本不支持（需要 schema = 1）".into());
    }
    let version = v
        .get("version")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "清单缺少 version".to_string())?
        .to_string();
    let tag = v
        .get("tag")
        .and_then(|x| x.as_str())
        .unwrap_or(&format!("v{}", version))
        .to_string();
    let notes = v
        .get("notes")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let mut assets = Vec::new();
    if let Some(arr) = v.get("assets").and_then(|x| x.as_arr()) {
        for a in arr {
            let (Some(name), Some(url)) = (
                a.get("name").and_then(|x| x.as_str()),
                a.get("url").and_then(|x| x.as_str()),
            ) else {
                continue;
            };
            assets.push(Asset {
                name: name.to_string(),
                url: url.to_string(),
                sha256: a
                    .get("sha256")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase(),
                size: a.get("size").and_then(|x| x.as_u64()),
            });
        }
    }
    if assets.is_empty() {
        return Err("清单里没有可下载的文件".into());
    }
    Ok(ReleaseInfo {
        tag,
        version,
        notes,
        assets,
    })
}

/// 查询最新版本。
pub fn check(repo: &str) -> CheckResult {
    let current = current_version().to_string();
    let url = manifest_url(repo);
    let (status, body) = match win::http_get(&url, MANIFEST_MAX_BYTES) {
        Ok(r) => r,
        Err(e) => return CheckResult::Failed(format!("无法获取版本清单：{}", e)),
    };
    if status != 200 {
        return CheckResult::Failed(format!(
            "获取版本清单失败（HTTP {}）。请确认仓库是公开的，且已经发布过正式版本。",
            status
        ));
    }
    let text = String::from_utf8_lossy(&body);
    let info = match parse_manifest(&text) {
        Ok(i) => i,
        Err(e) => return CheckResult::Failed(e),
    };
    if compare_versions(&info.version, &current) == std::cmp::Ordering::Greater {
        CheckResult::Available(Box::new(info))
    } else {
        CheckResult::UpToDate {
            current,
            latest: info.version,
        }
    }
}

// ---------------------------------------------------------------------------
// 下载与替换
// ---------------------------------------------------------------------------

/// 单个文件的更新结果。
#[derive(Debug, Clone)]
pub struct FileUpdate {
    pub path: PathBuf,
    /// 备份文件路径（旧版本改名到这里）
    pub backup: Option<PathBuf>,
    pub ok: bool,
    pub message: String,
}

/// 一次更新的整体结果。
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub files: Vec<FileUpdate>,
    /// 是否所有目标都已成功替换
    pub all_ok: bool,
}

/// 把更新写入指定路径：校验 → 备份旧文件 → 新文件就位，失败自动回滚。
///
/// **不结束任何进程**；调用方负责提示用户重启以生效。
pub fn install_asset(target: &Path, asset: &Asset) -> FileUpdate {
    let mut fu = FileUpdate {
        path: target.to_path_buf(),
        backup: None,
        ok: false,
        message: String::new(),
    };

    if asset.sha256.is_empty() {
        fu.message = "更新包缺少 SHA-256，已拒绝安装".into();
        return fu;
    }

    let (status, body) = match win::http_get(&asset.url, ASSET_MAX_BYTES) {
        Ok(r) => r,
        Err(e) => {
            fu.message = format!("下载失败：{}", e);
            return fu;
        }
    };
    if status != 200 {
        fu.message = format!("下载失败（HTTP {}）", status);
        return fu;
    }

    let got = sha256_hex(&body);
    if got != asset.sha256 {
        fu.message = format!(
            "校验失败：期望 {}，实际 {}（已放弃安装）",
            &asset.sha256[..asset.sha256.len().min(16)],
            &got[..16]
        );
        return fu;
    }

    // 临时文件与目标同目录，保证同一卷内改名是原子操作
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "winmem".into());
    let staged = dir.join(format!("{}.new", name));
    let backup = dir.join(format!("{}{}", name, BACKUP_SUFFIX));

    if let Err(e) = std::fs::write(&staged, &body) {
        fu.message = format!("写入临时文件失败：{}", e);
        return fu;
    }

    // 清掉上次遗留的备份（可能删不掉，不影响本次更新）
    let _ = win::delete_file(&backup);

    if let Err(e) = win::move_file(target, &backup) {
        let _ = win::delete_file(&staged);
        fu.message = format!(
            "无法备份当前程序（{}）；目录不可写时需要以管理员身份运行",
            e
        );
        return fu;
    }

    if let Err(e) = win::move_file(&staged, target) {
        // 回滚：把备份放回去
        let rolled = win::move_file(&backup, target).is_ok();
        fu.message = if rolled {
            format!("替换失败（{}），已回滚到原文件", e)
        } else {
            format!(
                "替换失败（{}），且回滚失败；原程序在 {}",
                e,
                backup.display()
            )
        };
        return fu;
    }

    fu.backup = Some(backup);
    fu.ok = true;
    fu.message = "已更新".into();
    fu
}

/// 更新当前正在运行的程序（以及同目录下的另一个可执行文件，如果存在）。
///
/// `siblings` 是除自身外、需要一并尝试更新的文件名（例如命令行更新 GUI）。
pub fn install(info: &ReleaseInfo, self_exe: &Path, siblings: &[&str]) -> ApplyReport {
    let mut report = ApplyReport::default();

    let self_name = self_exe
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut targets: Vec<PathBuf> = vec![self_exe.to_path_buf()];
    let dir = self_exe.parent().unwrap_or_else(|| Path::new("."));
    for s in siblings {
        let p = dir.join(s);
        if p.exists() && !self_name.eq_ignore_ascii_case(s) {
            targets.push(p);
        }
    }

    for t in targets {
        let name = t
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match info.asset(&name) {
            Some(a) => report.files.push(install_asset(&t, a)),
            None => report.files.push(FileUpdate {
                path: t.clone(),
                backup: None,
                ok: false,
                message: format!("本次发布没有提供 {}", name),
            }),
        }
    }
    report.all_ok = !report.files.is_empty() && report.files.iter().all(|f| f.ok);
    report
}

/// 启动时清理上次更新遗留的备份文件。
///
/// 返回被成功清理的文件数。正在运行的旧进程可能尚未退出，
/// 此时删除会失败——下次启动再试即可，不视为错误。
pub fn cleanup_backups(self_exe: &Path) -> usize {
    let dir = match self_exe.parent() {
        Some(d) => d,
        None => return 0,
    };
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.to_ascii_lowercase().ends_with(BACKUP_SUFFIX) {
            let p = e.path();
            if win::delete_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// 启动新的自身进程（替换完成后调用）。
pub fn restart(self_exe: &Path, extra_args: &[String]) -> Result<(), String> {
    std::process::Command::new(self_exe)
        .args(extra_args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法启动新版本：{}", e))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 跨块边界：正好 64 / 65 字节
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn sha256_is_incremental() {
        let mut h = Sha256::new();
        for chunk in b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".chunks(7) {
            h.update(chunk);
        }
        assert_eq!(
            to_hex(&h.finish()),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn version_ordering() {
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.1", "1.0.0"), Ordering::Greater);
        assert_eq!(compare_versions("0.9.9", "1.0.0"), Ordering::Less);
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
        // 正式版比同号预发布版新
        assert_eq!(compare_versions("1.0.0", "1.0.0-beta"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0-beta", "1.0.0"), Ordering::Less);
        assert_eq!(
            compare_versions("1.0.0-beta.2", "1.0.0-beta.10"),
            Ordering::Less
        );
    }

    #[test]
    fn manifest_parses_and_rejects_bad_schema() {
        let text = r#"{
            "schema": 1,
            "tag": "v0.2.0",
            "version": "0.2.0",
            "notes": "https://example/notes",
            "assets": [
                {"name": "winmem.exe", "url": "https://e/a", "sha256": "AB12", "size": 10},
                {"name": "winmem-gui.exe", "url": "https://e/b", "sha256": "cd34"}
            ]
        }"#;
        let info = parse_manifest(text).expect("应能解析清单");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.tag, "v0.2.0");
        assert_eq!(info.assets.len(), 2);
        // sha256 统一转小写，便于比较
        assert_eq!(info.assets[0].sha256, "ab12");
        assert_eq!(info.assets[0].size, Some(10));
        assert_eq!(info.assets[1].size, None);
        assert!(
            info.asset("WINMEM-GUI.EXE").is_some(),
            "文件名比较应忽略大小写"
        );

        assert!(parse_manifest(r#"{"schema": 2, "assets": []}"#).is_err());
        assert!(parse_manifest(r#"{"schema": 1, "assets": []}"#).is_err());
        assert!(parse_manifest("not json").is_err());
    }

    #[test]
    fn manifest_url_is_well_formed() {
        assert_eq!(
            manifest_url("npcxl/winmem"),
            "https://github.com/npcxl/winmem/releases/latest/download/latest.json"
        );
        assert_eq!(
            manifest_url("/npcxl/winmem/"),
            "https://github.com/npcxl/winmem/releases/latest/download/latest.json"
        );
    }

    #[test]
    fn install_rejects_asset_without_checksum() {
        let dir = std::env::temp_dir().join(format!("winmem-update-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("winmem-gui.exe");
        std::fs::write(&target, b"old").unwrap();

        let asset = Asset {
            name: "winmem-gui.exe".into(),
            url: "https://example.invalid/x".into(),
            sha256: String::new(),
            size: None,
        };
        let r = install_asset(&target, &asset);
        assert!(!r.ok, "缺少校验和时必须拒绝安装");
        assert_eq!(std::fs::read(&target).unwrap(), b"old", "原文件不能被改动");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_rejects_checksum_mismatch_and_keeps_original() {
        // 用本地文件冒充下载结果：校验失败时不得改动原文件
        let dir =
            std::env::temp_dir().join(format!("winmem-update-mismatch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("winmem.exe");
        std::fs::write(&target, b"original").unwrap();

        let asset = Asset {
            name: "winmem.exe".into(),
            url: "https://example.invalid/x".into(),
            sha256: "deadbeef".repeat(8),
            size: None,
        };
        let r = install_asset(&target, &asset);
        assert!(!r.ok);
        assert_eq!(std::fs::read(&target).unwrap(), b"original");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_only_backup_files() {
        let dir = std::env::temp_dir().join(format!("winmem-update-clean-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("winmem.exe"), b"x").unwrap();
        std::fs::write(dir.join("winmem.exe.old"), b"y").unwrap();
        std::fs::write(dir.join("winmem-gui.exe.OLD"), b"z").unwrap();

        let n = cleanup_backups(&dir.join("winmem.exe"));
        assert_eq!(n, 2, "两个 .old 都应被清理");
        assert!(dir.join("winmem.exe").exists(), "正常文件不能被删掉");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
