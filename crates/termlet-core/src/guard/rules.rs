//! 危险命令规则：内置规则、自定义正则规则与匹配。
//!
//! 内置规则按 Go RE2 的语义写，以关闭 Unicode 的字节正则编译：`\b`、`\s`、`\S`、`\w`、`\d` 与 RE2 一样只认 ASCII
//! （唯一的差别是这里的 `\s` 含垂直制表符 `\v`，RE2 不含）。自定义规则按 `regex` crate 的默认语法编译（Unicode），
//! 与 RE2 的差别见 [`validate_custom_rule`]。

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::NoExpand;
use regex::bytes::{Regex, RegexBuilder};

use crate::model::GuardSettings;

/// 自定义规则的数量上限。
pub const MAX_CUSTOM_RULES: usize = 100;
/// 一条自定义规则的长度上限（字节）。
pub const MAX_PATTERN_LENGTH: usize = 500;
/// 自定义规则编译后的大小上限：正常的规则只有几 KB，防止 `(?:\w{100}){100}` 这类规则占用大量内存。
const CUSTOM_SIZE_LIMIT: usize = 1 << 20;

/// 命令开头：
/// - 每一行的行首（多行脚本），或 `;` `&` `|` `(` `` ` `` `$(` 与回车之后（在终端中回车即执行）；
/// - 以上位置紧随其后的 `if` / `then` / `do` / `else` 等 shell 关键字，以及 `!` 与 `{`（与 shell 相同，
///   二者只在命令开头、且后面有空白时才是关键字：`Done! reboot`、`# ! halt`、`${!name}`、`awk '{…}'` 都不算）；
/// - `sudo` / `doas`（含带参数的选项 `-u root`）、`env` / `nice` / `command` 等前缀之后，`timeout <时长>` 之后，
///   `sh -c` / `bash -c` 等之后（可带引号）；
/// - 以上位置之后还可以有变量赋值（`LANG=C reboot`），命令名可以带路径（`/sbin/reboot`、`./rm`）或反斜杠（`\rm`）。
///
/// 行首只在这里按多行匹配，各规则其余部分的 `^` `$` 语义不变（结尾用 `\s` 判断，换行同样算作结束）。
/// 按行检查、不做 shell 解析是已知的取舍：多行引号字符串与 heredoc 正文中位于行首的词同样算作命令开头，
/// 引号与注释中的 `;` `&&` `|` 之后的词也一样（`echo 'a; reboot'`、`ls # && reboot`）。宁可误拦（可以请用户批准），
/// 不能因为解析错引号或注释而漏掉真正执行的命令。
/// case 分支中与模式同一行的命令（`stop) reboot ;;`）不在这里处理：把 `)` 之后算作命令开头会误报 `$(date) reboot`、
/// 正文中的 `1) reboot` 这类编号，加进这里会让每条规则都变慢。见 [`case_lines`]。
macro_rules! start {
    () => {
        concat!(
            r"(?:(?:(?m:^)|[;&|(\x60\r]|\$\()\s*(?:(?:if|then|do|else|elif|while|until|!|\{)\s+)*",
            r"|\b(?:sudo|doas)\s+(?:(?:-[ugCDhpRrTU]\s*[^\s-]\S*",
            r"|--(?:user|group|chdir|host|prompt|role|type|other-user|close-from|chroot|command-timeout)(?:=|\s+)\S+",
            r"|-\S+)\s+)*",
            r"|\benv\s+(?:-[uCS]\s+\S+\s+|-\S+\s+)*",
            r"|\b(?:nice|ionice)\s+(?:-[a-zA-Z]\s+-?\d\S*\s+|-\S+\s+)*",
            r"|\bcommand\s+(?:-p\s+)*",
            r"|\b(?:nohup|exec|xargs|time|stdbuf|setsid)\s+(?:-\S+\s+)*",
            r"|\b(?:ba|z|da|k|c|tc)?sh\s+(?:-\S+\s+)*-[a-zA-Z]*c[a-zA-Z]*\s+['\x22]?",
            r"|\btimeout\s+(?:-[ks]\s+\S+\s+|-\S+\s+)*\d[\d.]*[smhd]?\s+)\s*",
            r"(?:[A-Za-z_]\w*=\S*\s+)*(?:\\|[^\s;&|()<>'\x22\x60]*/)?",
        )
    };
}

/// 一条规则：内置规则或设置中的自定义规则。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    id: Cow<'static, str>,
    pattern: Cow<'static, str>,
    /// 内置规则的说明；自定义规则没有说明
    text: Option<Texts>,
}

/// 内置规则的说明：简体中文、英文，以及桌面端界面用的繁體中文、日文、韩文（命令行只有简体与英文）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Texts {
    zh: &'static str,
    en: &'static str,
    zh_tw: &'static str,
    ja: &'static str,
    ko: &'static str,
}

impl Rule {
    const fn builtin(id: &'static str, pattern: &'static str, text: Texts) -> Self {
        Self {
            id: Cow::Borrowed(id),
            pattern: Cow::Borrowed(pattern),
            text: Some(text),
        }
    }

    /// 内置规则为固定的 ID（`rm-root` 等，发布后不能改）；自定义规则为 `custom-<序号>`，
    /// 序号是在设置中的位置（从 1 起，空的规则也占位）。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 正则表达式。
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn is_builtin(&self) -> bool {
        self.text.is_some()
    }

    /// 中文说明；自定义规则为正则本身。
    pub fn description(&self) -> &str {
        self.text.map_or(&self.pattern, |t| t.zh)
    }

    /// 英文说明；自定义规则为正则本身。
    pub fn description_en(&self) -> &str {
        self.text.map_or(&self.pattern, |t| t.en)
    }

    /// 繁體中文说明（桌面端的繁体界面用；命令行只有简体与英文）；自定义规则为正则本身。
    pub fn description_zh_tw(&self) -> &str {
        self.text.map_or(&self.pattern, |t| t.zh_tw)
    }

