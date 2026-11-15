//! 更新签名的公钥（minisign 格式）：在用的一把与离线备用的一把。私钥都不放进仓库：在用的那把加密的密钥文件
//! 与口令存在 GitHub Actions 的 `release` 环境的 Secrets 中（用时要维护者批准）；离线备用的那把只在发布者的
//! 离线介质中，不进 CI。
//!
//! 为空时应用不接受任何更新（校验一律失败），发布脚本 `scripts/release.mjs` 也拒绝签名。
//!
//! 轮换：在用的密钥丢失或泄露时，用 `node scripts/release.mjs newkey --out <仓库外的目录>` 生成一把新的备用密钥，
//! 把这里改成"原备用（改为在用）+ 新备用"，在本机用原备用密钥签发这个版本（`release.mjs fetch`、`sign --key`、
//! `publish`），再把它的密钥文件与口令换进 `release` 环境的 Secrets，之后都用它签名。
//! 两把都丢失时已安装的版本无法再自动更新，只能让用户手动下载新版。

/// 受信任的更新公钥。发布脚本按这里的字符串核对签名用的密钥，并确认发布构建的程序中含有它。
pub const UPDATE_PUBLIC_KEYS: &[&str] = &[
    // 在用：发布时用它签名（密钥 ID 70538B50651A6352，2026-10-15 生成）
    "RWRSYxplUItTcPz4ZAEBJVcK0n3Sy1xPjhO3CNJIVFxbazT7cXsPacBG",
    // 离线备用：只在轮换密钥时使用（密钥 ID 1274237273808A74，2026-10-15 生成）
    "RWR0ioBzciN0EjIEaOwb2jw2Xujxz3oxGYgF8nh1HrPVLkgkqH9dsmog",
];
