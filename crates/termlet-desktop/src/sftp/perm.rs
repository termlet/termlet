//! 修改权限对话框的位运算：对话框记下“全部设置”与“全部清除”的位，其余的位各项保持原样。
//!
//! 多选时各项原来的权限可能不同，提交时按每一项原来的权限分别计算新的权限，不会把第一项的权限套用到全部所选项。

/// 全部的权限位：所有者 / 组 / 其他人的读、写、执行，setuid、setgid、粘滞位。
pub const ALL_BITS: u32 = 0o7777;

/// 对权限的修改：`set` 中的位设置、`clear` 中的位清除（两者不重叠），其余的位各项保持原样。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermChange {
    pub set: u32,
    pub clear: u32,
}

/// 一位的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitState {
    On,
    Off,
    /// 各项不同（保持各自原样）
    Mixed,
}

/// 八进制写法：3 或 4 位，0–7777。
pub fn parse_octal(text: &str) -> Option<u32> {
    let text = text.trim();
    if !(3..=4).contains(&text.len()) || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(text, 8).ok()
}

/// 特殊位都没有时写 3 位，否则 4 位。
pub fn format_octal(mode: u32) -> String {
    if mode > 0o777 {
        format!("{mode:04o}")
    } else {
        format!("{mode:03o}")
    }
}

impl PermChange {
    /// 设为确定的权限：全部的位都确定。
    pub fn exact(mode: u32) -> Self {
        let set = mode & ALL_BITS;
        Self {
            set,
            clear: ALL_BITS & !set,
        }
    }

    /// 打开对话框时的状态：各项都有的位为设置，各项都没有的位为清除，其余的位各项不同。
    pub fn initial(perms: &[u32]) -> Self {
        if perms.is_empty() {
            return Self::exact(0o644);
        }
        let (all, any) = perms
            .iter()
            .fold((ALL_BITS, 0), |(all, any), p| (all & p, any | p));
        Self {
            set: all & ALL_BITS,
            clear: ALL_BITS & !any,
        }
    }

    /// 各项不同（保持各自原样）的位。
    pub fn mixed(self) -> u32 {
        ALL_BITS & !(self.set | self.clear)
    }

    pub fn state(self, bit: u32) -> BitState {
        if self.set & bit == bit {
            BitState::On
        } else if self.clear & bit == bit {
            BitState::Off
        } else {
            BitState::Mixed
        }
    }

    /// 点击某一位的复选框：设置 → 清除 → 设置……；打开对话框时各项不同的位多一个“保持各自原样”：
    /// 不确定 → 设置 → 清除 → 不确定（与 Windows 资源管理器中多选文件时的三态复选框相同）。
    pub fn cycle(self, bit: u32, mixed_at_start: u32) -> Self {
        let set = self.set & !bit;
        let clear = self.clear & !bit;
        match self.state(bit) {
            BitState::Mixed => Self {
                set: set | bit,
                clear,
            },
            BitState::On => Self {
                set,
                clear: clear | bit,
            },
            BitState::Off if mixed_at_start & bit != 0 => Self { set, clear },
            BitState::Off => Self {
                set: set | bit,
                clear,
            },
        }
    }

    /// 显示用的八进制：各项不同的位所在的那一位数字显示为 `?`（例如 `6??`）；特殊位全部清除时只显示 3 位。
    pub fn display(self) -> String {
        let mixed = self.mixed();
        if mixed == 0 {
            return format_octal(self.set);
        }
        let digits: Vec<char> = [0o7000u32, 0o700, 0o70, 0o7]
            .iter()
            .enumerate()
            .map(|(i, mask)| {
                if mixed & mask != 0 {
                    '?'
                } else {
                    let digit = (self.set & mask) >> (3 * (3 - i));
                    char::from_digit(digit, 8).unwrap_or('0')
                }
            })
            .collect();
        let start = if self.clear & 0o7000 == 0o7000 { 1 } else { 0 };
        digits[start..].iter().collect()
    }

    /// 按某一项原来的权限计算新的权限。
    pub fn apply(self, perm: u32) -> u32 {
        ((perm & !self.clear) | self.set) & ALL_BITS
    }
}

/// 按每一项原来的权限计算新的权限，新权限相同的项归为一组（一组提交一次），按第一次出现的顺序。
pub fn chmod_plan<'a>(items: &[(&'a str, u32)], change: PermChange) -> Vec<(u32, Vec<&'a str>)> {
    let mut groups: Vec<(u32, Vec<&'a str>)> = Vec::new();
    for (path, perm) in items {
        let mode = change.apply(*perm);
        match groups.iter_mut().find(|(m, _)| *m == mode) {
            Some((_, paths)) => paths.push(path),
            None => groups.push((mode, vec![path])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_parse_and_format() {
        assert_eq!(parse_octal("755"), Some(0o755));
        assert_eq!(parse_octal(" 4755 "), Some(0o4755));
        assert_eq!(parse_octal("75"), None);
        assert_eq!(parse_octal("789"), None);
        assert_eq!(parse_octal("77777"), None);
        assert_eq!(format_octal(0o644), "644");
        assert_eq!(format_octal(0o1777), "1777");
    }

    #[test]
    fn mixed_bits_cycle_through_keep() {
        let change = PermChange::initial(&[0o644, 0o600]);
        assert_eq!(change.state(0o400), BitState::On);
        assert_eq!(change.state(0o044), BitState::Mixed);
        assert_eq!(change.state(0o001), BitState::Off);
        assert_eq!(change.display(), "6??");
        let start = change.mixed();
        let a = change.cycle(0o040, start);
        assert_eq!(a.state(0o040), BitState::On);
        let b = a.cycle(0o040, start);
        assert_eq!(b.state(0o040), BitState::Off);
        let c = b.cycle(0o040, start);
        assert_eq!(
            c.state(0o040),
            BitState::Mixed,
            "原来各项不同的位可以回到保持原样"
        );
        // 原来确定的位只在设置与清除之间切换
        let d = change.cycle(0o001, start).cycle(0o001, start);
        assert_eq!(d.state(0o001), BitState::Off);
    }

    #[test]
    fn display_keeps_four_digits_for_special_bits() {
        assert_eq!(PermChange::exact(0o4755).display(), "4755");
        let change = PermChange::initial(&[0o4755, 0o755]);
        assert_eq!(change.display(), "?755");
    }

    #[test]
    fn plan_applies_per_item_and_groups() {
        let change = PermChange {
            set: 0o100,
            clear: 0o002,
        };
        let plan = chmod_plan(&[("a", 0o644), ("b", 0o666), ("c", 0o644)], change);
        assert_eq!(plan, vec![(0o744, vec!["a", "c"]), (0o764, vec!["b"])]);
    }
}