    /// 日文说明（桌面端的日文界面用）；自定义规则为正则本身。
    pub fn description_ja(&self) -> &str {
        self.text.map_or(&self.pattern, |t| t.ja)
    }

    /// 韩文说明（桌面端的韩文界面用）；自定义规则为正则本身。
    pub fn description_ko(&self) -> &str {
        self.text.map_or(&self.pattern, |t| t.ko)
    }
}

/// 内置规则与它的关键字。
struct Builtin {
    rule: Rule,
    /// 命中这条规则时命令中必然出现的文字（之一），都不出现时不必运行正则（见 [`Compiled::may_match`]）。
    /// 必须与规则的正则一致：正则能命中的命令一定含其中之一。以 `(?i)` 开头的规则按小写比较，关键字写成小写
    keywords: &'static [&'static str],
}

const fn builtin(
    id: &'static str,
    pattern: &'static str,
    text: Texts,
    keywords: &'static [&'static str],
) -> Builtin {
    Builtin {
        rule: Rule::builtin(id, pattern, text),
        keywords,
    }
}

/// 内置规则，按顺序匹配。ID 发布后不能改：设置中按 ID 关闭内置规则。
static BUILTINS: &[Builtin] = &[
    builtin(
        "rm-root",
        concat!(
            start!(),
            r"rm\s+(?:-\S+\s+)*(?:--recursive|-[a-zA-Z]*[rR][a-zA-Z]*)\s+(?:-\S+\s+)*['\x22]?",
            r"(?:/|/\*|~|~/|~/\*|\$HOME/?|\$\{HOME\}/?",
            r"|/(?:bin|boot|dev|etc|home|lib|lib64|opt|proc|root|sbin|srv|sys|usr|var)/?\*?)",
            r"(?:\s|$|[;&|)'\x22])",
        ),
        Texts {
            zh: "递归删除根目录、家目录或系统目录",
            en: "Recursively delete the root, home or a system directory",
            zh_tw: "遞迴刪除根目錄、主目錄或系統目錄",
            ja: "ルート、ホーム、システムディレクトリを再帰的に削除",
            ko: "루트, 홈 또는 시스템 디렉터리를 재귀적으로 삭제",
        },
        &["rm"],
    ),
    builtin(
        "rm-no-preserve-root",
        r"--no-preserve-root",
        Texts {
            zh: "rm --no-preserve-root",
            en: "rm --no-preserve-root",
            zh_tw: "rm --no-preserve-root",
            ja: "rm --no-preserve-root",
            ko: "rm --no-preserve-root",
        },
        &["--no-preserve-root"],
    ),
    builtin(
        "mkfs",
        concat!(start!(), r"mkfs(?:\.\w+)?\b"),
        Texts {
            zh: "格式化文件系统（mkfs）",
            en: "Format a file system (mkfs)",
            zh_tw: "格式化檔案系統（mkfs）",
            ja: "ファイルシステムをフォーマット（mkfs）",
            ko: "파일 시스템 포맷(mkfs)",
        },
        &["mkfs"],
    ),
    builtin(
        "wipe-disk",
        concat!(start!(), r"(?:wipefs|sfdisk|sgdisk|blkdiscard)\b"),
        Texts {
            zh: "擦除磁盘或分区表（wipefs、sfdisk、sgdisk、blkdiscard）",
            en: "Wipe a disk or partition table (wipefs, sfdisk, sgdisk, blkdiscard)",
            zh_tw: "抹除磁碟或分割表（wipefs、sfdisk、sgdisk、blkdiscard）",
            ja: "ディスクやパーティションテーブルを消去（wipefs、sfdisk、sgdisk、blkdiscard）",
            ko: "디스크 또는 파티션 테이블 지우기(wipefs, sfdisk, sgdisk, blkdiscard)",
        },
        &["wipefs", "sfdisk", "sgdisk", "blkdiscard"],
    ),
    builtin(
        "dd-device",
        concat!(
            start!(),
            r"dd\b[^;&|\n]*\bof=/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|disk|mapper/)"
        ),
        Texts {
            zh: "用 dd 直接写磁盘设备",
            en: "Write to a disk device with dd",
            zh_tw: "用 dd 直接寫入磁碟裝置",
            ja: "dd でディスクデバイスに直接書き込み",
            ko: "dd로 디스크 장치에 직접 쓰기",
        },
        &["of=/dev/"],
    ),
    builtin(
        "write-device",
        r">\s*/dev/(?:sd|hd|vd|xvd|nvme|mmcblk)\w*",
        Texts {
            zh: "重定向输出覆盖磁盘设备",
            en: "Redirect output onto a disk device",
            zh_tw: "重新導向輸出覆寫磁碟裝置",
            ja: "リダイレクトでディスクデバイスを上書き",
            ko: "리디렉션으로 디스크 장치 덮어쓰기",
        },
        &["/dev/"],
    ),
    builtin(
        "shred-device",
        concat!(start!(), r"shred\b[^;&|\n]*/dev/"),
        Texts {
            zh: "用 shred 擦除设备",
            en: "Shred a device",
            zh_tw: "用 shred 抹除裝置",
            ja: "shred でデバイスを消去",
            ko: "shred로 장치 지우기",
        },
        &["shred"],
    ),
    builtin(
        "power",
        concat!(
            start!(),
            r"(?:shutdown|reboot|halt|poweroff)\b|",
            start!(),
            r"init\s+[06]\b|",
            start!(),
            r"systemctl\s+(?:-\S+\s+)*(?:reboot|poweroff|halt|kexec)\b",
        ),
        Texts {
            zh: "关机或重启（shutdown、reboot、halt、poweroff、init 0/6）",
            en: "Shut down or reboot (shutdown, reboot, halt, poweroff, init 0/6)",
            zh_tw: "關機或重新啟動（shutdown、reboot、halt、poweroff、init 0/6）",
            ja: "シャットダウンまたは再起動（shutdown、reboot、halt、poweroff、init 0/6）",
            ko: "시스템 종료 또는 다시 시작(shutdown, reboot, halt, poweroff, init 0/6)",
        },
        &["shutdown", "reboot", "halt", "poweroff", "init", "kexec"],
    ),
    builtin(
        "fork-bomb",
        r":\s*\(\s*\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:",
        Texts {
            zh: "fork 炸弹",
            en: "Fork bomb",
            zh_tw: "fork 炸彈",
            ja: "fork 爆弾",
            ko: "포크 폭탄",
        },
        &["{"],
    ),
    builtin(
        "chmod-root",
        concat!(
            start!(),
            r"(?:chmod|chown|chgrp)\s+(?:-\S+\s+)*-[a-zA-Z]*R[a-zA-Z]*\s+(?:-\S+\s+)*\S+\s+['\x22]?/(?:\s|$|[;&|'\x22])",
        ),
        Texts {
            zh: "递归修改根目录的权限或属主",
            en: "Recursively change permissions or owner of the root directory",
            zh_tw: "遞迴變更根目錄的權限或擁有者",
            ja: "ルートディレクトリの権限や所有者を再帰的に変更",
            ko: "루트 디렉터리의 권한 또는 소유자를 재귀적으로 변경",
        },
        &["chmod", "chown", "chgrp"],
    ),
    builtin(
        "kill-all",
        concat!(
            start!(),
            r"kill\s+(?:-\S+\s+)*-1(?:\s|$|[;&|'\x22])|",
            start!(),
            r"killall5\b",
        ),
        Texts {
            zh: "结束所有进程（kill -1、killall5）",
            en: "Kill all processes (kill -1, killall5)",
            zh_tw: "結束所有處理程序（kill -1、killall5）",
            ja: "すべてのプロセスを終了（kill -1、killall5）",
            ko: "모든 프로세스 종료(kill -1, killall5)",
        },
        &["kill"],
    ),
    builtin(
        "firewall-flush",
        concat!(
            start!(),
            r"iptables\s+(?:-\S+\s+)*-(?:F|-flush)\b|",
            start!(),
            r"nft\s+flush\s+ruleset\b",
        ),
        Texts {
            zh: "清空防火墙规则",
            en: "Flush firewall rules",
            zh_tw: "清除防火牆規則",
            ja: "ファイアウォールのルールをすべて削除",
            ko: "방화벽 규칙 모두 삭제",
        },
        &["iptables", "nft"],
    ),
    builtin(
        "crontab-remove",
        concat!(start!(), r"crontab\s+(?:-\S+\s+)*-r\b"),
        Texts {
            zh: "删除全部定时任务（crontab -r）",
            en: "Remove all cron jobs (crontab -r)",
            zh_tw: "刪除全部排程工作（crontab -r）",
            ja: "すべての cron ジョブを削除（crontab -r）",
            ko: "모든 cron 작업 삭제(crontab -r)",
        },
        &["crontab"],
    ),
    builtin(
        "overwrite-auth-files",
        r">\s*/etc/(?:passwd|shadow|group|sudoers)\b",
        Texts {
            zh: "覆盖 /etc/passwd、shadow、sudoers 等账号文件",
            en: "Overwrite account files such as /etc/passwd, shadow or sudoers",
            zh_tw: "覆寫 /etc/passwd、shadow、sudoers 等帳號檔案",
            ja: "/etc/passwd、shadow、sudoers などのアカウントファイルを上書き",
            ko: "/etc/passwd, shadow, sudoers 등 계정 파일 덮어쓰기",
        },
        &["/etc/"],
    ),
    builtin(
        "sql-drop",
        r"(?i)\bdrop\s+(?:database|schema)\b",
        Texts {
            zh: "删除数据库（DROP DATABASE / SCHEMA）",
            en: "Drop a database (DROP DATABASE / SCHEMA)",
            zh_tw: "刪除資料庫（DROP DATABASE / SCHEMA）",
            ja: "データベースを削除（DROP DATABASE / SCHEMA）",
            ko: "데이터베이스 삭제(DROP DATABASE / SCHEMA)",
        },
        &["drop"],
    ),
];

