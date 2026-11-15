//! YAML 的解析（Tabby 的 config.yaml、更新的版本描述文件），用 serde-saphyr。
//!
//! 选项尽量与原来用的 serde_yaml_ng（YAML 1.2）一致：
//! - yes、no、on、off 等 YAML 1.1 的写法是文本，不当作布尔值；
//! - 重复的键报错；合并键（`<<`）展开；锚点与别名照常展开（有次数上限，防止“别名炸弹”）；
//! - 只读一个文档，有多个文档时报错；
//! - 结构不定时（解析为 JSON 值）NaN、无穷大保留为文本（`.nan`、`.inf`），不报错；
//! - 错误信息只带位置，不附原文片段（配置文件中可能有不该显示、记录的内容）。
//!
//! 与 serde_yaml_ng 不同的地方见各处的 `yaml_edge_cases` 测试，例如结构不定时有多余前导零的数字（`007`）解析为小数。

use serde::de::DeserializeOwned;

fn options() -> serde_saphyr::Options {
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    options.reject_non_finite_typeless_float = false;
    options.with_snippet = false;
    options
}

/// 解析为指定的类型。
pub fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, serde_saphyr::Error> {
    serde_saphyr::from_str_with_options(text, options())
}

/// 解析结构不定的内容（例如别的程序的配置文件）。数字、布尔值作键时转为字符串，null 与集合作键时报错；
/// 带标签的取其中的值。
pub fn to_json(text: &str) -> Result<serde_json::Value, serde_saphyr::Error> {
    from_str(text)
}
