# sshkey 测试向量

这里的私钥全部是公开的测试密钥，只用于验证解析器与 PuTTY 的兼容性，不能用于任何真实环境。

## putty/

来源：PuTTY 0.85 官方源码包 `putty-0.85.tar.gz`（https://the.earth.li/~sgtatham/putty/latest/）
中的 `test/cryptsuite.py`，`testPPKLoadSave` 用例。PuTTY 用这些文件做自身 PPK 读写的稳定性测试。

| 文件 | 说明 | 密码 |
|---|---|---|
| ed25519-v3.ppk | `input_clear_key`，v3 未加密 | — |
| ed25519-v3-argon2id.ppk | `input_encrypted_key`，v3 aes256-cbc + Argon2id | test-passphrase |
| ed25519-v2.ppk | `v2_clear_key`，v2 未加密 | — |
| ed25519-v2-aes.ppk | `v2_encrypted_key`，v2 aes256-cbc | test-passphrase |
| ed25519.pub | 由同一用例中的 `public_blob`（十六进制）与 `comment` 拼成的 authorized_keys 行 | — |

许可：PuTTY 使用 MIT 许可证，Copyright 1997-2026 Simon Tatham 等（见源码包 LICENCE）。

## kayrus/

来源：Go 模块 `github.com/kayrus/putty@v1.0.5` 的 `putty_test.go`（通过 `go mod download` 获取）。
文件内容由 `puttygen` 生成（测试代码注释中记录了生成命令，例如 `puttygen -t rsa -b 512 -C "a@b" -o pass.ppk`），
密码均为 `testkey`，注释均为 `a@b`。

| 文件 | 说明 |
|---|---|
| rsa512b-v2.ppk | `keyContent`，v2 未加密 RSA-512（另一把密钥，无独立的期望公钥） |
| rsa512-v2-aes.ppk | v2 加密 RSA-512 |
| dsa-v2-aes.ppk | v2 加密 DSA（用于验证返回不支持） |
| ecdsa256-v2-aes.ppk / ecdsa384-v2-aes.ppk / ecdsa521-v2-aes.ppk | v2 加密 ECDSA |
| ed25519-v2-aes.ppk | v2 加密 Ed25519 |
| rsa2048-v3.ppk | v3 未加密 RSA-2048 |
| rsa2048-v3-argon2id.ppk | v3 加密 RSA-2048（Argon2id） |
| *.pub | 期望公钥：由该测试文件中独立给出的私钥参数（`rsaKey`、`rsa2048Key`、`ecDsa256Key` 等大整数）计算得到，不依赖本模块的解析器 |

许可：Apache License 2.0（见该模块的 LICENSE）。

## 其他覆盖

- Argon2i / Argon2d、Ed25519 种子的小端编码边界（末尾 0 字节被省略、开头为 0 字节）、私钥与公钥不一致：
  没有现成的真实文件。`ppk.rs` 测试中的 `encode_ppk` 按 PuTTY `ppk_save` 的算法生成 PPK，
  先与上面 4 个 PuTTY 官方文件逐字节比对一致，再用它构造这些变体。
- OpenSSH / PEM：仓库中不存放这类私钥（gitleaks 会拦截）。测试运行时构造：密钥由 ssh-key 随机生成或取自上面的 ppk 向量，
  PKCS#8 由 russh 编码，再拆成 PKCS#1、拼成 SEC1、加上旧式 PEM 加密（AES、3DES）；
  另有用系统自带的 `ssh-keygen`、`openssl` 现场生成的兼容性测试（找不到时跳过）。
