//! 版本号比较：`a.b.c[-预发布]`，可以带 `v` 前缀、少写几段，
//! 预发布标识按 SemVer 的规则比较（数字段按数值，其余按字符串，数字段低于字符串段，正式版高于同号的预发布版）。

use std::cmp::Ordering;

fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut x = a.split('.');
    let mut y = b.split('.');
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let pn = !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
                let qn = !q.is_empty() && q.bytes().all(|c| c.is_ascii_digit());
                let order = match (pn, qn) {
                    // 数值比较：去掉前导零后先比长度再比字典序，不会溢出
                    (true, true) => {
                        let p = p.trim_start_matches('0');
                        let q = q.trim_start_matches('0');
                        p.len().cmp(&q.len()).then_with(|| p.cmp(q))
                    }
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => p.cmp(q),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn parse(v: &str) -> ([u64; 3], &str) {
    let s = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let (main, pre) = s.split_once('-').unwrap_or((s, ""));
    let mut nums = [0u64; 3];
    for (slot, part) in nums.iter_mut().zip(main.split('.')) {
        // 与 JavaScript 的 parseInt 相同：取开头的数字，没有时按 0
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        *slot = digits.parse().unwrap_or(0);
    }
    (nums, pre)
}

/// 比较两个版本号。
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (x, xp) = parse(a);
    let (y, yp) = parse(b);
    x.cmp(&y)
        .then_with(|| match (xp.is_empty(), yp.is_empty()) {
            _ if xp == yp => Ordering::Equal,
            // 正式版高于同号的预发布版
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => compare_pre(xp, yp),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    #[test]
    fn compares_each_part_numerically() {
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Greater);
        assert_eq!(compare_versions("v0.2.0", "0.10.0"), Less);
        assert_eq!(compare_versions("1.0", "1.0.0"), Equal);
        assert_eq!(compare_versions("2.0.0", "10.0.0"), Less);
    }

    #[test]
    fn release_is_newer_than_its_prereleases() {
        assert_eq!(compare_versions("1.0.0", "1.0.0-beta.2"), Greater);
        assert_eq!(compare_versions("1.0.0-beta.1", "1.0.0-beta.2"), Less);
        // 预发布序号按数值比较
        assert_eq!(compare_versions("1.0.0-beta.10", "1.0.0-beta.2"), Greater);
        assert_eq!(compare_versions("1.0.0-rc.1", "1.0.0-beta.9"), Greater);
        assert_eq!(compare_versions("1.0.0-beta", "1.0.0-beta.1"), Less);
        // 数字段低于字符串段
        assert_eq!(compare_versions("1.0.0-1", "1.0.0-alpha"), Less);
        assert_eq!(compare_versions("1.0.0-beta.1", "1.0.0-beta.1"), Equal);
        // 很长的数字段也不会溢出
        assert_eq!(
            compare_versions("1.0.0-99999999999999999999999", "1.0.0-100"),
            Greater
        );
    }
}
