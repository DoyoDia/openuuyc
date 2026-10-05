//! Inbound forwarding belongs to an authenticated peer, never to a listening server.
use super::*;
use crate::features::host::Lease;
use std::sync::atomic::AtomicBool;

const MAX_STREAMS: usize = 64;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    pub connections: usize,
    pub sent: u64,
    pub received: u64,
    pub error: Option<String>,
}

pub(crate) struct Request {
    generation: u64,
    policy: u64,
    frame: Frame,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    transport: Arc<Transport>,
    generation: Arc<AtomicU64>,
    requests: mpsc::Sender<Request>,
    lease: Lease,
    connected: Arc<AtomicBool>,
    changed: Arc<Notify>,
}
impl Receiver {
    pub(crate) fn new(lease: Lease, connected: Arc<AtomicBool>) -> (Self, mpsc::Receiver<Request>) {
        let (requests, input) = mpsc::channel(64);
        (
            Self {
                transport: Arc::default(),
                generation: Arc::default(),
                requests,
                lease,
                connected,
                changed: Arc::default(),
            },
            input,
        )
    }
    pub(crate) async fn bind(&self, channel: &Arc<RTCDataChannel>) {
        self.invalidate();
        self.transport.bind(channel);
        channel.set_buffered_amount_low_threshold(BUFFER / 2).await;
        let transport = self.transport.clone();
        channel
            .on_buffered_amount_low(Box::new(move || {
                transport.wake();
                Box::pin(async {})
            }))
            .await;
    }
    pub(crate) fn close(&self, channel: &Arc<RTCDataChannel>) {
        if lock(&self.transport.channel).ptr_eq(&Arc::downgrade(channel)) {
            self.invalidate();
            self.transport.close();
        }
    }
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_one();
        for route in lock(&self.transport.routes).values() {
            route.stop.cancel();
        }
    }
    pub(crate) async fn receive(
        &self,
        channel: &Arc<RTCDataChannel>,
        bytes: &[u8],
    ) -> Result<bool> {
        ensure!(bytes.len() <= WIRE_LIMIT, "端口转发消息过大");
        let Some(payload) = crate::features::stream_control::decode_port_mapping(bytes)? else {
            return Ok(false);
        };
        if !lock(&self.transport.channel).ptr_eq(&Arc::downgrade(channel)) {
            return Ok(true);
        }
        let frame = Frame::decode(payload.as_slice())?;
        // Only TCP and the explicit OpenUUYC UDP extension can create sockets.
        if !matches!(frame.kind,0..=4|16..=20) {
            return Ok(true);
        }
        ensure!(
            !matches!(frame.kind, 0 | 16) || frame.payload.len() <= 16_384,
            "端口转发握手过大"
        );
        self.requests
            .send(Request {
                generation: self.generation.load(Ordering::Acquire),
                policy: self.lease.port_policy().1,
                frame,
            })
            .await
            .context("端口转发会话已关闭")?;
        Ok(true)
    }
    fn permitted(&self, generation: u64, policy: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
            && self.connected.load(Ordering::Acquire)
            && self.lease.port_policy() == (true, policy)
    }
}

