//! 按远程路径识别文件的语言：特殊文件名、扩展名、nginx 目录、shebang。
//! HTML 与 XML 分开识别，CSS 单独识别（这两种有语法高亮）。

use std::sync::LazyLock;

use regex::Regex;

/// 识别出的语言；识别不了的文件按纯文本编辑。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Json,
    Yaml,
    Markdown,
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
    Python,
    Shell,
    Nginx,
    /// INI 风格的配置：key=value、\[section\]、# 注释（含 systemd unit、yum 源）
    Properties,
    Toml,
    Xml,
    Html,
    Css,
    Dockerfile,
}

impl Language {
    /// 状态栏显示的名称（专有名词，不翻译）。
    pub fn label(self) -> &'static str {
        match self {
            Language::Json => "JSON",
            Language::Yaml => "YAML",
            Language::Markdown => "Markdown",
            Language::JavaScript => "JavaScript",
            Language::Jsx => "JSX",
            Language::TypeScript => "TypeScript",
            Language::Tsx => "TSX",
            Language::Python => "Python",
            Language::Shell => "Shell",
            Language::Nginx => "Nginx",
            Language::Properties => "INI",
            Language::Toml => "TOML",
            Language::Xml => "XML",
            Language::Html => "HTML",
            Language::Css => "CSS",
            Language::Dockerfile => "Dockerfile",
        }
    }

    /// 编辑器语法高亮用的语言名（gpui-component 的 tree-sitter 语法）；没有语法的为 `None`，按纯文本显示。
    pub fn grammar(self) -> Option<&'static str> {
        match self {
            Language::Json => Some("json"),
            Language::Yaml => Some("yaml"),
            Language::Markdown => Some("markdown"),
            // tree-sitter-javascript 本身支持 JSX
            Language::JavaScript | Language::Jsx => Some("javascript"),
            Language::TypeScript => Some("typescript"),
            Language::Tsx => Some("tsx"),
            Language::Python => Some("python"),
            Language::Shell => Some("bash"),
            Language::Toml => Some("toml"),
            Language::Html => Some("html"),
            Language::Css => Some("css"),
            Language::Nginx | Language::Properties | Language::Xml | Language::Dockerfile => None,
        }
    }
}

/// 扩展名（小写，不含点）对应的语言。
fn by_extension(ext: &str) -> Option<Language> {
    use Language::*;
    Some(match ext {
        "json" | "jsonc" | "json5" | "webmanifest" => Json,
        "yml" | "yaml" => Yaml,
        "md" | "markdown" | "mkd" => Markdown,
        "js" | "mjs" | "cjs" => JavaScript,
        "jsx" => Jsx,
        "ts" | "mts" | "cts" => TypeScript,
        "tsx" => Tsx,
        "py" | "pyw" | "pyi" => Python,
        "sh" | "bash" | "zsh" | "ksh" | "ash" => Shell,
        "ini" | "cfg" | "conf" | "cnf" | "properties" | "env" | "service" | "timer" | "socket"
        | "repo" => Properties,
        "toml" => Toml,
        "xml" | "xsd" | "xsl" | "xslt" | "svg" | "plist" => Xml,
        "html" | "htm" | "xhtml" => Html,
        "css" => Css,
        _ => return None,
    })
}

/// 没有扩展名或扩展名说明不了类型的常见文件（文件名小写）。
fn by_name(name: &str) -> Option<Language> {
    use Language::*;
    Some(match name {
        "dockerfile" | "containerfile" => Dockerfile,
        "nginx.conf" => Nginx,
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".profile" | ".zshrc"
        | ".zprofile" | ".zshenv" | ".kshrc" | ".envrc" => Shell,
        ".env" | ".editorconfig" | ".gitconfig" | ".npmrc" | ".pypirc" | "my.cnf" => Properties,
        "pipfile" => Toml,
        ".babelrc" | ".prettierrc" | ".eslintrc" => Json,
        _ => return None,
    })
}