/// 编译好的内置规则（与 [`BUILTINS`] 一一对应），所有 [`Guard`] 共用。
static BUILTIN_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    BUILTINS
        .iter()
        .map(|b| {
            RegexBuilder::new(b.rule.pattern())
                .unicode(false)
                .build()
                .expect("内置的危险命令规则")
        })
        .collect()
});

/// 全部内置规则，按匹配顺序（设置界面列出、按 ID 开关）。
pub fn builtin_rules() -> impl ExactSizeIterator<Item = &'static Rule> {
    BUILTINS.iter().map(|b| &b.rule)
}

/// 是否为内置规则的 ID（设置中关闭的规则只能是内置规则）。
pub fn is_builtin(id: &str) -> bool {
    BUILTINS.iter().any(|b| b.rule.id() == id)
}

/// 自定义规则不能使用的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("规则过长（最多 {MAX_PATTERN_LENGTH} 字节）")]
    TooLong,
    /// 详情是 `regex` crate 的英文报错，只写日志
    #[error("规则不是合法的正则表达式：{0}")]
    Syntax(String),
    #[error("规则过于复杂")]
    TooComplex,
    #[error("规则会匹配任何命令")]
    MatchesEverything,
    #[error("自定义规则最多 {MAX_CUSTOM_RULES} 条")]
    TooMany,
}

/// 设置中不能使用的自定义规则（构造 [`Guard`] 时跳过）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRule {
    /// 在 `custom_rules` 中的位置（从 0 起）
    pub index: usize,
    pub pattern: String,
    pub error: RuleError,
}