pub(crate) async fn run(
    receiver: Receiver,
    mut input: mpsc::Receiver<Request>,
    stop: CancellationToken,
) {
    let mut jobs = JoinSet::new();
    let stats = Arc::new(Mutex::new(Status::default()));
    let mut generation = receiver.generation.load(Ordering::Acquire);
    let mut policy = receiver.lease.port_policy();
    let mut changes = receiver.lease.port_changes();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if stop.is_cancelled() || !receiver.lease.requested() {
            break;
        }
        let next_generation = receiver.generation.load(Ordering::Acquire);
        let next_policy = receiver.lease.port_policy();
        if next_generation != generation
            || next_policy != policy
            || !receiver.connected.load(Ordering::Acquire)
        {
            for route in lock(&receiver.transport.routes).values() {
                route.stop.cancel();
            }
            while let Some(result) = jobs.join_next().await {
                if let Ok(key) = result {
                    lock(&receiver.transport.routes).remove(&key);
                }
            }
            generation = next_generation;
            policy = next_policy;
        }
        receiver.lease.port_status(lock(&stats).clone());
        tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            Some(result)=jobs.join_next(), if !jobs.is_empty()=>{
                if let Ok(key)=result {lock(&receiver.transport.routes).remove(&key);}
            },
            _=tick.tick()=>{},
            _=changes.changed()=>{},
            _=receiver.changed.notified()=>{},
            request=input.recv()=>{
                let Some(request)=request else{break};
                // Binding or permission may have changed while recv was asleep.
                let actual_generation=receiver.generation.load(Ordering::Acquire);
                let actual_policy=receiver.lease.port_policy();
                if actual_generation!=generation || actual_policy!=policy {
                    for route in lock(&receiver.transport.routes).values(){route.stop.cancel();}
                    while let Some(result)=jobs.join_next().await {
                        if let Ok(key)=result {lock(&receiver.transport.routes).remove(&key);}
                    }
                    generation=actual_generation;policy=actual_policy;
                }
                let frame=request.frame;
                if request.generation!=generation || request.policy!=policy.1 {continue;}
                if !matches!(frame.kind,0|16) {
                    if receiver.permitted(generation, policy.1) { let _=receiver.transport.deliver(frame); }
                    continue;
                }
                let key=(frame.rule_id,frame.stream_id);
                let denied=if !receiver.permitted(generation,policy.1) {Some("本机未允许端口转发")}
                    else if key.0==0 || key.1==0 {Some("无效转发编号")}
                    else if lock(&receiver.transport.routes).contains_key(&key) {Some("重复转发编号")}
                    else if jobs.len()>=MAX_STREAMS {Some("端口转发连接数已达上限")}
                    else {None};
                if let Some(error)=denied {
                    lock(&stats).error=Some(error.to_owned());
                    if let Some(route)=lock(&receiver.transport.routes).get(&key) { route.stop.cancel(); }
                    // Bounded rejection in this actor; never spawn one task per invalid packet.
                    let _=tokio::time::timeout(Duration::from_secs(2),reply(&receiver.transport,key,if frame.kind==16 {Protocol::Udp}else{Protocol::Tcp},false,error)).await;
                    continue;
                }
                let cancel=stop.child_token();
                let (tx,rx)=mpsc::channel(256);
                let remote_fin=Arc::new(AtomicBool::new(false));
                let fault=Arc::new(Mutex::new(None));
                lock(&receiver.transport.routes).insert(key,Route{
                    sender:tx,budget:Arc::new(Semaphore::new(BUFFER)),stop:cancel.clone(),
                    remote_fin:remote_fin.clone(),fault:fault.clone(),
                });
                let owner=receiver.clone();
                let stats=stats.clone();
                jobs.spawn(async move {
                    serve(owner,request.generation,request.policy,frame,rx,cancel,remote_fin,stats,fault).await;
                    key
                });
            }
        }
    }
    input.close();
    receiver.invalidate();
    while jobs.join_next().await.is_some() {}
    receiver.transport.close();
    receiver.lease.port_status(Status::default());
}