/// shebang 中的解释器对应的语言。
fn by_interpreter(interpreter: &str) -> Option<Language> {
    match interpreter {
        "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash" => Some(Language::Shell),
        "node" | "nodejs" | "deno" | "bun" => Some(Language::JavaScript),
        _ => {
            let version = interpreter.strip_prefix("python")?;
            version
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.')
                .then_some(Language::Python)
        }
    }
}

/// `#!/usr/bin/env -S node --flag`、`#! /bin/sh -e` 这类 shebang 中的解释器名。
static SHEBANG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^#!\s*(?:\S*/)?(?:env\s+(?:-\S+\s+)*)?([A-Za-z0-9_.-]+)").expect("shebang 的正则")
});

/// 远程路径的最后一段（根目录为 `/`）。
fn base_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("/")
}

/// 按远程路径识别语言；`first_line` 是文件的第一行，没有扩展名的脚本按 shebang 识别。识别不了时为 `None`（纯文本）。
pub fn language_for(path: &str, first_line: &str) -> Option<Language> {
    let name = base_name(path).to_lowercase();
    if let Some(language) = by_name(&name) {
        return Some(language);
    }
    if name.starts_with(".env.") {
        return Some(Language::Properties);
    }
    if name.starts_with("dockerfile.") || name.ends_with(".dockerfile") {
        return Some(Language::Dockerfile);
    }
    // 隐藏文件（.bashrc）开头的点不是扩展名的分隔符
    let ext = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => "",
    };
    // nginx 目录下的 .conf 与站点配置（sites-available/default 这类没有扩展名的文件）
    if path.contains("/nginx/") && (ext == "conf" || ext.is_empty()) {
        return Some(Language::Nginx);
    }
    if !ext.is_empty()
        && let Some(language) = by_extension(ext)
    {
        return Some(language);
    }
    let interpreter = SHEBANG.captures(first_line)?.get(1)?.as_str();
    by_interpreter(interpreter)
}

