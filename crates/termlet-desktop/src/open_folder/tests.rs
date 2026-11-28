use super::*;

// 换成绝对路径：相对路径按启动时的当前目录；空格、中文原样；去掉资源管理器的菜单加的 `\.`（盘符根目录也对）与末尾的
// 分隔符；空的不算
#[test]
fn resolves_against_the_current_directory() {
    let cwd = if cfg!(windows) {
        PathBuf::from(r"C:\Users\ruan")
    } else {
        PathBuf::from("/home/ruan")
    };
    assert_eq!(resolve("  ", &cwd), None);
    assert_eq!(resolve("", &cwd), None);
    assert_eq!(resolve(".", &cwd), Some(cwd.clone()));
    assert_eq!(resolve("项目 a", &cwd), Some(cwd.join("项目 a")));
    if cfg!(windows) {
        let cases = [
            (r"D:\my work\项目", r"D:\my work\项目"),
            (r"D:\my work\项目\.", r"D:\my work\项目"),
            (r"D:\my work\", r"D:\my work"),
            (r"C:\\.", r"C:\"),
            (r"C:\.", r"C:\"),
            // 命令行中的 "D:\" 拆出来是 D:"
            (r#"D:""#, r"D:\"),
            (r#"D:\work""#, r"D:\work"),
            (r"\\server\share\dir\.", r"\\server\share\dir"),
        ];
        for (raw, want) in cases {
            assert_eq!(resolve(raw, &cwd), Some(PathBuf::from(want)), "{raw}");
        }
        assert_eq!(
            resolve(r"..\x", &cwd),
            Some(PathBuf::from(r"C:\Users\ruan\..\x"))
        );
    } else {
        assert_eq!(
            resolve("/srv/a b/./c/", &cwd),
            Some(PathBuf::from("/srv/a b/c"))
        );
        assert_eq!(resolve("/", &cwd), Some(PathBuf::from("/")));
        // 结尾的引号是名称的一部分
        assert_eq!(
            resolve(r#"/tmp/x""#, &cwd),
            Some(PathBuf::from(r#"/tmp/x""#))
        );
    }
    assert_eq!(
        forwarded(&cwd.join("项目 a")),
        [
            "--open-folder".to_string(),
            cwd.join("项目 a").display().to_string()
        ]
    );
}

// 交给已在运行的实例的参数，在那边照样解析出同一个目录
#[test]
fn forwarded_arguments_parse_back() {
    let dir = std::env::temp_dir().join("Termlet 项目 \"x");
    let parsed = crate::args::parse(forwarded(&dir));
    assert_eq!(
        parsed
            .open_folder
            .and_then(|raw| resolve(&raw, Path::new("/elsewhere"))),
        Some(dir.components().collect())
    );
    assert!(parsed.rest.is_empty());
}

#[gpui_kit::test]
fn queue_is_taken_once(cx: &mut gpui_kit::TestAppContext) {
    cx.update(|cx| {
        assert!(!pending(cx));
        assert!(take(cx).is_empty());
        queue(PathBuf::from("a"), cx);
        queue(PathBuf::from("b"), cx);
        assert!(pending(cx));
        assert_eq!(take(cx), [PathBuf::from("a"), PathBuf::from("b")]);
        assert!(!pending(cx));
        assert!(take(cx).is_empty());
    });
}