async fn reply(
    transport: &Transport,
    key: (u64, u64),
    protocol: Protocol,
    ok: bool,
    error: &str,
) -> Result<()> {
    transport
        .send(
            key.0,
            key.1,
            protocol.ack(),
            serde_json::to_vec(&serde_json::json!({"ok":ok,"version":1,"error":error}))?.into(),
        )
        .await
}
async fn serve(
    receiver: Receiver,
    generation: u64,
    policy: u64,
    syn: Frame,
    mut input: mpsc::Receiver<Incoming>,
    cancel: CancellationToken,
    remote_fin: Arc<AtomicBool>,
    stats: Arc<Mutex<Status>>,
    fault: Arc<Mutex<Option<String>>>,
) {
    let key = (syn.rule_id, syn.stream_id);
    let protocol = if syn.kind == 16 {
        Protocol::Udp
    } else {
        Protocol::Tcp
    };
    lock(&stats).connections += 1;
    let mut accepted = false;
    let result = tokio::select! {
        biased;
        _=cancel.cancelled()=>Ok(()),
        result=async {
            #[derive(serde::Deserialize)]
            struct Target { target_host: IpAddr, target_port: u16, version: u32 }
            let target:Target=serde_json::from_slice(&syn.payload).context("无效的目标地址或握手")?;
            ensure!(target.target_port!=0 && target.version>=1,"无效目标端口或协议版本");
            ensure!(!target.target_host.is_unspecified() && !target.target_host.is_multicast(),"目标必须为单播 IP 地址");
            ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
            if protocol==Protocol::Udp {
                let socket=tokio::net::UdpSocket::bind(if target.target_host.is_ipv4(){"0.0.0.0:0"}else{"[::]:0"}).await?;
                socket.connect(SocketAddr::new(target.target_host,target.target_port)).await?;
                reply(&receiver.transport,key,protocol,true,"").await?;
                accepted=true;
                tracing::debug!(rule=key.0,stream=key.1,?protocol,"host port forwarding stream opened");
                let first=tokio::time::timeout(TIMEOUT,input.recv()).await.context("UDP 握手超时")?.context("转发已关闭")?;
                if first.frame.kind==19 {return Ok(());}
                ensure!(matches!(first.frame.kind,18|20),"UDP 握手顺序错误");
                if first.frame.kind==18 {ensure!(first.frame.payload.len()<=65_507,"UDP 数据报过大");socket.send(&first.frame.payload).await?;}
                drop(first);
                let mut buffer=vec![0;65_536];
                loop {
                    ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
                    tokio::select! {
                        _=tokio::time::sleep(Duration::from_secs(60))=>return Ok(()),
                        packet=socket.recv(&mut buffer)=>{
                            let count=packet?;
                            ensure!(count<=65_507,"UDP 数据报过大");
                            receiver.transport.send(key.0,key.1,18,Bytes::copy_from_slice(&buffer[..count])).await?;
                            lock(&stats).sent+=count as u64;
                        },
                        packet=input.recv()=>{
                            let packet=packet.context("UDP 转发已关闭")?;
                            if packet.frame.kind==19 {return Ok(());}
                            ensure!(packet.frame.kind==18 && packet.frame.payload.len()<=65_507,"无效 UDP 数据报");
                            ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
                            socket.send(&packet.frame.payload).await?;
                            lock(&stats).received+=packet.frame.payload.len() as u64;
                        }
                    }
                }
            }
            let socket=tokio::select! {
                incoming=input.recv()=>{
                    let message=incoming.context("转发已关闭")?;
                    ensure!(message.frame.kind==1,"目标连接完成前收到数据");
                    return Ok(());
                },
                socket=tokio::time::timeout(Duration::from_secs(10),TcpStream::connect(SocketAddr::new(target.target_host,target.target_port)))=>socket.context("目标连接超时")??,
            };
            socket.set_nodelay(true)?;
            ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
            reply(&receiver.transport,key,protocol,true,"").await?;
            accepted=true;
            tracing::debug!(rule=key.0,stream=key.1,?protocol,"host port forwarding stream opened");
            let first=tokio::time::timeout(TIMEOUT,input.recv()).await.context("转发握手超时")?.context("转发通道已关闭")?;
            if first.frame.kind==1 {return Ok(());}
            ensure!(matches!(first.frame.kind,2|4),"转发握手顺序错误");
            let (mut read,mut write)=socket.into_split();
            let sending=async {
                let mut bytes=vec![0;CHUNK];
                loop {
                    let count=read.read(&mut bytes).await?;
                    if count==0{return Ok::<_,anyhow::Error>(());}
                    ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
                    receiver.transport.send(key.0,key.1,2,Bytes::copy_from_slice(&bytes[..count])).await?;
                    lock(&stats).sent+=count as u64;
                }
            };
            let receiving=async {
                let mut next=Some(first);
                loop {
                    let message=match next.take(){Some(m)=>m,None=>input.recv().await.context("转发通道已关闭")?};
                    if message.frame.kind==1 {return Ok::<_,anyhow::Error>(());}
                    ensure!(receiver.permitted(generation,policy),"端口转发许可已失效");
                    ensure!(matches!(message.frame.kind,2|4),"转发收到重复握手");
                    if message.frame.kind==2 {
                        write.write_all(&message.frame.payload).await?;
                        lock(&stats).received+=message.frame.payload.len() as u64;
                    }
                }
            };
            tokio::select!{result=sending=>result,result=receiving=>result}
        }=>result
    };
    if let Err(error) = &result {
        lock(&stats).error = Some(error.to_string());
        if !accepted && receiver.generation.load(Ordering::Acquire) == generation {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                reply(
                    &receiver.transport,
                    key,
                    protocol,
                    false,
                    &error.to_string(),
                ),
            )
            .await;
        }
    }
    if accepted
        && !remote_fin.load(Ordering::Acquire)
        && receiver.generation.load(Ordering::Acquire) == generation
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            receiver
                .transport
                .send(key.0, key.1, protocol.fin(), Bytes::new()),
        )
        .await;
    }
    if let Some(error) = lock(&fault).take() {
        lock(&stats).error = Some(error);
    }
    tracing::debug!(rule=key.0,stream=key.1,?protocol,accepted,error=?result.err().map(|e|e.to_string()),"host port forwarding stream closed");
    lock(&stats).connections -= 1;
}