/// 文件的第一行（识别 shebang 用），只看开头的 200 个字符。
pub fn first_line_of(text: &str) -> &str {
    let end = text.char_indices().nth(200).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    match head.find('\n') {
        Some(newline) => {
            let line = &head[..newline];
            line.strip_suffix('\r').unwrap_or(line)
        }
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Language::*;

    fn detect(path: &str) -> Option<Language> {
        language_for(path, "")
    }

    #[test]
    fn by_extension_ignoring_case() {
        assert_eq!(detect("/srv/app/package.json"), Some(Json));
        assert_eq!(detect("/srv/app/docker-compose.YML"), Some(Yaml));
        assert_eq!(detect("/srv/README.md"), Some(Markdown));
        assert_eq!(detect("/srv/a.mjs"), Some(JavaScript));
        assert_eq!(detect("/srv/a.jsx"), Some(Jsx));
        assert_eq!(detect("/srv/a.ts"), Some(TypeScript));
        assert_eq!(detect("/srv/a.tsx"), Some(Tsx));
        assert_eq!(detect("/srv/manage.py"), Some(Python));
        assert_eq!(detect("/opt/deploy.sh"), Some(Shell));
        assert_eq!(detect("/etc/php.ini"), Some(Properties));
        assert_eq!(detect("/etc/systemd/system/app.service"), Some(Properties));
        assert_eq!(detect("/srv/Cargo.toml"), Some(Toml));
        assert_eq!(detect("/srv/pom.xml"), Some(Xml));
        // HTML、CSS 单独识别（有语法高亮）
        assert_eq!(detect("/var/www/index.HTML"), Some(Html));
        assert_eq!(detect("/var/www/site.css"), Some(Css));
    }

    #[test]
    fn by_special_names_and_hidden_files() {
        assert_eq!(detect("/home/u/.bashrc"), Some(Shell));
        assert_eq!(detect("/home/u/.profile"), Some(Shell));
        assert_eq!(detect("/srv/app/.env"), Some(Properties));
        assert_eq!(detect("/srv/app/.env.production"), Some(Properties));
        assert_eq!(detect("/srv/app/Dockerfile"), Some(Dockerfile));
        assert_eq!(detect("/srv/app/Dockerfile.dev"), Some(Dockerfile));
        assert_eq!(detect("/srv/app/api.dockerfile"), Some(Dockerfile));
        assert_eq!(detect("/srv/Pipfile"), Some(Toml));
        assert_eq!(detect("/srv/.prettierrc"), Some(Json));
    }

    #[test]
    fn nginx_directory_and_other_conf() {
        assert_eq!(detect("/etc/nginx/nginx.conf"), Some(Nginx));
        assert_eq!(detect("/etc/nginx/conf.d/site.conf"), Some(Nginx));
        assert_eq!(detect("/etc/nginx/sites-available/default"), Some(Nginx));
        assert_eq!(detect("/usr/local/nginx.conf"), Some(Nginx));
        assert_eq!(detect("/etc/redis/redis.conf"), Some(Properties));
        // nginx 目录下有别的扩展名的照常按扩展名
        assert_eq!(detect("/etc/nginx/mime.types.json"), Some(Json));
    }

    #[test]
    fn scripts_without_extension_by_shebang() {
        let run = "/usr/local/bin/run";
        assert_eq!(language_for(run, "#!/bin/bash"), Some(Shell));
        assert_eq!(language_for(run, "#!/usr/bin/env python3"), Some(Python));
        assert_eq!(language_for(run, "#!/usr/bin/python3.11"), Some(Python));
        assert_eq!(
            language_for(run, "#!/usr/bin/env -S node --no-warnings"),
            Some(JavaScript)
        );
        assert_eq!(language_for(run, "#! /bin/sh -e"), Some(Shell));
        // 扩展名优先于 shebang
        assert_eq!(language_for("/srv/a.py", "#!/bin/sh"), Some(Python));
    }

    #[test]
    fn plain_text_when_unknown() {
        assert_eq!(detect("/var/log/syslog"), None);
        assert_eq!(detect("/srv/notes.txt"), None);
        assert_eq!(language_for("/srv/run", "#!/usr/bin/perl"), None);
        assert_eq!(language_for("/srv/run", "#!/usr/bin/pythonista"), None);
        assert_eq!(detect("/srv/.hidden"), None);
        assert_eq!(detect("/"), None);
    }

    #[test]
    fn first_line() {
        assert_eq!(first_line_of("#!/bin/sh\necho hi"), "#!/bin/sh");
        assert_eq!(first_line_of("#!/bin/sh\r\necho hi"), "#!/bin/sh");
        assert_eq!(first_line_of(""), "");
        assert_eq!(first_line_of("单行没有换行"), "单行没有换行");
        // 只看开头的 200 个字符（按字符数，不会切在多字节字符中间）
        let long = "中".repeat(300);
        assert_eq!(first_line_of(&long).chars().count(), 200);
    }

    /// 每个语法名都对应编译进来的 tree-sitter 语法（`Cargo.toml` 中 gpui-kit 的 `tree-sitter-*` 特性）。
    #[test]
    fn grammars_are_built_in() {
        use gpui_kit::component::highlighter::LanguageRegistry;
        let all = [
            Json, Yaml, Markdown, JavaScript, Jsx, TypeScript, Tsx, Python, Shell, Nginx,
            Properties, Toml, Xml, Html, Css, Dockerfile,
        ];
        let registry = LanguageRegistry::singleton();
        for language in all {
            if let Some(name) = language.grammar() {
                assert!(
                    registry.language(name).is_some_and(|c| c.has_grammar()),
                    "{name} 没有语法"
                );
            }
        }
        assert_eq!(Shell.grammar(), Some("bash"));
        assert_eq!(Jsx.grammar(), Some("javascript"));
        assert_eq!(Nginx.grammar(), None);
        assert_eq!(Properties.label(), "INI");
    }
}
