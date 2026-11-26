//! 局域网中的客户端经一次性传输链接传文件（HTTP 一侧见 `cli::http::link`）：给出链接之前的检查（[`prepare`]），与经
//! 链接的上传（[`upload`]）、下载（[`download`]）。
//!
//! 检查与命令行的传输相同：锁定、开关、主机授权、保险库，用 CLI 的凭据连接；真正传输时再检查一遍（给出链接之后设置
//! 可能变了）。传文件不经危险命令规则（与 `upload_file`、`download_file` 相同）。每次传输一条审计记录：给出链接之前就
//! 失败的也记，给出链接本身不记（服务器上什么也没发生）。调用方是取链接的 MCP 客户端 `mcp:<客户端名> < http <
//! lan:<IP>`；详情写“对方的地址 → 服务器上的路径”（下载反过来），链接的随机串不进审计。

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use termlet_proto::{ErrorCode, Lang, TransferResponse};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::oneshot;

use super::{Call, Ctx};
use crate::audit::{AuditAction, AuditStatus};
use crate::cli::connect::{self, Credentials};
use crate::cli::error::{CliError, Failure, Refused, from_sftp};
use crate::cli::service::{Cancel, Shared};
use crate::id::HostId;
use crate::sftp::{self, Probe, Rejection, SftpError, Side};

/// 检查过、可以给出链接的传输。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkPlan {
    pub host_id: HostId,
    /// 主机名（结果中给 AI 看）
    pub host_name: String,
    /// 服务器上的路径（AI 给的原样）
    pub remote: String,
    pub upload: bool,
    /// 下载的文件现在的大小
    pub size: Option<u64>,
}

fn action(upload: bool) -> AuditAction {
    if upload {
        AuditAction::Upload
    } else {
        AuditAction::Download
    }
}

/// 审计的详情：数据从哪里到哪里（对方的地址代替本机路径）。
fn detail(upload: bool, peer: IpAddr, remote: &str) -> String {
    if upload {
        format!("{peer} → {remote}")
    } else {
        format!("{remote} → {peer}")
    }
}

/// 给出传输链接之前：检查锁定、开关、主机授权与保险库，连上服务器看一眼路径（下载的要是普通文件；上传的不能是
/// 目录，不存在时所在的目录要在）。失败时写一条审计记录，返回错误。`caller` 是取链接的客户端，`peer` 是它的地址。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare(
    shared: &Shared,
    lang: Lang,
    host: &str,
    remote: &str,
    upload: bool,
    peer: IpAddr,
    caller: &[String],
    cancel: Cancel,
) -> Result<LinkPlan, CliError> {
    let ctx = Ctx {
        shared,
        lang,
        cancel,
    };
    let mut call = Call::new(action(upload), host, detail(upload, peer, remote), caller);
    match ctx.prepare_link(&mut call, remote, upload).await {
        Ok(plan) => Ok(plan),
        Err(failure) => {
            call.record(shared, failure.status, Some(&failure.error))
                .await;
            Err(failure.error)
        }
    }
}

/// 经链接上传：`reader` 是 HTTP 请求的正文（`size` 字节）。再检查一遍，连上服务器写入（见 [`sftp::put_stream`]），
/// 写一条审计记录。`progress` 加上确认写入的字节数（调用方据此看有没有停住）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn upload<R: AsyncRead + Unpin>(
    shared: &Shared,
    lang: Lang,
    plan: &LinkPlan,
    peer: IpAddr,
    caller: &[String],
    reader: &mut R,
    size: u64,
    progress: &AtomicU64,
    cancel: Cancel,
) -> Result<TransferResponse, CliError> {
    let ctx = Ctx {
        shared,
        lang,
        cancel,
    };
    let mut call = Call::new(
        AuditAction::Upload,
        plan.host_id.as_str(),
        detail(true, peer, &plan.remote),
        caller,
    );
    match ctx
        .link_upload(&mut call, plan, peer, reader, size, progress)
        .await
    {
        Ok(done) => {
            call.entry.bytes = Some(done.bytes as i64);
            call.entry.detail = detail(true, peer, &done.remote);
            call.record(shared, AuditStatus::Ok, None).await;
            Ok(done)
        }
        Err(failure) => {
            call.entry.bytes = partial(progress);
            call.record(shared, failure.status, Some(&failure.error))
                .await;
            Err(failure.error)
        }
    }
}

/// 经链接下载：再检查一遍，连上服务器打开文件后经 `ready` 交出它的大小（HTTP 的回答头要写 Content-Length），再写进
/// `sink`（回答的正文，见 [`sftp::RemoteReader::send_to`]），写一条审计记录。打开之前就失败的经 `ready` 交出错误；
/// 之后失败的只记审计（对方按 Content-Length 发现没收齐）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn download<W: AsyncWrite + Unpin>(
    shared: &Shared,
    lang: Lang,
    plan: &LinkPlan,
    peer: IpAddr,
    caller: &[String],
    ready: oneshot::Sender<Result<u64, CliError>>,
    sink: &mut W,
    progress: &AtomicU64,
    cancel: Cancel,
) {
    let ctx = Ctx {
        shared,
        lang,
        cancel,
    };
    let mut call = Call::new(
        AuditAction::Download,
        plan.host_id.as_str(),
        detail(false, peer, &plan.remote),
        caller,
    );
    let mut ready = Some(ready);
    let result = ctx
        .link_download(&mut call, plan, &mut ready, sink, progress)
        .await;
    match result {
        Ok(bytes) => {
            call.entry.bytes = Some(bytes as i64);
            call.record(shared, AuditStatus::Ok, None).await;
        }
        Err(failure) => {
            call.entry.bytes = partial(progress);
            call.record(shared, failure.status, Some(&failure.error))
                .await;
            if let Some(ready) = ready {
                let _ = ready.send(Err(failure.error));
            }
        }
    }
}

