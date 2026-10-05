//! UDP association per local source endpoint. One DATA message is one datagram.
//! This deliberately retains SCTP reliability; it is not an IP/UDP VPN.
use super::*;
use tokio::net::UdpSocket;

pub(super) async fn run_rule(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    stats: Arc<Mutex<RuleStatus>>,
) {
    let result=async {
        rule.validate()?;
        let socket=Arc::new(UdpSocket::bind(SocketAddr::new(rule.local_addr,rule.local_port)).await.context("无法监听 UDP 端口")?);
        lock(&stats).listening=true;
        let mut flows=HashMap::<SocketAddr,mpsc::Sender<Bytes>>::new();
        let mut jobs=JoinSet::new();
        let mut buffer=vec![0;65_536];
        let outcome=loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break Ok::<_,anyhow::Error>(()),
                Some(result)=jobs.join_next(), if !jobs.is_empty()=>{if let Ok(addr)=result {flows.remove(&addr);}},
                packet=socket.recv_from(&mut buffer)=>{
                    let (size,source)=match packet {Ok(v)=>v,Err(e) if matches!(e.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::ConnectionRefused)=>continue,Err(e)=>break Err(e.into())};
                    if size>65_507 {continue;}
                    if !flows.contains_key(&source) {
                        if flows.len()>=64 {continue;}
                        let (tx,rx)=mpsc::channel(8);
                        flows.insert(source,tx);
                        let transport=transport.clone();let rule=rule.clone();let socket=socket.clone();
                        let stop=stop.clone();let stats=stats.clone();
                        jobs.spawn(async move {association(rule,transport,socket,source,rx,stop,stats).await;source});
                    }
                    // UDP overload drops a datagram, never truncates or splices it.
                    let _=flows[&source].try_send(Bytes::copy_from_slice(&buffer[..size]));
                }
            }
        };
        stop.cancel();
        drop(socket);
        flows.clear();
        while jobs.join_next().await.is_some() {}
        outcome
    }.await;
    let mut state = lock(&stats);
    state.listening = false;
    if let Err(error) = result {
        state.error = Some(error.to_string());
    }
}

async fn association(
    rule: Rule,
    transport: Arc<Transport>,
    socket: Arc<UdpSocket>,
    source: SocketAddr,
    mut packets: mpsc::Receiver<Bytes>,
    parent: CancellationToken,
    stats: Arc<Mutex<RuleStatus>>,
) {
    let stream = transport
        .serial
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    if stream == 0 {
        return;
    }
    let stop = parent.child_token();
    let (tx, mut incoming) = mpsc::channel(64);
    let remote_fin = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fault = Arc::new(Mutex::new(None));
    if let Err(error) = transport.register(
        (rule.id, stream),
        Route {
            sender: tx,
            budget: Arc::new(Semaphore::new(1024 * 1024)),
            stop: stop.clone(),
            remote_fin: remote_fin.clone(),
            fault: fault.clone(),
        },
    ) {
        lock(&stats).error = Some(error.to_string());
        return;
    }
    lock(&stats).connections += 1;
    let result = tokio::select! {
        biased;
        _=stop.cancelled()=>Ok(()),
        result=async {
            transport.handshake(&rule,stream,&mut incoming).await.context("UDP 需要支持此功能的 OpenUUYC 被控端")?;
            lock(&stats).error=None;
            loop {
                tokio::select! {
                    _=tokio::time::sleep(Duration::from_secs(60))=>return Ok(()),
                    packet=packets.recv()=>{
                        let packet=packet.context("UDP 监听已关闭")?;
                        let size=packet.len();
                        transport.send(rule.id,stream,18,packet).await?;
                        lock(&stats).sent+=size as u64;
                    },
                    message=incoming.recv()=>{
                        let message=message.context("UDP 通道已关闭")?;
                        if message.frame.kind==19 {return Ok(());}
                        ensure!(message.frame.kind==18 && message.frame.payload.len()<=65_507,"无效 UDP 数据报");
                        socket.send_to(&message.frame.payload,source).await?;
                        lock(&stats).received+=message.frame.payload.len() as u64;
                    }
                }
            }
        }=>result
    };
    lock(&transport.routes).remove(&(rule.id, stream));
    if !remote_fin.load(Ordering::Acquire) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            transport.send(rule.id, stream, 19, Bytes::new()),
        )
        .await;
    }
    let mut state = lock(&stats);
    state.connections = state.connections.saturating_sub(1);
    if !parent.is_cancelled() {
        if let Err(error) = result {
            state.error = Some(format!("{error:#}"));
        }
    }
    if let Some(error) = lock(&fault).take() {
        state.error = Some(error);
    }
}
