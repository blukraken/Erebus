//! Named-pipe server. One pending instance is always waiting for the next client.

use std::convert::Infallible;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pinnos_protocol::{
    read_frame, write_frame, CoreMsg, FaceMsg, HEARTBEAT_MS, MAX_FRAME, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval_at, sleep, Instant, MissedTickBehavior};

use crate::hub::{Event, Out};
use crate::log::{log_error, log_info, log_warn};

/// Creates the first pipe instance. Fails if another core already owns the pipe.
pub fn bind(name: &str) -> io::Result<NamedPipeServer> {
    ServerOptions::new().first_pipe_instance(true).create(name)
}

/// ERROR_ACCESS_DENIED (first-instance clash) or ERROR_PIPE_BUSY.
pub fn is_already_running(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5 | 231))
}

pub async fn serve(
    first: NamedPipeServer,
    name: &str,
    started_ms: u64,
    hub: UnboundedSender<Event>,
) -> Infallible {
    let mut pending = first;
    let mut next_id = 1u64;
    loop {
        if let Err(e) = pending.connect().await {
            log_error!("accept failed: {e}");
            pending = recreate(name).await;
            continue;
        }
        let id = next_id;
        next_id += 1;
        match ServerOptions::new().create(name) {
            Ok(next) => {
                let connected = std::mem::replace(&mut pending, next);
                tokio::spawn(client(connected, id, started_ms, hub.clone()));
            }
            Err(e) => {
                log_error!("create pipe instance failed: {e}");
                tokio::spawn(client(pending, id, started_ms, hub.clone()));
                pending = recreate(name).await;
            }
        }
    }
}

/// Retries once a second until a pending instance exists again.
async fn recreate(name: &str) -> NamedPipeServer {
    loop {
        sleep(Duration::from_secs(1)).await;
        match ServerOptions::new().create(name) {
            Ok(p) => return p,
            Err(e) => log_error!("create pipe instance failed: {e}"),
        }
    }
}

enum End {
    Closed,
    VersionMismatch(u32),
    Failed(io::Error),
}

async fn client(pipe: NamedPipeServer, id: u64, started_ms: u64, hub: UnboundedSender<Event>) {
    log_info!("client {id} connected");
    let (rd, wr) = tokio::io::split(pipe);
    let (out_tx, out_rx) = unbounded_channel();
    let writer = tokio::spawn(write_loop(wr, out_rx, started_ms));
    let end = read_faces(rd, id, out_tx, &hub).await;
    let _ = hub.send(Event::Down(id));
    writer.abort();
    let _ = writer.await;
    match end {
        End::Closed => log_info!("client {id} disconnected"),
        End::VersionMismatch(v) => {
            log_warn!("client {id} speaks protocol {v}, core speaks {PROTOCOL_VERSION}; closing")
        }
        End::Failed(e) => log_warn!("client {id} dropped: {e}"),
    }
}

/// Registers the client with the hub on its first valid `Hello`.
async fn read_faces(
    mut rd: ReadHalf<NamedPipeServer>,
    id: u64,
    out: UnboundedSender<Out>,
    hub: &UnboundedSender<Event>,
) -> End {
    let mut out = Some(out);
    loop {
        match read_msg(&mut rd).await {
            Ok(FaceMsg::Hello { protocol, face_version, pid }) => {
                if protocol == PROTOCOL_VERSION {
                    if let Some(out) = out.take() {
                        let _ = hub.send(Event::Up(id, pid, out));
                    }
                }
                log_info!("client {id} hello: face {face_version} pid {pid} protocol {protocol}");
                if protocol != PROTOCOL_VERSION {
                    return End::VersionMismatch(protocol);
                }
            }
            Ok(FaceMsg::Hidden) => log_info!("client {id} window hidden"),
            Ok(FaceMsg::Shown) => log_info!("client {id} window shown"),
            Err(e) if is_disconnect(&e) => return End::Closed,
            Err(e) => return End::Failed(e),
        }
    }
}

fn is_disconnect(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(e.kind(), UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted)
}

/// Reads the length and body asynchronously, then decodes with the protocol's sync reader.
async fn read_msg(rd: &mut ReadHalf<NamedPipeServer>) -> io::Result<FaceMsg> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len);
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; 4 + n as usize];
    buf[..4].copy_from_slice(&len);
    rd.read_exact(&mut buf[4..]).await?;
    read_frame(&mut buf.as_slice())
}

/// Sends `Hello`, then a heartbeat every period and any message the hub queues in between.
async fn write_loop(
    mut wr: impl AsyncWrite + Unpin,
    mut queue: UnboundedReceiver<Out>,
    started_ms: u64,
) -> io::Result<()> {
    send(
        &mut wr,
        &CoreMsg::Hello {
            protocol: PROTOCOL_VERSION,
            core_version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            started_unix_ms: started_ms,
        },
    )
    .await?;

    let period = Duration::from_millis(HEARTBEAT_MS);
    let mut tick = interval_at(Instant::now() + period, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut seq = 0u64;
    let mut open = true;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                seq += 1;
                send(&mut wr, &CoreMsg::Heartbeat { seq, unix_ms: now_ms() }).await?;
            }
            queued = queue.recv(), if open => match queued {
                Some(Out { msg, done }) => {
                    send(&mut wr, &msg).await?;
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                }
                None => open = false,
            },
        }
    }
}

async fn send(wr: &mut (impl AsyncWrite + Unpin), msg: &CoreMsg) -> io::Result<()> {
    let mut buf = Vec::with_capacity(128);
    write_frame(&mut buf, msg)?;
    wr.write_all(&buf).await
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
