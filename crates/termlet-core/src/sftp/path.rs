//! 远程（POSIX）路径：SFTP 的路径一律用 `/` 分隔，与本机的 Windows 路径无关。规则与 Go 的 `path` 包相同。

/// 规范化：合并重复的 `/`，去掉 `.`，处理 `..`（根目录的上级仍是根目录）；空路径为 `.`。
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// 拼接并规范化（`name` 是绝对路径时就是它）。
pub fn join(base: &str, name: &str) -> String {
    if name.starts_with('/') || base.is_empty() {
        return clean(name);
    }
    if name.is_empty() {
        return clean(base);
    }
    clean(&format!("{base}/{name}"))
}

/// 最后一级名称；根目录为 `/`，空路径为 `.`。
pub fn base_name(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// 上级目录（规范化后）；根目录的上级仍是根目录。
pub fn parent(path: &str) -> String {
    let (dir, _) = split(path);
    clean(dir)
}

/// 拆成目录（含末尾的 `/`）与名称，两者拼起来就是原路径。
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_like_go() {
        for (input, want) in [
            ("", "."),
            ("/", "/"),
            ("//a//b/", "/a/b"),
            ("/a/./b/../c", "/a/c"),
            ("/../a", "/a"),
            ("a/../..", ".."),
            ("a/b/..", "a"),
            ("/a/b/../../..", "/"),
        ] {
            assert_eq!(clean(input), want, "{input}");
        }
        assert_eq!(join("/home/u", "x/y"), "/home/u/x/y");
        assert_eq!(join("/home/u", "/etc"), "/etc");
        assert_eq!(join("/home/u", "../v"), "/home/v");
        assert_eq!(base_name("/a/b.txt"), "b.txt");
        assert_eq!(base_name("/"), "/");
        assert_eq!(base_name("/a/"), "a");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(split("/d/a.txt"), ("/d/", "a.txt"));
        assert_eq!(split("a.txt"), ("", "a.txt"));
    }
}
