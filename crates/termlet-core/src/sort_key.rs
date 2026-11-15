//! 分数索引（fractional index）：用字符串表示位置，任意两个键之间总能生成一个新键。
//!
//! 拖动排序时只改被拖动的那一条，同步时两台电脑各自调整过顺序也能按条目合并。
//! 键由 base62 数字组成，表示 0 与 1 之间的小数（`"V"` 约等于 0.5），按字节比较即按位置排序；
//! 键不以 `'0'` 结尾，因此任何两个不同的键之间都还有空位。

const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: u8 = 62;

fn value(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'A'..=b'Z' => c - b'A' + 10,
        b'a'..=b'z' => c - b'a' + 36,
        // 不合法的字符（数据被手工改过）按最小值处理，不让排序崩溃
        _ => 0,
    }
}

/// 生成一个排在 `before` 之后、`after` 之前的键。`None` 表示列表的开头 / 结尾。
///
/// `before` 必须小于 `after`；两者相等或顺序颠倒时（数据异常）返回排在 `before` 之后的键。
pub fn between(before: Option<&str>, after: Option<&str>) -> String {
    let a: Vec<u8> = before.unwrap_or("").bytes().map(value).collect();
    let b: Option<Vec<u8>> = after
        .map(|s| s.bytes().map(value).collect::<Vec<u8>>())
        .filter(|b| b.as_slice() > a.as_slice() && !b.is_empty());
    let digits = midpoint(&a, b.as_deref());
    digits
        .into_iter()
        .map(|d| DIGITS[d as usize] as char)
        .collect()
}

/// 在 `a` 与 `b`（`None` 表示 1）之间取一个数字串。`a`、`b` 是 base62 数字的值。
fn midpoint(a: &[u8], b: Option<&[u8]>) -> Vec<u8> {
    if let Some(b) = b {
        // 跳过公共前缀（a 不足的位按 0）
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(0) == b[n] {
            n += 1;
        }
        if n > 0 {
            let mut out = b[..n].to_vec();
            let rest_a = if n < a.len() { &a[n..] } else { &[][..] };
            out.extend(midpoint(rest_a, Some(&b[n..])));
            return out;
        }
    }
    let digit_a = a.first().copied().unwrap_or(0);
    let digit_b = b.map(|b| b[0]).unwrap_or(BASE);
    if digit_b - digit_a > 1 {
        return vec![(digit_a + digit_b) / 2];
    }
    // 首位相邻
    if let Some(b) = b
        && b.len() > 1
    {
        return vec![b[0]];
    }
    let mut out = vec![digit_a];
    let rest_a = if a.len() > 1 { &a[1..] } else { &[][..] };
    out.extend(midpoint(rest_a, None));
    out
}

/// 为 `count` 个按顺序排列的条目生成均匀分布的键（导入、首次排序时用）。
pub fn sequence(count: usize) -> Vec<String> {
    let mut keys = Vec::with_capacity(count);
    let mut last: Option<String> = None;
    for _ in 0..count {
        let key = between(last.as_deref(), None);
        last = Some(key.clone());
        keys.push(key);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key_is_middle() {
        assert_eq!(between(None, None), "V");
    }

    #[test]
    fn keys_sort_between_neighbours() {
        let first = between(None, None);
        let before = between(None, Some(&first));
        let after = between(Some(&first), None);
        assert!(before < first && first < after);
        let mid = between(Some(&before), Some(&first));
        assert!(before < mid && mid < first);
    }

    #[test]
    fn repeated_inserts_at_same_place_stay_ordered() {
        // 一直插在同一个位置（列表开头、两个相邻键之间）
        let mut low = between(None, None);
        let high = between(Some(&low), None);
        for _ in 0..500 {
            let mid = between(Some(&low), Some(&high));
            assert!(low < mid && mid < high, "{low} < {mid} < {high}");
            assert!(!mid.ends_with('0'));
            low = mid;
        }
        let mut top = between(None, None);
        for _ in 0..500 {
            let next = between(None, Some(&top));
            assert!(next < top, "{next} < {top}");
            top = next;
        }
    }

    #[test]
    fn sequence_is_strictly_increasing() {
        let keys = sequence(200);
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn bad_input_does_not_panic() {
        let key = between(Some("b"), Some("a"));
        assert!(key.as_str() > "b");
        let key = between(Some("a"), Some("a"));
        assert!(key.as_str() > "a");
        let _ = between(Some("!!"), None);
    }
}
