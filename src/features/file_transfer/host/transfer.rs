use super::super::service::PartialFile;
use super::filesystem::{Record, Reservation, Store};
use super::*;
use std::{collections::HashSet, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

struct Rpc {
    request_header: Correlation,
    reply_header: Correlation,
    direction: Direction,
    task: i32,
    rx: mpsc::Receiver<Incoming>,
    output: mpsc::Sender<Packet>,
    stop: CancellationToken,
    capabilities: Capabilities,
    progress: notices::Progress,
}
impl Rpc {
    fn id(&self, index: i32) -> Option<TaskId> {
        Some(TaskId {
            task_id: self.task,
            file_index: index,
        })
    }
    async fn send(&self, packet: Packet) -> Result<()> {
        ensure!(packet.data.len() < PAYLOAD_LIMIT, "文件响应过大");
        tokio::select! {biased;_=self.stop.cancelled()=>anyhow::bail!("文件任务已暂停"),r=tokio::time::timeout(Duration::from_secs(30),self.output.send(packet))=>{r.context("文件发送队列超时")??;Ok(())}}
    }
    async fn res(&self, value: Res) -> Result<()> {
        self.send(response(self.reply_header, value)).await
    }
    async fn req(&self, value: Req, file: bool) -> Result<()> {
        self.send(request(self.request_header, value, file)).await
    }
    async fn next(&mut self) -> Result<Payload> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let message = tokio::select! {biased;_=self.stop.cancelled()=>anyhow::bail!("文件任务已暂停"),v=tokio::time::timeout_at(deadline,self.rx.recv())=>v.context("文件任务等待超时")?.context("文件连接已结束")?};
            let id = message.payload.task().context("传输消息缺少TaskId")?;
            ensure!(id.task_id == self.task, "文件任务编号不符");
            if let Payload::Response(Res::Result(v)) = &message.payload {
                super::super::success(v.file_error)?;
                if self.direction == Direction::Send {
                    continue;
                }
            }
            if matches!(&message.payload, Payload::Request(_)) {
                self.reply_header = message.header;
            }
            return Ok(message.payload);
        }
    }
    fn check(&self, id: &Option<TaskId>, index: i32) -> Result<()> {
        contract::file_id(id, self.task, Some(index), false).map(|_| ())
    }
    fn check_response(&self, id: &Option<TaskId>, index: i32) -> Result<()> {
        contract::file_id(id, self.task, Some(index), true).map(|_| ())
    }
    async fn complete(&self, index: i32, error: i32) -> Result<()> {
        self.req(
            Req::Complete(FileTransferComplete {
                id: self.id(index),
                error,
            }),
            false,
        )
        .await
    }
    async fn result(&self, index: i32, code: i32) -> Result<()> {
        self.res(Res::Result(FileTransferResult {
            id: self.id(index),
            file_error: code,
            err_msg: if code == 1 {
                String::new()
            } else {
                super::super::error(code)
            },
        }))
        .await
    }
}
pub(super) async fn run(
    first: Incoming,
    rx: mpsc::Receiver<Incoming>,
    output: mpsc::Sender<Packet>,
    store: Arc<Store>,
    stop: CancellationToken,
    discard: Arc<std::sync::atomic::AtomicBool>,
    capabilities: Capabilities,
    journal: notices::Journal,
) {
    let task = first.payload.task().expect("checked initial task").task_id;
    let mut rpc = Rpc {
        // Official sender-initiated FileAsk/Block/Complete requests are routed
        // by task_id (4.41 S71ABB0), independently of the peer's init RPC ID.
        // Responses to peer requests still echo reply_header below.
        request_header: Correlation::new(i64::from(task as u32)),
        direction: match first.payload.kind() {
            Kind::Start(direction) => direction,
            _ => unreachable!(),
        },
        reply_header: first.header,
        task,
        rx,
        output,
        stop,
        capabilities,
        progress: journal.begin(matches!(
            first.payload,
            Payload::Request(Req::ReceiveRequest(_))
        )),
    };
    let key = match &first.payload {
        Payload::Request(Req::ReceiveRequest(v)) => Some(v.task_unique_id.clone()),
        _ => None,
    };
    let result = match first.payload {
        Payload::Request(Req::ReceiveRequest(v)) => receive(&mut rpc, v, store.clone()).await,
        Payload::Request(Req::SendRequest(v)) => send(&mut rpc, v).await,
        _ => unreachable!(),
    };
    if discard.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(key) = key {
            let cleanup = tokio::task::spawn_blocking(move || store.clear(&key)).await;
            if !matches!(cleanup, Ok(Ok(()))) {
                tracing::warn!(task, ?cleanup, "cancelled receive cleanup failed");
            }
        }
    }
    match &result {
        Ok(()) => rpc.progress.terminal(notices::COMPLETE, String::new()),
        Err(_) if rpc.stop.is_cancelled() => {
            rpc.progress.terminal(notices::INTERRUPTED, String::new())
        }
        Err(e) => rpc.progress.terminal(notices::FAILED, e.to_string()),
    }
    if let Err(e) = result {
        if rpc.stop.is_cancelled() {
            tracing::debug!(task, error=%e,"host file task stopped");
        } else {
            tracing::warn!(task, request=?rpc.reply_header, error=%e,"host file task failed");
        }
        if !rpc.stop.is_cancelled() {
            let _ = rpc.result(-1, failure(&e)).await;
        }
    }
}
fn upsert(r: &mut Record, p: PartialFile) {
    if let Some(old) = r
        .partial
        .iter_mut()
        .find(|v| v.info.rel_path == p.info.rel_path)
    {
        *old = p;
    } else {
        r.partial.push(p);
    }
}
async fn persist(store: &Arc<Store>, record: &Record) -> Result<()> {
    let store = store.clone();
    let record = record.clone();
    tokio::task::spawn_blocking(move || store.save(&record)).await?
}
async fn receive(
    rpc: &mut Rpc,
    mut v: FileTransferReceiveRequest,
    store: Arc<Store>,
) -> Result<()> {
    tracing::debug!(
        task = rpc.task,
        strategy = v.file_op_strategy,
        manifest_items = v.files.len(),
        compressed_manifest_bytes = v.list_data.len(),
        resume_key_present = !v.task_unique_id.is_empty(),
        default_directory = v.path == ":/Default",
        "host file receive requested"
    );
    let id = v.id.clone();
    let init_store = store.clone();
    let initialized =
        tokio::task::spawn_blocking(move || -> Result<(Record, Vec<Reservation>, Reservation)> {
            v.file_op_strategy = contract::collision_policy(v.file_op_strategy)?;
            let files = if v.list_data.is_empty() {
                v.files
            } else {
                storage::decompress::<FileList>(&v.list_data)?.files
            };
            storage::validate_manifest(&files)?;
            let record_lock = init_store.reserve_record(&v.task_unique_id)?;
            let destination = filesystem::directory(&v.path)?;

            let record = if v.file_op_strategy == 4 {
                let r = init_store.load(&v.task_unique_id)?.ok_or(ErrorCode(11))?;
                if r.destination != destination || r.folder != v.folder_name {
                    return Err(ErrorCode(11).into());
                }
                for f in &files {
                    if !r.files.iter().any(|old| old == f) {
                        return Err(ErrorCode(11).into());
                    }
                }
                Record { files, ..r }
            } else {
                ensure!(
                    init_store.load(&v.task_unique_id)?.is_none(),
                    "任务标识已有记录，请继续原任务或先取消"
                );
                let root =
                    filesystem::receiving_root(&destination, &v.folder_name, v.file_op_strategy)?;
                Record {
                    key: uuid::Uuid::new_v4().to_string(),
                    remote_key: v.task_unique_id,
                    destination,
                    root,
                    folder: v.folder_name,
                    policy: v.file_op_strategy,
                    files,
                    partial: Vec::new(),
                }
            };
            let targets = if !record.folder.is_empty() || record.files.is_empty() {
                vec![record.root.clone()]
            } else {
                record
                    .files
                    .iter()
                    .map(|f| record.root.join(&f.rel_path))
                    .collect()
            };
            let destination_locks = targets
                .iter()
                .map(|p| Reservation::acquire(init_store.clone(), p))
                .collect::<Result<Vec<_>>>()?;
            init_store.save(&record)?;
            Ok((record, destination_locks, record_lock))
        })
        .await?;
    let (mut record, _reservation, _record_lock) = match initialized {
        Ok(v) => v,
        Err(e) => {
            rpc.progress.terminal(notices::FAILED, e.to_string());
            tracing::warn!(task=rpc.task, error=%e, "file receive initialization failed");
            rpc.res(Res::ReceiveResponse(FileTransferReceiveResponse {
                id,
                err: failure(&e),
                files: Vec::new(),
            }))
            .await?;
            return Ok(());
        }
    };
    rpc.progress.describe(&record.files, &record.root);
    rpc.res(Res::ReceiveResponse(FileTransferReceiveResponse {
        id: rpc.id(-1),
        files: Vec::new(),
        err: 1,
    }))
    .await?;
    let mut open: Option<(i32, storage::Receiving)> = None;
    let mut completed = HashSet::new();
    let mut clear = false;
    let mut terminal_reply = None;
    let outcome = async {
        loop {
            match rpc.next().await? {
                Payload::Request(Req::FileAsk(v)) => {
                    ensure!(open.is_none(), "前一个文件尚未完成");
                    let index = v.id.as_ref().context("文件编号缺失")?.file_index;
                    ensure!(index > 0 && !completed.contains(&index), "文件序号重复");
                    let info = record
                        .files
                        .get((index - 1) as usize)
                        .context("文件序号越界")?
                        .clone();
                    contract::metadata(&info, &v)?;
                    let old = record
                        .partial
                        .iter()
                        .find(|p| p.info.rel_path == info.rel_path)
                        .cloned();
                    let root = record.root.clone();
                    let key = record.key.clone();
                    let policy = record.policy;
                    let item = info.clone();
                    let prepared = tokio::task::spawn_blocking(move || {
                        storage::prepare(&root, &key, &item, policy, old.as_ref())
                    })
                    .await?;
                    let prepared = match prepared {
                        Ok(p) => p,
                        Err(e) => {
                            rpc.progress.terminal(notices::FAILED, e.to_string());
                            rpc.res(Res::FileConfirm(FileTransferConfirm {
                                id: rpc.id(index),
                                err: failure(&e),
                                ..Default::default()
                            }))
                            .await?;
                            return Ok(());
                        }
                    };
                    let skip = prepared.is_none();
                    let resume_point = prepared.as_ref().map_or(0, |p| p.position);
                    rpc.progress
                        .advance(if skip { info.size } else { resume_point });
                    if let Some(mut p) = prepared {
                        upsert(&mut record, p.partial.clone());
                        p.file.seek(std::io::SeekFrom::Start(resume_point)).await?;
                        open = Some((index, p));
                    } else {
                        completed.insert(index);
                        if !record
                            .partial
                            .iter()
                            .any(|p| p.info.rel_path == info.rel_path)
                        {
                            upsert(
                                &mut record,
                                PartialFile {
                                    target: info.rel_path.clone(),
                                    info,
                                    done: false,
                                    skipped: true,
                                },
                            );
                        }
                    }
                    persist(&store, &record).await?;
                    rpc.res(Res::FileConfirm(FileTransferConfirm {
                        id: rpc.id(index),
                        skip,
                        err: 1,
                        resume_point,
                    }))
                    .await?;
                }
                Payload::Request(Req::FileBlock(v)) => {
                    let (index, p) = open.as_mut().context("文件尚未打开")?;
                    rpc.check(&v.id, *index)?;
                    // The reliable ordered carrier owns ordering. block_id is
                    // an opaque ACK correlation value, not a required 1-based
                    // sequence. The envelope decoder already enforces WIRE;
                    // BLOCK is our sender's chunk choice, not a receive limit.
                    let end =
                        contract::received_bytes(p.position, v.data.len(), p.partial.info.size)?;
                    p.file.write_all(&v.data).await?;
                    p.position = end;
                    rpc.progress.advance(v.data.len() as u64);
                    rpc.res(Res::BlockConfirm(FileTransferBlockConfirm {
                        id: rpc.id(*index),
                        block_id: v.block_id,
                        err: 1,
                        block_len: v.data.len() as i32,
                    }))
                    .await?;
                }
                Payload::Request(Req::Complete(v)) => {
                    let index = v.id.as_ref().context("完成消息编号缺失")?.file_index;
                    tracing::debug!(
                        task = rpc.task,
                        request = ?rpc.reply_header,
                        file_index = index,
                        error = v.error,
                        "host file completion received"
                    );
                    if v.error != 1 {
                        rpc.progress.terminal(
                            match v.error {
                                10 => notices::PAUSED,
                                7 => notices::CANCELLED,
                                _ => notices::FAILED,
                            },
                            if matches!(v.error, 7 | 10) {
                                String::new()
                            } else {
                                super::super::error(v.error)
                            },
                        );
                        if let Some((_, mut file)) = open.take() {
                            file.file.flush().await?;
                            file.file.sync_all().await?;
                            drop(file);
                        }
                        persist(&store, &record).await?;
                        clear = v.error == 7;
                        terminal_reply = Some((index, v.error));
                        return Ok(());
                    }
                    if index == -1 {
                        ensure!(
                            open.is_none() && completed.len() == record.files.len(),
                            "文件任务提前结束"
                        );
                        clear = true;
                        terminal_reply = Some((index, 1));
                        return Ok(());
                    }
                    let (current, p) = open.take().context("未打开的文件不能完成")?;
                    ensure!(current == index, "完成文件编号不符");
                    let item = storage::finish(p, &record.root, &record.key, record.policy).await?;
                    rpc.progress.saved();
                    upsert(&mut record, item);
                    persist(&store, &record).await?;
                    completed.insert(index);
                    rpc.result(index, 1).await?;
                }
                _ => anyhow::bail!("接收任务状态与消息不匹配"),
            }
        }
    }
    .await;
    // Tokio file writes may still be buffered when cancellation wins the next
    // message wait. Finish those accepted writes before releasing resume state.
    if let Some((_, mut file)) = open.take() {
        file.file.flush().await?;
        file.file.sync_all().await?;
    }
    persist(&store, &record).await?;
    if clear {
        let s = store.clone();
        let key = record.remote_key.clone();
        tokio::task::spawn_blocking(move || s.clear(&key)).await??;
    }
    drop(_reservation);
    drop(_record_lock);
    if let Some((index, error)) = terminal_reply {
        rpc.result(index, error).await?;
    }
    outcome
}
async fn send(rpc: &mut Rpc, v: FileTransferSendRequest) -> Result<()> {
    let stop = rpc.stop.clone();
    let id = v.id.clone();
    let scan = tokio::task::spawn_blocking(move || -> Result<_> {
        let source = filesystem::local_path(&v.path)?;
        let (root, folder, mut files) = storage::scan(&source, &stop)?;
        if !v.list_data.is_empty() {
            let requested = storage::decompress::<FileList>(&v.list_data)?.files;
            storage::validate_manifest(&requested)?;
            for f in &requested {
                if !files.iter().any(|x| x == f) {
                    return Err(ErrorCode(11).into());
                }
            }
            // An empty selection means scan the source, including an encoded empty list;
            // not a request to replace the scanned manifest with zero files.
            if !requested.is_empty() {
                files = requested;
            }
        }
        Ok((root, folder, files))
    })
    .await?;
    let (root, folder, files) = match scan {
        Ok(v) => v,
        Err(e) => {
            rpc.progress.terminal(notices::FAILED, e.to_string());
            tracing::warn!(task=rpc.task, error=%e, "file send initialization failed");
            rpc.res(Res::SendResponse(FileTransferSendResponse {
                id,
                err: failure(&e),
                ..Default::default()
            }))
            .await?;
            return Ok(());
        }
    };
    rpc.progress.describe(&files, &root);
    let (listed_files, list_data) = if rpc.capabilities.compressed {
        (
            Vec::new(),
            storage::compress(&FileList {
                files: files.clone(),
            })?,
        )
    } else {
        (files.clone(), Vec::new())
    };
    rpc.res(Res::SendResponse(FileTransferSendResponse {
        id: rpc.id(-1),
        err: 1,
        folder_name: folder,
        list_data,
        files: listed_files,
    }))
    .await?;
    for (n, info) in files.into_iter().enumerate() {
        let index = i32::try_from(n + 1)?;
        rpc.req(
            Req::FileAsk(FileTransferAsk {
                id: rpc.id(index),
                last_modified: info.modified_time,
                file_size: info.size,
            }),
            false,
        )
        .await?;
        let confirm = match rpc.next().await? {
            Payload::Response(Res::FileConfirm(v)) => v,
            Payload::Request(Req::Complete(v)) => {
                rpc.progress.terminal(
                    match v.error {
                        10 => notices::PAUSED,
                        7 => notices::CANCELLED,
                        _ => notices::FAILED,
                    },
                    super::super::error(v.error),
                );
                rpc.result(-1, v.error).await?;
                return Ok(());
            }
            _ => anyhow::bail!("文件准备响应不符"),
        };
        rpc.check_response(&confirm.id, index)?;
        super::super::success(confirm.err)?;
        if confirm.skip {
            rpc.progress.advance(info.size);
            continue;
        }
        ensure!(confirm.resume_point <= info.size, "续传位置超过文件大小");
        let dir = root.clone();
        let item = info.clone();
        let file = tokio::task::spawn_blocking(move || storage::open_source(&dir, &item)).await??;
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(confirm.resume_point))
            .await?;
        let mut position = confirm.resume_point;
        rpc.progress.advance(position);
        let mut sequence = 0i32;
        let mut pending = contract::PendingBlocks::default();
        let window = if rpc.capabilities.speedy { 16 } else { 1 };
        while position < info.size || !pending.is_empty() {
            if position < info.size && pending.len() < window {
                let len = BLOCK.min((info.size - position) as usize);
                let mut data = vec![0; len];
                tokio::select! {biased;_=rpc.stop.cancelled()=>anyhow::bail!("发送已暂停"),r=file.read_exact(&mut data)=>{r?;}}
                sequence = sequence.checked_add(1).context("文件块编号溢出")?;
                rpc.req(
                    Req::FileBlock(FileTransferBlock {
                        id: rpc.id(index),
                        block_id: sequence,
                        data,
                    }),
                    rpc.capabilities.speedy,
                )
                .await?;
                position += len as u64;
                pending.insert(sequence, len)?;
                if position < info.size && pending.len() < window {
                    continue;
                }
            }
            match rpc.next().await? {
                Payload::Response(Res::BlockConfirm(v)) => {
                    rpc.check_response(&v.id, index)?;
                    let expected = pending.confirm(&v)?;
                    rpc.progress.advance(expected as u64);
                }
                Payload::Request(Req::Complete(v)) => {
                    rpc.progress.terminal(
                        match v.error {
                            10 => notices::PAUSED,
                            7 => notices::CANCELLED,
                            _ => notices::FAILED,
                        },
                        super::super::error(v.error),
                    );
                    rpc.result(-1, v.error).await?;
                    return Ok(());
                }
                _ => anyhow::bail!("文件块响应不符"),
            }
        }
        drop(file);
        rpc.complete(index, 1).await?;
        // Every byte has been acknowledged. Official senders notify completion
        // and move on; there is no mandatory final-save RPC round trip.
        rpc.progress.saved();
    }
    rpc.complete(-1, 1).await
}