/// 校验并编译一条自定义规则：区分大小写，需要时在开头加 `(?i)`。
///
/// 语法是 `regex` crate 的，与 Go RE2 基本相同；不同之处：`\b` `\w` `\s` `\d` 认 Unicode（RE2 只认 ASCII，
/// 紧挨着中文的词因此可能没有词的边界），不支持 `\Q…\E`（报语法错误）。
fn compile_custom(pattern: &str) -> Result<Regex, RuleError> {
    if pattern.len() > MAX_PATTERN_LENGTH {
        return Err(RuleError::TooLong);
    }
    let regex = RegexBuilder::new(pattern)
        .size_limit(CUSTOM_SIZE_LIMIT)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => RuleError::TooComplex,
            other => RuleError::Syntax(other.to_string()),
        })?;
    if regex.is_match(b"") {
        return Err(RuleError::MatchesEverything);
    }
    Ok(regex)
}

/// 校验一条自定义规则（设置界面在输入时提示），规则见 [`Guard::from_settings`]。
pub fn validate_custom_rule(pattern: &str) -> Result<(), RuleError> {
    compile_custom(pattern).map(drop)
}

#[derive(Clone, Debug)]
struct Compiled {
    rule: Rule,
    regex: Regex,
    /// 见 [`Builtin::keywords`]；为空时总是运行正则
    keywords: &'static [&'static str],
    /// 关键字不区分大小写（规则以 `(?i)` 开头）
    fold: bool,
}

impl Compiled {
    /// 用关键字快速排除：返回 false 时正则一定不会命中。`lower` 是按需计算的小写命令（不区分大小写的规则用）。
    fn may_match(&self, command: &str, lower: &mut Option<String>) -> bool {
        if self.keywords.is_empty() {
            return true;
        }
        let text = if self.fold {
            lower.get_or_insert_with(|| command.to_ascii_lowercase())
        } else {
            command
        };
        self.keywords.iter().any(|k| text.contains(k))
    }
}

/// 危险命令检查：未关闭的内置规则与自定义规则，按顺序匹配。
///
/// 定位是防止误操作（例如 AI 工具生成了 `rm -rf /`），不是安全边界：命令可以通过编码、变量拼接、
/// 脚本文件等方式绕过字符串匹配。真正的权限边界在服务器端（CLI 专用的低权限账号、sudoers）。
#[derive(Clone, Debug)]
pub struct Guard {
    rules: Vec<Compiled>,
}

impl Default for Guard {
    /// 全部内置规则，没有自定义规则。
    fn default() -> Self {
        Self::from_settings(&GuardSettings::default()).0
    }
}

impl Guard {
    /// 按设置创建：跳过关闭的内置规则（不认识的 ID 忽略）与空的自定义规则；不合法的自定义规则跳过并返回，
    /// 超过 [`MAX_CUSTOM_RULES`] 条的部分同样跳过（[`RuleError::TooMany`]）。
    pub fn from_settings(settings: &GuardSettings) -> (Self, Vec<InvalidRule>) {
        let mut rules = Vec::with_capacity(BUILTINS.len() + settings.custom_rules.len());
        for (b, regex) in BUILTINS.iter().zip(BUILTIN_REGEXES.iter()) {
            if settings.disabled_rules.iter().any(|id| id == b.rule.id()) {
                continue;
            }
            rules.push(Compiled {
                rule: b.rule.clone(),
                regex: regex.clone(),
                keywords: b.keywords,
                fold: b.rule.pattern().starts_with("(?i)"),
            });
        }
        let mut invalid = Vec::new();
        for (index, pattern) in settings.custom_rules.iter().enumerate() {
            if pattern.trim().is_empty() {
                continue;
            }
            let compiled = if index >= MAX_CUSTOM_RULES {
                Err(RuleError::TooMany)
            } else {
                compile_custom(pattern)
            };
            match compiled {
                Ok(regex) => rules.push(Compiled {
                    rule: Rule {
                        id: Cow::Owned(format!("custom-{}", index + 1)),
                        pattern: Cow::Owned(pattern.clone()),
                        text: None,
                    },
                    regex,
                    keywords: &[],
                    fold: false,
                }),
                Err(error) => invalid.push(InvalidRule {
                    index,
                    pattern: pattern.clone(),
                    error,
                }),
            }
        }
        (Self { rules }, invalid)
    }

    /// 返回命令（可以是多行脚本）命中的第一条规则，没有命中时为 `None`。
    ///
    /// 含 case 语句（`esac`）时，把与命令同一行的分支模式（`stop) reboot ;;`）换成 `;` 再检查一遍，
    /// 其后的命令由此按命令开头检查；先检查原文，自定义规则照常对原文生效。
    pub fn check(&self, command: &str) -> Option<&Rule> {
        if let Some(rule) = self.first_match(command) {
            return Some(rule);
        }
        if command.contains("esac") {
            let lines = case_lines(command);
            if !lines.is_empty() {
                return self.first_match(&lines);
            }
        }
        None
    }

    fn first_match(&self, command: &str) -> Option<&Rule> {
        let mut lower = None;
        self.rules
            .iter()
            .find(|c| c.may_match(command, &mut lower) && c.regex.is_match(command.as_bytes()))
            .map(|c| &c.rule)
    }
}

/// case 分支模式中的一个词：不含空白、括号、`;` `&` `|` 与引号的字符，以及成对的引号（不跨行）。
/// `\b` 与空白按 ASCII（与 RE2 相同）。
macro_rules! case_word {
    () => {
        r#"(?:[^\t\n\f\r ()|;&'"]|"[^"\n]*"|'[^'\n]*')+"#
    };
}

