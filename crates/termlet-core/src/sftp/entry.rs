//! 目录列表中的条目（远程与本机共用的种类），权限的显示写法。

use russh_sftp::protocol::FileAttributes;

use super::client::{
    DirItem, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, file_type, mtime_of, perm_bits, size_of,
};

/// 条目的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// 设备文件、命名管道、套接字等（传输时跳过）
    Special,
}

impl EntryKind {
    pub(crate) fn of(attrs: &FileAttributes) -> Self {
        match file_type(attrs) {
            S_IFREG => EntryKind::File,
            S_IFDIR => EntryKind::Dir,
            S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Special,
        }
    }
}

/// 远程目录中的一项（列目录、查看属性的结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// 符号链接指向的目标的种类；不是链接或目标不存在（悬空链接）时为 `None`
    pub target: Option<EntryKind>,
    /// 字节数（符号链接是链接本身的长度）
    pub size: u64,
    /// 修改时间（Unix 秒）；服务器没有报告时为 0
    pub modified: i64,
    /// 权限位（八进制 0–07777，含 setuid、setgid、粘滞位），修改权限时作为初始值。
    /// 符号链接取目标的（修改权限作用于目标；链接本身通常是 777）；悬空链接保持链接本身的
    pub perm: u32,
    /// `ls -l` 的写法，例如 `drwxr-xr-x`、`lrwxrwxrwx`
    pub mode: String,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// 属主、属组的名称（服务器在列表中给出时，OpenSSH 会给）
    pub owner: Option<String>,
    pub group: Option<String>,
}

impl Entry {
    /// 目录，或指向目录的符号链接。
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir || self.target == Some(EntryKind::Dir)
    }

    pub fn is_link(&self) -> bool {
        self.kind == EntryKind::Symlink
    }

    pub(crate) fn from_attrs(name: String, attrs: &FileAttributes) -> Self {
        Self {
            name,
            kind: EntryKind::of(attrs),
            target: None,
            size: size_of(attrs),
            modified: mtime_of(attrs),
            perm: perm_bits(attrs),
            mode: mode_string(attrs.permissions.unwrap_or(S_IFREG)),
            uid: attrs.uid,
            gid: attrs.gid,
            owner: None,
            group: None,
        }
    }

    pub(crate) fn from_item(item: &DirItem) -> Self {
        let mut entry = Self::from_attrs(item.name.clone(), &item.attrs);
        if let Some((owner, group)) = owner_group(&item.longname) {
            entry.owner = Some(owner);
            entry.group = Some(group);
        }
        entry
    }

    /// 符号链接按目标判断是否为目录，权限也取目标的；`target` 为 `None` 表示目标不存在。
    pub(crate) fn resolve_link(&mut self, target: Option<&FileAttributes>) {
        if let Some(attrs) = target {
            self.target = Some(EntryKind::of(attrs));
            self.perm = perm_bits(attrs);
        }
    }
}

/// 目录列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// 解析后的绝对路径
    pub path: String,
    pub entries: Vec<Entry>,
}

/// `ls -l` 写法的权限：类型字符加九位权限，setuid、setgid、粘滞位写在执行位上（s/S、t/T）。
pub fn mode_string(mode: u32) -> String {
    let kind = match mode & S_IFMT {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'p',
        0o140000 => 's',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    };
    let bit = |mask: u32, c: char| if mode & mask != 0 { c } else { '-' };
    let exec = |mask: u32, special: u32, on: char, off: char| match (
        mode & mask != 0,
        mode & special != 0,
    ) {
        (true, true) => on,
        (false, true) => off,
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        kind,
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        exec(0o100, 0o4000, 's', 'S'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        exec(0o010, 0o2000, 's', 'S'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        exec(0o001, 0o1000, 't', 'T'),
    ]
    .iter()
    .collect()
}

/// 从 `ls -l` 式的描述中取属主与属组（`-rw-r--r--  1 root  root  5 Oct 19 10:00 a.txt`）。
fn owner_group(longname: &str) -> Option<(String, String)> {
    let fields: Vec<&str> = longname.split_whitespace().collect();
    let perms = fields.first()?;
    if fields.len() < 8 || perms.len() < 10 {
        return None;
    }
    Some((fields[2].to_string(), fields[3].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_strings() {
        assert_eq!(mode_string(0o100644), "-rw-r--r--");
        assert_eq!(mode_string(0o040755), "drwxr-xr-x");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx");
        assert_eq!(mode_string(0o104750), "-rwsr-x---");
        assert_eq!(mode_string(0o041777), "drwxrwxrwt");
        assert_eq!(mode_string(0o102644), "-rw-r-Sr--");
        assert_eq!(
            owner_group("-rw-r--r--    1 termlet  staff      5 Oct 19 10:00 a b.txt"),
            Some(("termlet".into(), "staff".into()))
        );
        assert_eq!(owner_group(""), None);
    }
}