impl Ctx<'_> {
    async fn prepare_link(
        &self,
        call: &mut Call,
        remote: &str,
        upload: bool,
    ) -> Result<LinkPlan, Failure> {
        let record = self.access(call)?;
        self.check_vault()?;
        let mut cancel = self.cancel.clone();
        let (connection, refused) = connect::connect(
            self.shared.store.clone(),
            &record.id,
            Credentials::Cli,
            &mut cancel,
        )
        .await?;
        let probed = sftp::probe(&connection, remote, upload, record.doc.ssh.single_channel).await;
        connect::disconnect_later(connection);
        let size = match probed {
            Ok(Probe::File { size }) => Some(size),
            Ok(Probe::Missing) => None,
            Ok(Probe::Directory) if upload => {
                return Err(CliError::new(
                    ErrorCode::BadRequest,
                    self.tr(
                        &format!("{remote} 是服务器上的目录：remote_path 要写文件的完整路径（含文件名）"),
                        &format!(
                            "{remote} is a directory on the server: remote_path must be the full path of the file, including its name"
                        ),
                    ),
                )
                .into());
            }
            Ok(Probe::Directory) => {
                let error = SftpError::from(Rejection::NotRegularFile);
                return Err(from_sftp(&error, None, self.lang).into());
            }
            Err(e) => return Err(self.sftp_failure(&e, &refused).into()),
        };
        Ok(LinkPlan {
            host_id: record.id,
            host_name: record.doc.name,
            remote: remote.to_string(),
            upload,
            size: if upload { None } else { size },
        })
    }

    async fn link_upload<R: AsyncRead + Unpin>(
        &self,
        call: &mut Call,
        plan: &LinkPlan,
        peer: IpAddr,
        reader: &mut R,
        size: u64,
        progress: &AtomicU64,
    ) -> Result<TransferResponse, Failure> {
        let record = self.access(call)?;
        self.check_vault()?;
        let mut cancel = self.cancel.clone();
        let (connection, refused) = connect::connect(
            self.shared.store.clone(),
            &record.id,
            Credentials::Cli,
            &mut cancel,
        )
        .await?;
        let expiring_certificates = connect::expiring_certificates(&connection);
        let stop = cancel.wait();
        let result = sftp::put_stream(
            &connection,
            reader,
            size,
            &plan.remote,
            record.doc.ssh.single_channel,
            progress,
            stop,
        )
        .await;
        connect::disconnect_later(connection);
        let done = result.map_err(|e| self.sftp_failure(&e, &refused))?;
        Ok(TransferResponse {
            local: peer.to_string(),
            remote: done.path,
            bytes: done.bytes,
            duration_ms: call.elapsed_ms(),
            protocol: protocol_name(done.protocol).to_string(),
            expiring_certificates,
        })
    }

    async fn link_download<W: AsyncWrite + Unpin>(
        &self,
        call: &mut Call,
        plan: &LinkPlan,
        ready: &mut Option<oneshot::Sender<Result<u64, CliError>>>,
        sink: &mut W,
        progress: &AtomicU64,
    ) -> Result<u64, Failure> {
        let record = self.access(call)?;
        self.check_vault()?;
        let mut cancel = self.cancel.clone();
        let (connection, refused) = connect::connect(
            self.shared.store.clone(),
            &record.id,
            Credentials::Cli,
            &mut cancel,
        )
        .await?;
        let opened = sftp::open_reader(&connection, &plan.remote, record.doc.ssh.single_channel)
            .await
            .map_err(|e| self.sftp_failure(&e, &refused));
        let file = match opened {
            Ok(file) => file,
            Err(e) => {
                connect::disconnect_later(connection);
                return Err(e.into());
            }
        };
        if let Some(ready) = ready.take()
            && ready.send(Ok(file.size())).is_err()
        {
            // 对方已经走了（回答头还没发出）
            connect::disconnect_later(connection);
            return Err(CliError::code(ErrorCode::Canceled).into());
        }
        let sent = file.send_to(sink, progress, cancel.wait()).await;
        drop(file);
        connect::disconnect_later(connection);
        let bytes = sent.map_err(|e| self.sftp_failure(&e, &refused))?;
        // 发够了就结束回答的正文，不等审计写完
        let _ = sink.shutdown().await;
        Ok(bytes)
    }

    /// 传输链接上的失败：对方（curl 那一端）断开、停住的算取消，其余与命令行的传输相同。
    fn sftp_failure(
        &self,
        error: &SftpError,
        refused: &std::sync::Mutex<Option<Refused>>,
    ) -> CliError {
        if let SftpError::Failed {
            side: Side::Local, ..
        } = error
        {
            return CliError::new(
                ErrorCode::Canceled,
                self.tr(
                    "传输链接的另一端（curl）在传完之前断开或停住了",
                    "the other end of the link (curl) disconnected or stalled before the transfer finished",
                ),
            );
        }
        let refused = refused.lock().unwrap_or_else(|e| e.into_inner()).clone();
        from_sftp(error, refused.as_ref(), self.lang)
    }
}

/// 没传完的传输已经传了多少（审计中记下；还没开始传的不记）。
fn partial(progress: &AtomicU64) -> Option<i64> {
    let done = progress.load(Ordering::Relaxed);
    (done > 0).then_some(done as i64)
}

fn protocol_name(protocol: sftp::Protocol) -> &'static str {
    match protocol {
        sftp::Protocol::Sftp => termlet_proto::TRANSFER_SFTP,
        sftp::Protocol::Scp => termlet_proto::TRANSFER_SCP,
    }
}