/// case 语句中分支的模式（行首、`;;` `;&` `;;&` 之后，或 `case … in` 之后的 `pat)` 与 `a|b)`）。
/// 只在含 `esac` 的命令中把它换成 `;` 再检查一遍：与模式同一行的命令（`stop) reboot ;;`）由此按命令开头检查。
/// 只认由词与 `|` 组成的模式：词不含空白、括号，引号中的部分可以有（`"stop now"`、`'a|b'`），`$(date) reboot`、
/// `echo "1) reboot"` 这类不受影响。
static CASE_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?m)(?:^|;;&?|;&|(?-u:\b)in)[ \t]*\(?",
        case_word!(),
        r"(?:[ \t]*\|[ \t]*",
        case_word!(),
        r")*[ \t]*\)",
    ))
    .expect("case 分支模式的正则")
});

/// 返回含 case 分支模式的各行，其中的模式换成 `;`。只有这些行多出了命令开头，其余各行检查原文时已经检查过，
/// 不必再对整个脚本匹配一遍。
fn case_lines(command: &str) -> String {
    let mut out = String::new();
    let mut line_end = 0;
    for m in CASE_PATTERN.find_iter(command) {
        // 模式不跨行；同一行的其他模式随这一行一起处理
        if m.start() < line_end {
            continue;
        }
        let begin = command[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        line_end = command[m.end()..]
            .find('\n')
            .map_or(command.len(), |i| m.end() + i);
        out.push_str(&CASE_PATTERN.replace_all(&command[begin..line_end], NoExpand(";")));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn settings(disabled: &[&str], custom: &[&str]) -> GuardSettings {
        GuardSettings {
            disabled_rules: disabled.iter().map(|s| s.to_string()).collect(),
            custom_rules: custom.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn hit(guard: &Guard, command: &str) -> Option<String> {
        guard.check(command).map(|r| r.id().to_string())
    }

    /// 逐条检查：应命中的命中了指定的规则，不应命中的都没有命中。
    fn assert_rules(guard: &Guard, blocked: &[(&str, &str)], allowed: &[&str]) {
        let mut failures = Vec::new();
        for (command, want) in blocked {
            let got = hit(guard, command);
            if got.as_deref() != Some(want) {
                failures.push(format!("{command:?} 应命中 {want}，实际 {got:?}"));
            }
        }
        for command in allowed {
            if let Some(got) = hit(guard, command) {
                failures.push(format!("{command:?} 不应被拦截，命中 {got}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn builtin_rules_compile() {
        assert_eq!(BUILTIN_REGEXES.len(), BUILTINS.len());
        let mut ids: Vec<_> = builtin_rules().map(Rule::id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), BUILTINS.len(), "内置规则的 ID 不能重复");
        assert!(builtin_rules().all(|r| r.is_builtin()
            && !r.description_en().is_empty()
            && !r.description_zh_tw().is_empty()
            && !r.description_ja().is_empty()
            && !r.description_ko().is_empty()));
    }

    #[test]
    fn builtin_rules_match() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("rm -rf /", "rm-root"),
                ("sudo rm -rf / ", "rm-root"),
                ("rm -fr /*", "rm-root"),
                ("cd /tmp && rm -rf ~", "rm-root"),
                ("rm -r -f $HOME", "rm-root"),
                ("rm --recursive --force /etc", "rm-root"),
                ("rm -Rf /usr/", "rm-root"),
                ("rm -rf --no-preserve-root /x", "rm-no-preserve-root"),
                ("mkfs.ext4 /dev/sdb1", "mkfs"),
                ("sudo wipefs -a /dev/sdb", "wipe-disk"),
                ("dd if=/dev/zero of=/dev/sda bs=1M", "dd-device"),
                ("cat x > /dev/nvme0n1", "write-device"),
                ("shutdown -h now", "power"),
                ("sudo reboot", "power"),
                ("echo hi; init 0", "power"),
                ("systemctl reboot", "power"),
                (":(){ :|:& };:", "fork-bomb"),
                ("chmod -R 777 /", "chmod-root"),
                ("sudo chown -R nobody /", "chmod-root"),
                ("kill -9 -1", "kill-all"),
                ("iptables -F", "firewall-flush"),
                ("nft flush ruleset", "firewall-flush"),
                ("crontab -r", "crontab-remove"),
                ("echo x > /etc/passwd", "overwrite-auth-files"),
                (r#"mysql -e "drop database prod""#, "sql-drop"),
                ("psql -c 'DROP SCHEMA public CASCADE'", "sql-drop"),
            ],
            // 日常命令不应被拦截
            &[
                "ls -la /",
                "rm -rf /tmp/build",
                "rm -rf ./node_modules",
                "rm -f /var/log/app.log",
                "df -h",
                "systemctl status nginx",
                "echo reboot later",
                "cat /etc/passwd",
                "grep shutdown /var/log/syslog",
                "dd if=/dev/sda of=/tmp/mbr.bin bs=512 count=1",
                "chmod -R 755 /srv/app",
                "kill -9 1234",
                "iptables -L",
                "crontab -l",
                "docker logs --tail 100 web",
                "git rm -r --cached .",
            ],
        );
    }

    /// 多行脚本（--stdin、AI 生成的脚本、同步输入的粘贴）与复合命令：每一行、每个 shell 关键字之后都是命令开头。
    #[test]
    fn multiline_and_compound() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("echo ok\nreboot", "power"),
                ("echo ok\r\nreboot", "power"),
                ("echo ok\rreboot", "power"),
                ("cd /tmp\nrm -rf /", "rm-root"),
                ("set -e\n  mkfs.ext4 /dev/sdb1\n", "mkfs"),
                ("if true; then reboot; fi", "power"),
                ("if reboot; then echo ok; fi", "power"),
                ("if [ -f x ]; then :; else kill -9 -1; fi", "kill-all"),
                ("if a; then b; elif shutdown -h now; then c", "power"),
                ("while true; do reboot; done", "power"),
                ("until false; do crontab -r; done", "crontab-remove"),
                ("for d in a b; do\n  wipefs -a /dev/sdb\ndone", "wipe-disk"),
                ("{ reboot; }", "power"),
                ("true && { poweroff; }", "power"),
                ("! reboot", "power"),
                ("cat <<'EOF' | bash\nrm -rf /\nEOF", "rm-root"),
                ("echo a\nchmod -R 777 /\necho b", "chmod-root"),
                ("echo a\niptables -F", "firewall-flush"),
            ],
            &[
                r#"echo "reboot""#,
                "echo ok\necho reboot later",
                "echo then reboot",
                "git commit -m 'do reboot later'",
                "ls\n# reboot later",
                "awk '{ print $1 }' /etc/hosts",
                r#"echo "${HOME}""#,
                "uptime\nsystemctl status nginx\ndf -h",
                "if [ -d /tmp/build ]; then rm -rf /tmp/build; fi",
                "for f in *.log; do\n  gzip \"$f\"\ndone",
            ],
        );
    }

    /// `!` 与 `{` 只在命令开头、且后面有空白时才是 shell 的关键字；timeout 前缀。
    #[test]
    fn bang_brace_and_timeout() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("! reboot", "power"),
                ("if ! reboot; then echo x; fi", "power"),
                ("true && ! halt", "power"),
                ("timeout 5 reboot", "power"),
                ("sudo timeout -s KILL 10s reboot", "power"),
                ("timeout --signal=KILL 5m shutdown -h now", "power"),
                ("timeout -k 5 10 poweroff", "power"),
                ("timeout 0.5 rm -rf /", "rm-root"),
                // case 分支中命令另起一行时按行首检查
                ("case \"$1\" in\n  stop)\n    reboot\n    ;;\nesac", "power"),
                // 已知取舍：按行检查，多行引号字符串与 heredoc 正文中位于行首的词同样算作命令开头（不做 shell 解析）
                ("git commit -m \"fix\nreboot handling\"", "power"),
                ("cat > notes <<EOF\nreboot after upgrade\nEOF", "power"),
            ],
            &[
                "echo 'Done! reboot the server'",
                "printf x # ! halt",
                "[ ! -f x ] && rm -rf /tmp/x",
                "echo ${!name}",
                r#"echo "${!prefix@}" reboot"#,
                "awk '/x/{halt=1} END{print halt}' f",
                "echo { reboot",
                "echo $(date) reboot",
                r#"echo "(see notes) reboot later""#,
                "echo timeout 5",
                "ps aux | timeout 5 grep reboot",
                r#"echo "1) reboot 2) halt""#,
                "case $1 in start) systemctl start nginx;; esac",
            ],
        );
    }

    /// 命令名带路径、反斜杠，前面有变量赋值、带参数的 sudo 选项，经 env / nice / command / sh -c 执行。
    #[test]
    fn prefixes_and_paths() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("/bin/rm -rf /", "rm-root"),
                (r"\rm -rf /", "rm-root"),
                ("command rm -rf /", "rm-root"),
                ("sudo -u root rm -rf /", "rm-root"),
                ("sudo -H -u root /usr/bin/rm -rf /etc", "rm-root"),
                ("sudo --user=root reboot", "power"),
                ("doas -u root halt", "power"),
                ("bash -c 'rm -rf /'", "rm-root"),
                (r#"sh -c "reboot""#, "power"),
                ("zsh -lc poweroff", "power"),
                ("env LANG=C reboot", "power"),
                ("LANG=C FOO=1 reboot", "power"),
                ("nice reboot", "power"),
                ("nice -n 10 shutdown -h now", "power"),
                ("ionice -c 3 rm -rf ~", "rm-root"),
                ("./sbin/reboot", "power"),
                ("/sbin/shutdown -r now", "power"),
                (r#"rm -rf "/""#, "rm-root"),
                (r#"rm -rf "$HOME/""#, "rm-root"),
                (r#"sudo chmod -R 777 "/""#, "chmod-root"),
            ],
            &[
                "command -v reboot",
                "type reboot",
                "ls /sbin/reboot",
                "cat /usr/lib/systemd/systemd-shutdown",
                "bash script.sh",
                "sh -c 'echo reboot'",
                r#"rm -rf "$HOME/project""#,
                "sudo -u postgres psql",
                "env | grep HOME",
                "nice -n 10 make",
                "FOO=1 make reboot-image",
                "ssh web -c aes128-ctr uptime",
            ],
        );
    }

    /// case 分支中与模式同一行的命令同样检查；不是 case 语句的 `)` 不算命令开头。
    #[test]
    fn case_branches() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("case $1 in\n  stop) reboot ;;\nesac", "power"),
                ("case $1 in stop) reboot;; esac", "power"),
                (
                    "case $1 in start) echo hi;; stop|halt) poweroff;; esac",
                    "power",
                ),
                (
                    "case \"$x\" in\n  (wipe) rm -rf / ;;\n  *) echo no ;;\nesac",
                    "rm-root",
                ),
                (
                    "case $a in\n  a) echo a ;;\n  *) sudo crontab -r ;;\nesac",
                    "crontab-remove",
                ),
                (
                    "case $a in\n  a) echo a ;&\n  b) timeout 5 halt ;;\nesac",
                    "power",
                ),
                (
                    "case $a in x) true ;; esac; case $b in y) mkfs.ext4 /dev/sdb1;; esac",
                    "mkfs",
                ),
                // 模式中带空格的引号
                ("case \"$1\" in\n  \"stop now\") reboot ;;\nesac", "power"),
                ("case $1 in 'a b'|\"c d\") halt ;; esac", "power"),
                (
                    "case $1 in\n  start) echo ;;\n  pre\"x y\") poweroff ;;\nesac",
                    "power",
                ),
            ],
            &[
                "case $1 in start) systemctl start nginx;; esac",
                "case $1 in\n  status) echo \"reboot pending\" ;;\nesac",
                "echo $(date) reboot\necho esac",
                "echo \"1) reboot 2) halt\"; echo esac",
                "(cd /tmp; ls) reboot-notes.txt; echo esac",
                "case $1 in\n  \"stop now\") echo \"reboot later\" ;;\nesac",
            ],
        );
    }

    /// 关键字只用于跳过不可能命中的规则：每条内置规则都要有关键字，且关键字出现在规则的正则中。
    #[test]
    fn builtin_keywords() {
        for b in BUILTINS {
            assert!(
                !b.keywords.is_empty(),
                "{} 没有关键字（新规则请补上）",
                b.rule.id()
            );
            let pattern = b.rule.pattern().replace('\\', "").to_lowercase();
            for k in b.keywords {
                assert!(
                    pattern.contains(k),
                    "{} 的关键字 {k:?} 不在正则中",
                    b.rule.id()
                );
            }
        }
        // 不区分大小写的规则按小写比较关键字
        let guard = Guard::default();
        assert_eq!(
            hit(&guard, r#"mysql -e "DROP DATABASE prod""#).as_deref(),
            Some("sql-drop")
        );
    }

    /// 检查关键字预筛的语料：各条内置规则的正例、反例（含大小写变化、前缀、多行脚本与 case 语句）。
    const PREFILTER_CORPUS: &[&str] = &[
        // rm-root、rm-no-preserve-root
        "rm -rf /",
        "sudo rm -rf / ",
        "rm -fr /*",
        "rm -r -f $HOME",
        "rm -rf ${HOME}/",
        "rm --recursive /etc",
        "RM -RF /",
        "rm -rf /tmp/build",
        "git rm -r --cached .",
        "rm -rf --no-preserve-root /x",
        "x --NO-PRESERVE-ROOT",
        // mkfs、wipe-disk、dd-device、write-device、shred-device
        "mkfs.ext4 /dev/sdb1",
        "sudo MKFS /dev/sdb",
        "mkfs -t xfs /dev/sdc",
        "wipefs -a /dev/sdb",
        "sfdisk /dev/sda < t",
        "sgdisk -Z /dev/nvme0n1",
        "blkdiscard /dev/sdb",
        "dd if=/dev/zero of=/dev/sda bs=1M",
        "dd of=/dev/mapper/x if=y",
        "dd if=/dev/sda of=/tmp/mbr.bin",
        "cat x > /dev/nvme0n1",
        "echo >/dev/sda1",
        "cat x > /dev/null",
        "shred -u /dev/sdb",
        "shred secret.txt",
        // power
        "shutdown -h now",
        "sudo reboot",
        "echo hi; init 0",
        "init 6",
        "systemctl reboot",
        "systemctl --force kexec",
        "systemctl -i poweroff",
        "nohup halt",
        "REBOOT",
        "echo reboot later",
        "grep shutdown /var/log/syslog",
        "systemctl status nginx",
        "initctl list",
        // fork-bomb、chmod-root、kill-all
        ":(){ :|:& };:",
        ": ( ) { : | : & } ; :",
        "chmod -R 777 /",
        "sudo chown -R nobody /",
        "chgrp -R x /",
        "chmod -R 755 /srv/app",
        "kill -9 -1",
        "kill -1",
        "killall5 -9",
        "kill -9 1234",
        "pkill -f x",
        // firewall-flush、crontab-remove、overwrite-auth-files、sql-drop
        "iptables -F",
        "iptables --flush",
        "iptables -t nat -F",
        "nft flush ruleset",
        "iptables -L",
        "crontab -r",
        "crontab -u root -r",
        "crontab -l",
        "echo x > /etc/passwd",
        ">/etc/shadow",
        "cat /etc/passwd",
        r#"mysql -e "drop database prod""#,
        "psql -c 'DROP SCHEMA public CASCADE'",
        "Drop   Database x",
        "drop table t",
        // 多行脚本、复合命令、case 语句
        "echo ok\nreboot",
        "echo ok\r\nreboot",
        "cd /tmp\nrm -rf /",
        "if true; then reboot; fi",
        "while true; do reboot; done",
        "for d in a b; do\n  wipefs -a /dev/sdb\ndone",
        "{ reboot; }",
        "! reboot",
        "cat <<'EOF' | bash\nrm -rf /\nEOF",
        "timeout 5 reboot",
        "sudo timeout -s KILL 10s reboot",
        "timeout 0.5 rm -rf /",
        "case $1 in\n  stop) reboot ;;\nesac",
        "case $1 in start) echo hi;; stop|halt) poweroff;; esac",
        "case \"$x\" in\n  (wipe) rm -rf / ;;\n  *) echo no ;;\nesac",
        "case $a in\n  *) sudo crontab -r ;;\nesac",
        "case \"$1\" in\n  \"stop now\") reboot ;;\nesac",
        "case $1 in 'a b') kill -9 -1 ;; esac",
        "case $a in x) true ;; esac; case $b in y) mkfs.ext4 /dev/sdb1;; esac",
        "echo $(date) reboot\necho esac",
        "echo \"1) reboot 2) halt\"; echo esac",
        "ls\n# reboot later",
        "awk '{ print $1 }' /etc/hosts",
        "uptime\nsystemctl status nginx\ndf -h",
        "set -e\nexport A=1\nmysql -e 'DROP DATABASE x'\n",
    ];

    /// 关键字预筛不能改变结果：在语料上，每条内置规则的正则命中时预筛都不排除它；
    /// 开启预筛与不预筛（每条规则都运行正则）的结果一致（含 case 语句中模式换成 `;` 之后的各行）。
    /// 只检查关键字出现在正则文本中（[`builtin_keywords`]）不能保证每个分支都被关键字覆盖，
    /// 改规则或关键字时由这里发现漏报。
    #[test]
    fn keyword_prefilter_consistent() {
        let guard = Guard::default();
        let mut raw = Guard::default();
        for c in &mut raw.rules {
            c.keywords = &[];
        }
        let mut hits = std::collections::HashMap::<&str, usize>::new();
        let mut failures = Vec::new();
        for command in PREFILTER_CORPUS {
            for text in [command.to_string(), case_lines(command)] {
                let mut lower = None;
                for c in &guard.rules {
                    if c.regex.is_match(text.as_bytes()) {
                        *hits.entry(c.rule.id()).or_default() += 1;
                        if !c.may_match(&text, &mut lower) {
                            failures.push(format!(
                                "{} 的正则命中 {text:?}，关键字预筛却排除了它",
                                c.rule.id()
                            ));
                        }
                    }
                }
            }
            let (a, b) = (hit(&guard, command), hit(&raw, command));
            if a != b {
                failures.push(format!("{command:?}：预筛 {a:?}，不预筛 {b:?}"));
            }
        }
        // 语料要覆盖每条内置规则的正例
        for r in builtin_rules() {
            if !hits.contains_key(r.id()) {
                failures.push(format!("语料中没有 {} 的正例", r.id()));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// 1MB 的脚本（含全是换行的）：不含任何规则关键字时很快。
    #[test]
    fn large_input_is_fast() {
        let guard = Guard::default();
        let script = "echo hello world\n".repeat((1 << 20) / 17);
        for input in ["\n".repeat(1 << 20), script.clone()] {
            let began = Instant::now();
            assert_eq!(hit(&guard, &input), None);
            let elapsed = began.elapsed();
            assert!(
                elapsed < Duration::from_secs(1),
                "1MB 的输入耗时 {elapsed:?}"
            );
        }
        assert_eq!(
            hit(&guard, &format!("{script}reboot\n")).as_deref(),
            Some("power"),
            "末尾的 reboot 应命中"
        );
    }

    #[test]
    fn disable_and_custom() {
        let (guard, invalid) = Guard::from_settings(&settings(
            &["power"],
            &["", r"(?i)\bdocker\s+system\s+prune\b"],
        ));
        assert!(invalid.is_empty());
        assert_eq!(hit(&guard, "reboot"), None, "关闭的内置规则不应生效");
        let rule = guard
            .check("Docker System Prune -af")
            .expect("自定义规则应命中");
        assert_eq!(rule.id(), "custom-2");
        assert!(!rule.is_builtin());
        assert_eq!(rule.description(), r"(?i)\bdocker\s+system\s+prune\b");
        assert_eq!(rule.description_en(), rule.pattern());
        assert_eq!(rule.description_zh_tw(), rule.pattern());
        assert_eq!(rule.description_ja(), rule.pattern());
        assert_eq!(rule.description_ko(), rule.pattern());

        for bad in ["(", ".*", "x?"] {
            assert!(
                validate_custom_rule(bad).is_err(),
                "不合法或匹配一切的规则应被拒绝：{bad:?}"
            );
        }
        assert!(is_builtin("rm-root"));
        assert!(!is_builtin("custom-1"));

        let rule = Guard::default().check("sudo reboot").cloned();
        let rule = rule.expect("内置规则应命中");
        assert!(rule.is_builtin());
        assert_eq!(
            rule.description(),
            "关机或重启（shutdown、reboot、halt、poweroff、init 0/6）"
        );
        assert_eq!(
            rule.description_zh_tw(),
            "關機或重新啟動（shutdown、reboot、halt、poweroff、init 0/6）"
        );
    }

    /// 不合法的自定义规则不影响其余规则，逐条报告出来。
    #[test]
    fn invalid_custom_rules_are_reported() {
        let long = "a".repeat(MAX_PATTERN_LENGTH + 1);
        let (guard, invalid) = Guard::from_settings(&settings(
            &["no-such-rule"],
            &[
                "(",
                "  ",
                ".*",
                &long,
                r"(?:\w{100}){100}",
                r"\bpurge\b",
                r"\Qa.b\E",
            ],
        ));
        let errors: Vec<_> = invalid.iter().map(|i| (i.index, i.error.clone())).collect();
        assert!(matches!(errors[0], (0, RuleError::Syntax(_))));
        assert_eq!(errors[1], (2, RuleError::MatchesEverything));
        assert_eq!(errors[2], (3, RuleError::TooLong));
        assert_eq!(errors[3], (4, RuleError::TooComplex));
        assert!(matches!(errors[4], (6, RuleError::Syntax(_))));
        assert_eq!(errors.len(), 5);
        assert_eq!(invalid[1].pattern, ".*");
        // 合法的照常生效，不认识的关闭 ID 忽略
        assert_eq!(hit(&guard, "apt purge x").as_deref(), Some("custom-6"));
        assert_eq!(hit(&guard, "reboot").as_deref(), Some("power"));

        let many: Vec<String> = (0..MAX_CUSTOM_RULES + 2)
            .map(|i| format!("^cmd{i}$"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let (guard, invalid) = Guard::from_settings(&settings(&[], &many));
        assert_eq!(invalid.len(), 2);
        assert!(invalid.iter().all(|i| i.error == RuleError::TooMany));
        assert_eq!(invalid[0].index, MAX_CUSTOM_RULES);
        assert_eq!(hit(&guard, "cmd99").as_deref(), Some("custom-100"));
        assert_eq!(hit(&guard, "cmd100"), None);
    }

    /// 内置规则按 ASCII 理解 `\b` 与空白（与 RE2 相同）：紧挨着中文的词照常有词的边界，
    /// 全角空格、不换行空格不算分隔（shell 同样不把它们当作分隔）。
    #[test]
    fn builtin_rules_are_ascii() {
        let guard = Guard::default();
        assert_rules(
            &guard,
            &[
                ("好sudo reboot", "power"),
                ("echo 完成;reboot", "power"),
                ("sudo reboot # 重启", "power"),
            ],
            &[
                "rm\u{3000}-rf /",
                "rm\u{a0}-rf /",
                "echo 完成\u{3000}reboot",
            ],
        );
        // 非 ASCII 的内容不 panic
        assert_eq!(hit(&guard, "case 中 in\n  停) 重启 ;;\nesac 😀"), None);
    }
}
