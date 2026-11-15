//! 脚本的基本约束与分段解析。

use regex::Regex;

use super::ShRunner;
use crate::hosttools::exec::one_line;
use crate::hosttools::scripts::{
    PRELUDE, action_script, conn_script, docker_script, kill_script, proc_detail_script,
    proc_script, read_script, service_log_script, service_script,
};
use crate::hosttools::sections::{SectionExt, Sections, users};

/// 合成一行后没有换行，没有 `!`（csh 的历史替换），没有注释，以 `export LC_ALL=C` 开头。
#[test]
fn scripts_are_one_line() {
    let comment = Regex::new(r"(^|[\s;])#").unwrap();
    for (name, script) in [
        ("proc", proc_script()),
        ("procDetail", proc_detail_script()),
        ("service", service_script()),
        ("docker", docker_script()),
        ("conn", conn_script()),
        ("action", action_script()),
        ("serviceLog", service_log_script()),
        ("read", read_script()),
        ("kill", kill_script()),
        ("privilege", crate::hosttools::privilege::privilege_script()),
        (
            "containerNames",
            crate::hosttools::scripts::container_names_script(),
        ),
    ] {
        let line = one_line(&script);
        assert!(!line.contains(['\n', '\r']), "{name}：合成后仍有换行");
        assert!(!line.contains('!'), "{name}：不能有 !");
        assert!(!comment.is_match(&line), "{name}：不能有注释");
        assert!(
            line.starts_with("export LC_ALL=C;"),
            "{name}：应以 export LC_ALL=C 开头"
        );
    }
}

/// 段标记只认行首的 `@@`；经 q 加了前缀的内容行（远端可控）即使以 `@@` 开头也只是内容，
/// 去掉一个空格后原样保留；脚本中的 q 在每行前加一个空格（含空行），没有输出时什么也不输出。
#[test]
fn sections_with_prefixed_content() {
    let sections =
        Sections::parse("@@exe\n /tmp/x\n @@passwd\n  root:0\n@@passwd\nwww-data:33\n@@empty\n");
    assert_eq!(sections.get("exe").text(), "/tmp/x\n@@passwd\n root:0");
    let names = users(sections.get("passwd"));
    assert_eq!(names.get(&33).map(String::as_str), Some("www-data"));
    assert_eq!(names.get(&0), None);
    assert!(sections.get("empty").is_some());
    assert_eq!(sections.get("empty").text(), "");
    assert_eq!(sections.get("none").text(), "");

    let Some(runner) = ShRunner::new() else {
        return;
    };
    let out = runner.run(
        &format!("{PRELUDE}printf 'a\\n@@b\\n\\n' | q; printf '' | q"),
        &[],
        &[],
    );
    assert_eq!(out, " a\n @@b\n \n");
}

/// 带参数的段另以"名称 参数"为键，同一个键只保留第一段；`@@` 后紧跟空白的不是标记。
#[test]
fn sections_with_arguments() {
    let sections = Sections::parse("@@proc tcp\na\n@@proc tcp6\nb\n@@ x\n@@total 12\n");
    assert_eq!(sections.get("proc").text(), "a");
    assert_eq!(sections.get("proc tcp").text(), "a");
    assert_eq!(sections.get("proc tcp6").text(), "b\n@@ x");
    assert_eq!(sections.get("total").num_arg(), Some(12));
    assert_eq!(sections.get("total").arg(), "12");
}
