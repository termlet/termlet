//! 同步文件：云端只有一个文件，内容是 gzip 压缩的 JSON，整个文件用同步密码加密（与完整备份相同的信封，类型为
//! `sync`，见 [`crate::backup::envelope`]：Argon2id 派生密钥、AES-256-GCM），云端看不到任何内容，包括主机名与地址。
//!
//! ```text
//! {"format":"termlet-sync","version":1,"schema":1,"app":"…","updatedAt":…,
//!  "devices":{"<设备 ID>":{"id","name","app","lastSync"}},
//!  "records":{"host/h_…":{"v":"<版本>","data":{…}}, …}}
//! ```
//!
//! `version` 是这个文件格式的版本，`schema` 是写入它的 Termlet 的数据库结构版本；任一个比本程序新时不读也不写
//! （请用户先升级）。

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use super::records::{Data, Remote, Remotes};
use super::{Device, SyncError};
use crate::backup::envelope::{self, SealedKind};
use crate::db::SCHEMA_VERSION;
use crate::secret::SecretString;

/// 同步位置中的文件名。
pub const FILE_NAME: &str = "termlet-sync.tlsync";

const FORMAT: &str = "termlet-sync";
const FORMAT_VERSION: u32 = 1;

/// 同步文件（加密后）的大小上限。
pub(crate) const MAX_FILE_SIZE: usize = 256 << 20;
/// 同步文件解压后（JSON）的大小上限。写入时同样检查，本程序不会写出自己读不了的同步文件。
pub(crate) const MAX_DOCUMENT_SIZE: usize = 256 << 20;

/// 同步文件解密后的内容。
#[derive(Clone, Debug, Default)]
pub(crate) struct Document {
    pub devices: BTreeMap<String, Device>,
    pub records: Remotes,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    format: String,
    version: u32,
    #[serde(default)]
    schema: i64,
    #[serde(default)]
    devices: BTreeMap<String, Device>,
    #[serde(default)]
    records: BTreeMap<String, WireRecord>,
}

#[derive(Deserialize)]
struct WireRecord {
    v: String,
    data: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Head<'a> {
    format: &'a str,
    version: u32,
    schema: i64,
    app: &'a str,
    updated_at: i64,
    devices: &'a BTreeMap<String, Device>,
}

/// 解开同步文件。在阻塞线程上调用（派生密钥要几百毫秒）。
pub(crate) fn open(
    data: &[u8],
    password: &SecretString,
    limit: usize,
) -> Result<Document, SyncError> {
    if !envelope::is_sealed(data) || envelope::peek_kind(data) != Some(SealedKind::Sync) {
        return Err(SyncError::Corrupt);
    }
    let (_, payload) = envelope::open(data, Some(password))?;
    let mut raw = Zeroizing::new(Vec::new());
    flate2::read::GzDecoder::new(&payload[..])
        .take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| SyncError::Corrupt)?;
    if raw.len() > limit {
        return Err(SyncError::TooLarge);
    }
    let wire: Wire = serde_json::from_slice(&raw).map_err(|_| SyncError::Corrupt)?;
    if wire.format != FORMAT {
        return Err(SyncError::Corrupt);
    }
    if wire.version > FORMAT_VERSION || wire.schema > SCHEMA_VERSION {
        return Err(SyncError::TooNew);
    }
    let records = wire
        .records
        .into_iter()
        .map(|(key, r)| (key, Remote::new(r.v, Data::of(&r.data))))
        .collect();
    Ok(Document {
        devices: wire.devices,
        records,
    })
}

