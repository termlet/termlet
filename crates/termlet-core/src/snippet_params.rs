//! 快捷命令的参数模板：内容中的 `{{名称}}` 或 `{{名称:默认值}}` 在发送前由用户填写（同名的参数只填一次）。
//!
//! 名称由文字、数字、下划线组成，中间可以用空格、点、连字符隔开（`{{目标 主机}}`、`{{db.host}}`）。
//! 其余的 `{{…}}` 原样保留：docker、kubectl 的 Go 模板（`{{.Names}}`、`{{index .Labels "a:b"}}`、
//! `{{range .items}}…{{end}}`）与 Jinja 的 `{{ x | upper }}` 等不是参数。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// 一个参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetParam {
    pub name: String,
    /// 第一次出现时写的默认值，没有时为空
    pub default_value: String,
}

static PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{([^{}:]+?)(?::([^{}]*))?\}\}").unwrap());
static NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{M}\p{N}_]+(?:[ .-][\p{L}\p{M}\p{N}_]+)*$").unwrap());
/// 内容中有 Go 模板的块动作（`{{range …}}`、`{{if …}}` 等）时，`{{end}}`、`{{else}}` 等是模板的一部分
static GO_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{-?\s*(?:range|if|with|block|define)\s").unwrap());
const GO_KEYWORDS: [&str; 4] = ["end", "else", "break", "continue"];

/// `{{…}}` 中冒号前的部分是参数名时返回名称（去掉首尾空白），否则返回 `None`（原样保留）。
fn param_name(raw: &str, go_template: bool) -> Option<&str> {
    let name = raw.trim();
    if !NAME.is_match(name) || (go_template && GO_KEYWORDS.contains(&name)) {
        return None;
    }
    Some(name)
}

/// 内容中的参数，按第一次出现的顺序。
pub fn snippet_params(content: &str) -> Vec<SnippetParam> {
    let go_template = GO_BLOCK.is_match(content);
    let mut params: Vec<SnippetParam> = Vec::new();
    for caps in PATTERN.captures_iter(content) {
        let Some(name) = param_name(&caps[1], go_template) else {
            continue;
        };
        if params.iter().any(|p| p.name == name) {
            continue;
        }
        params.push(SnippetParam {
            name: name.to_string(),
            default_value: caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
        });
    }
    params
}

/// 用填写的值替换参数；没有填写的用默认值。
pub fn fill_snippet(content: &str, values: &HashMap<String, String>) -> String {
    let go_template = GO_BLOCK.is_match(content);
    PATTERN
        .replace_all(content, |caps: &regex::Captures<'_>| {
            match param_name(&caps[1], go_template) {
                Some(name) => values.get(name).cloned().unwrap_or_else(|| {
                    caps.get(2)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default()
                }),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(content: &str) -> Vec<String> {
        snippet_params(content)
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    fn fill(content: &str, values: &[(&str, &str)]) -> String {
        let values = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        fill_snippet(content, &values)
    }

    #[test]
    fn params_in_first_seen_order_with_first_default() {
        assert_eq!(
            snippet_params("docker logs -f {{容器}} --tail {{行数:200}} | grep {{容器}}"),
            [
                SnippetParam {
                    name: "容器".into(),
                    default_value: String::new()
                },
                SnippetParam {
                    name: "行数".into(),
                    default_value: "200".into()
                },
            ]
        );
        assert_eq!(
            snippet_params("echo {{ name : x }}"),
            [SnippetParam {
                name: "name".into(),
                default_value: " x ".into()
            }]
        );
        assert!(snippet_params("echo {{}} {{ }} $HOME {x}").is_empty());
    }

    #[test]
    fn fill_uses_values_then_defaults() {
        let content = "ssh {{用户:root}}@{{主机}} -p {{端口:22}}";
        assert_eq!(
            fill(content, &[("主机", "10.0.0.1"), ("端口", "2222")]),
            "ssh root@10.0.0.1 -p 2222"
        );
        assert_eq!(fill("{{a}}-{{a}}", &[("a", "x")]), "x-x");
        assert_eq!(fill("{{a}}", &[]), "");
        assert_eq!(fill("awk \"{print $1}\"", &[]), "awk \"{print $1}\"");
    }

    #[test]
    fn names_allow_inner_separators() {
        assert_eq!(
            names("{{image-tag:latest}} {{主机_1}} {{2}} {{目标 主机}} {{db.host}}"),
            ["image-tag", "主机_1", "2", "目标 主机", "db.host"]
        );
        assert!(
            names("{{-a}} {{a-}} {{a  b}} {{a .b}} {{.a}} {{$x}} {{\"a\"}} {{a|b}}").is_empty()
        );
    }

    #[test]
    fn go_templates_are_kept() {
        let ps = "docker ps --format '{{.Names}}\\t{{ .Status }}'";
        assert!(snippet_params(ps).is_empty());
        assert_eq!(fill(ps, &[]), ps);
        let labels =
            "docker inspect -f '{{index .Config.Labels \"com.docker.compose:project\"}}' {{容器}}";
        assert_eq!(names(labels), ["容器"]);
        assert_eq!(
            fill(labels, &[("容器", "web")]),
            "docker inspect -f '{{index .Config.Labels \"com.docker.compose:project\"}}' web"
        );
        assert!(snippet_params("docker inspect -f '{{json .State}}' x").is_empty());
        let pods = "kubectl get pods -n {{命名空间:default}} -o go-template='{{range .items}}{{.metadata.name}}{{\"\\n\"}}{{end}}'";
        assert_eq!(names(pods), ["命名空间"]);
        assert_eq!(
            fill(pods, &[]),
            "kubectl get pods -n default -o go-template='{{range .items}}{{.metadata.name}}{{\"\\n\"}}{{end}}'"
        );
        let if_else = "kubectl get no -o go-template='{{- if .x}}a{{else}}b{{ end -}}'";
        assert!(snippet_params(if_else).is_empty());
        assert_eq!(fill(if_else, &[]), if_else);
        // 没有块动作时 end 仍可以作为参数名
        assert_eq!(names("seq {{start:1}} {{end}}"), ["start", "end"]);
    }

    #[test]
    fn jinja_expressions_are_kept_but_plain_names_are_params() {
        let play =
            "ansible all -m debug -a 'msg={{ item.name | upper }} {{ \"a:b\" }} {{ x + 1 }}'";
        assert!(snippet_params(play).is_empty());
        assert_eq!(fill(play, &[]), play);
        assert_eq!(
            names("echo {{ inventory_hostname }}"),
            ["inventory_hostname"]
        );
    }
}
