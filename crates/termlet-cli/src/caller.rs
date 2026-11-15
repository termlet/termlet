//! 调用方：`termlet` 的祖先进程名（由近到远，不含自身），审计日志据此区分是用户手动执行还是 AI 工具调用。
//! 取不到时为空，不影响命令执行。Windows 用进程快照，Linux 读 `/proc`，其他平台不记录。

use std::collections::HashSet;

/// 最多记录的祖先进程层数。
const MAX_DEPTH: usize = 5;

/// 一个进程的父进程号与进程名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    pub name: String,
}

/// 本进程的祖先进程名。
pub fn chain() -> Vec<String> {
    match imp::lookup() {
        Some(lookup) => walk(std::process::id(), &lookup, MAX_DEPTH),
        None => Vec::new(),
    }
}

/// 从 `pid` 开始沿父进程链向上，收集最多 `limit` 个祖先的进程名。查不到、遇到 0 号进程或出现循环时停止
/// （Windows 上父进程退出后进程号可能被复用）。
pub fn walk(pid: u32, lookup: &dyn Fn(u32) -> Option<ProcInfo>, limit: usize) -> Vec<String> {
    let mut names = Vec::new();
    let Some(me) = lookup(pid) else {
        return names;
    };
    let mut seen = HashSet::from([pid]);
    let mut current = me.ppid;
    while names.len() < limit && current > 0 && seen.insert(current) {
        let Some(info) = lookup(current) else {
            break;
        };
        if !info.name.is_empty() {
            names.push(info.name);
        }
        current = info.ppid;
    }
    names
}

/// 解析 Linux 的 `/proc/<pid>/stat`：`<pid> (<comm>) <state> <ppid> …`。comm 可能包含空格和括号，按最后一个 `)` 切分。
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_stat(stat: &str) -> Option<ProcInfo> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close < open {
        return None;
    }
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some(ProcInfo {
        ppid,
        name: stat[open + 1..close].to_string(),
    })
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    use super::ProcInfo;

    /// 用进程快照建立进程号 →（父进程号，进程名）的表。
    pub(super) fn lookup() -> Option<impl Fn(u32) -> Option<ProcInfo>> {
        let mut table = HashMap::new();
        // SAFETY: 标准的快照遍历；entry 的 dwSize 先设好，句柄用完关闭
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snapshot, &mut entry);
            while ok != 0 {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                table.insert(
                    entry.th32ProcessID,
                    ProcInfo {
                        ppid: entry.th32ParentProcessID,
                        name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                    },
                );
                ok = Process32NextW(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
        }
        // 中途出错时用已读到的部分
        if table.is_empty() {
            return None;
        }
        Some(move |pid: u32| table.get(&pid).cloned())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{ProcInfo, parse_proc_stat};

    pub(super) fn lookup() -> Option<impl Fn(u32) -> Option<ProcInfo>> {
        Some(|pid: u32| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            parse_proc_stat(&stat)
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::ProcInfo;

    pub(super) fn lookup() -> Option<fn(u32) -> Option<ProcInfo>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn fake(table: &[(u32, u32, &str)]) -> impl Fn(u32) -> Option<ProcInfo> {
        let table: HashMap<u32, ProcInfo> = table
            .iter()
            .map(|&(pid, ppid, name)| {
                (
                    pid,
                    ProcInfo {
                        ppid,
                        name: name.into(),
                    },
                )
            })
            .collect();
        move |pid| table.get(&pid).cloned()
    }

    #[test]
    fn walks_up_to_the_limit() {
        let lookup = fake(&[
            (100, 90, "termlet"),
            (90, 80, "bash"),
            (80, 70, "node"),
            (70, 60, "Code.exe"),
            (60, 50, "explorer.exe"),
            (50, 40, "a"),
            (40, 0, "b"),
        ]);
        assert_eq!(
            walk(100, &lookup, 5),
            ["bash", "node", "Code.exe", "explorer.exe", "a"],
            "由近到远、最多 5 个、不含自身"
        );
        assert_eq!(walk(100, &lookup, 10).len(), 6, "0 号进程处停止");
    }

    #[test]
    fn stops_on_cycles_and_missing_parents() {
        let cycle = fake(&[(10, 20, "self"), (20, 30, "a"), (30, 20, "b")]);
        assert_eq!(walk(10, &cycle, 5), ["a", "b"], "循环时停止");
        let self_loop = fake(&[(10, 10, "self")]);
        assert!(walk(10, &self_loop, 5).is_empty());
        let missing = fake(&[(10, 20, "self")]);
        assert!(walk(10, &missing, 5).is_empty());
        assert!(walk(10, &fake(&[]), 5).is_empty());
    }

    #[test]
    fn parses_proc_stat() {
        let ok = |ppid: u32, name: &str| {
            Some(ProcInfo {
                ppid,
                name: name.into(),
            })
        };
        assert_eq!(
            parse_proc_stat("1234 (bash) S 1000 1234 1234 34816 ..."),
            ok(1000, "bash")
        );
        assert_eq!(
            parse_proc_stat("42 (my prog) R 7 42 42 0"),
            ok(7, "my prog")
        );
        assert_eq!(parse_proc_stat("42 (a) (b)) S 9 42"), ok(9, "a) (b)"));
        assert_eq!(parse_proc_stat("1 (systemd) S 0 1 1 0"), ok(0, "systemd"));
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("42 (x) S"), None);
        assert_eq!(parse_proc_stat("42 (x) S notanumber"), None);
    }

    // cargo test 直接运行测试程序，祖先中应有 cargo
    #[test]
    fn chain_on_this_platform() {
        let chain = chain();
        if cfg!(not(any(windows, target_os = "linux"))) {
            assert!(chain.is_empty());
            return;
        }
        assert!(!chain.is_empty() && chain.len() <= MAX_DEPTH, "{chain:?}");
        let names: Vec<String> = chain
            .iter()
            .map(|n| n.to_lowercase().trim_end_matches(".exe").to_string())
            .collect();
        // 由 cargo test 或 cargo nextest 运行（Unix 上 cargo 以 exec 换成 cargo-nextest，祖先中只有 cargo-nextest）
        assert!(
            names.iter().any(|n| n == "cargo" || n == "cargo-nextest"),
            "祖先中应有 cargo：{chain:?}"
        );
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_string_lossy();
        assert!(!chain[0].eq_ignore_ascii_case(&me), "不应包含自身");
    }
}