/// 加密同步文件。在阻塞线程上调用。
pub(crate) fn seal(
    doc: &Document,
    password: &SecretString,
    app: &str,
    now: i64,
    limit: usize,
) -> Result<Vec<u8>, SyncError> {
    // 记录的内容已经是规范的 JSON 文本，直接拼进去（不再解析一遍）
    let head = serde_json::to_string(&Head {
        format: FORMAT,
        version: FORMAT_VERSION,
        schema: SCHEMA_VERSION,
        app,
        updated_at: now,
        devices: &doc.devices,
    })?;
    let mut text = Zeroizing::new(String::with_capacity(head.len() + doc.records.len() * 256));
    text.push_str(&head[..head.len() - 1]);
    text.push_str(",\"records\":{");
    for (i, (key, record)) in doc.records.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&serde_json::to_string(key)?);
        text.push_str(":{\"v\":");
        text.push_str(&serde_json::to_string(&record.v)?);
        text.push_str(",\"data\":");
        text.push_str(record.data.as_str());
        text.push('}');
        if text.len() > limit {
            return Err(SyncError::TooLarge);
        }
    }
    text.push_str("}}");
    if text.len() > limit {
        return Err(SyncError::TooLarge);
    }
    let mut compressed = Zeroizing::new(Vec::new());
    {
        let mut gz =
            flate2::write::GzEncoder::new(&mut *compressed, flate2::Compression::default());
        gz.write_all(text.as_bytes())
            .and_then(|_| gz.finish().map(|_| ()))
            .map_err(|e| SyncError::Local {
                message: format!("压缩同步数据失败：{e}"),
            })?;
    }
    let sealed = envelope::seal(SealedKind::Sync, password, &compressed)?;
    if sealed.len() > MAX_FILE_SIZE {
        return Err(SyncError::TooLarge);
    }
    Ok(sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXED_PASSWORD: &str = "correct horse battery staple";
    /// 由 aes-gcm 0.10、argon2 0.5 写出的同步文件（Base64；测试用的派生参数）：一台设备、一条主机记录。
    /// 升级加密库后必须照常解开。
    const FIXED_FILE: &str = "dGVybWxldC1zZWFsZWQvMQp7ImtpbmQiOiJzeW5jIiwia2RmIjoiYXJnb24yaWQiLCJ0aW1lIjoxLCJtZW1vcnkiOjgsInRocmVhZHMiOjEsInNhbHQiOiIrLzhxRmJIdWZhaFQra3ZLMEFkcTdBPT0ifQoBoM8Qj/HJ8eXFBFchuGXIGdPFoMOH9T030lXZy+KD8NSviLh3gpIrVN0z7yy51rGAsalKrOZV7vBZg2UN6jXga+h7zQS1VGQ06/JCX8bQeGrcVw1M1Fyr9xfbHJzGPCqTuYj7QPKegyK0mumsB497T4pSFfWB5y7CdQI+dOqOq4gtqoR49L7QnU411+3ckovahzZvhlovYu9iocOv9OIDOdVqJfWU6GvKTezLfgop0yRIgmolEYGTSPsUqi2vttOAd7S435SttW3++N729i0XsKLxhx34fIcPAXYmEP3ORmNqthgwH6i8rUTbTXNK+XQX7kk=";

    #[test]
    fn open_fixed_file() {
        let data = data_encoding::BASE64.decode(FIXED_FILE.as_bytes()).unwrap();
        let password = SecretString::new(FIXED_PASSWORD);
        let doc = open(&data, &password, MAX_DOCUMENT_SIZE).unwrap();
        assert_eq!(
            doc.devices.values().collect::<Vec<_>>(),
            [&Device {
                id: "d_fixed".into(),
                name: "工作电脑".into(),
                app: "0.1.0".into(),
                last_sync: 1_793_000_000_000,
            }]
        );
        let record = &doc.records["host/h_fixed"];
        assert_eq!(record.v, "1793000000000-d_fixed-0123abcd");
        assert_eq!(
            record.data.as_str(),
            r#"{"address":"10.0.0.1","name":"web-01"}"#
        );
        assert_eq!(doc.records.len(), 1);
        assert_eq!(
            open(
                &data,
                &SecretString::new("wrong password"),
                MAX_DOCUMENT_SIZE
            )
            .unwrap_err(),
            SyncError::WrongPassword
        );
    }
}
