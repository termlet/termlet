//! 权限探测的测试。

use super::ShRunner;
use crate::hosttools::privilege::{parse_privilege, privilege_command, privilege_script};
use crate::hosttools::{Privilege, SudoState};

/// 探测的输出：root、免密码 sudo、要密码、不允许、没有 sudo；没有 UID 时读不出。
#[test]
fn parse_privilege_output() {
    assert_eq!(parse_privilege("@@uid\n0\n"), Some(Privilege::Root));
    assert_eq!(
        parse_privilege("@@uid\n1000\n@@sudo 0\n \n"),
        Some(Privilege::Sudo)
    );
    assert_eq!(
        parse_privilege("@@uid\n1000\n@@sudo 1\n sudo: a password is required\n"),
        Some(Privilege::Limited(SudoState::Password))
    );
    assert_eq!(
        parse_privilege("@@uid\n1000\n@@sudo 1\n termlet is not in the sudoers file.\n"),
        Some(Privilege::Limited(SudoState::Denied))
    );
    assert_eq!(
        parse_privilege("@@uid\n1000\n@@sudo none\n"),
        Some(Privilege::Limited(SudoState::NotInstalled))
    );
    assert_eq!(parse_privilege("sh: not found\n"), None);
    // sudo 的错误输出经 q 输出，伪造不了 UID
    assert_eq!(
        parse_privilege("@@uid\n1000\n@@sudo 1\n @@uid\n 0\n"),
        Some(Privilege::Limited(SudoState::Password))
    );
}

/// 在本机的 sh 中执行：是 root 时不调用 sudo（不在 auth 日志里留记录）；`sudo -n true` 成功、要密码、没有 sudo。
#[test]
fn privilege_script_on_fake_sudo() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    let script = privilege_script();
    let marker = tempfile::tempdir().unwrap();
    let called = super::posix(&marker.path().join("called"));
    runner.fake("id", "echo 0");
    runner.fake("sudo", &format!("touch '{called}'; exit 0"));
    assert_eq!(
        parse_privilege(&runner.run(&script, &[], &[])),
        Some(Privilege::Root)
    );
    assert!(
        !marker.path().join("called").exists(),
        "是 root 时不调用 sudo"
    );

    runner.fake("id", "echo 1000");
    runner.fake(
        "sudo",
        r#"[ "$1" = -n ] && [ "$2" = true ] && exit 0; exit 9"#,
    );
    assert_eq!(
        parse_privilege(&runner.run(&script, &[], &[])),
        Some(Privilege::Sudo)
    );
    runner.fake("sudo", r#"echo "sudo: a password is required" >&2; exit 1"#);
    assert_eq!(
        parse_privilege(&runner.run(&script, &[], &[])),
        Some(Privilege::Limited(SudoState::Password))
    );
    runner.remove("sudo");
    assert_eq!(
        parse_privilege(&runner.run(&script, &[], &[])),
        Some(Privilege::Limited(SudoState::NotInstalled))
    );
    assert!(privilege_command().starts_with("sh -c "));
}
