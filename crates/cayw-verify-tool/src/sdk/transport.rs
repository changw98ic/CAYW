//! Transparent fixture relay retaining actual IPC frames and their request IDs.
use super::Result;
use crate::measurement::{elapsed_ns, sha256};
use cayw_ai::CancellationToken;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

struct Journal {
    file: File,
    sequence: u64,
    started: Instant,
}
impl Journal {
    fn frame(&mut self, connection: u64, direction: &str, bytes: &[u8]) -> io::Result<()> {
        self.sequence += 1;
        let value = json!({"sequence":self.sequence,"elapsed_ns":elapsed_ns(self.started),"connection":connection,
            "direction":direction,"bytes":bytes.len(),"sha256":sha256(bytes),"utf8":String::from_utf8_lossy(bytes),
            "frame":serde_json::from_slice::<Value>(bytes).ok()});
        serde_json::to_writer(&mut self.file, &value)?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()
    }
}
pub struct RecordingProxy {
    pub address: SocketAddr,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
}
impl RecordingProxy {
    pub async fn start(upstream: SocketAddr, path: &Path) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let journal = Arc::new(Mutex::new(Journal {
            file: OpenOptions::new().write(true).create_new(true).open(path)?,
            sequence: 0,
            started: Instant::now(),
        }));
        let stop = CancellationToken::new();
        let cancellation = stop.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut id = 0;
            loop {
                tokio::select! {
                    biased;
                    ()=cancellation.cancelled()=>break,
                    result=connections.join_next(),if !connections.is_empty()=> {result.expect("pending") .map_err(io::Error::other)??;},
                    accepted=listener.accept()=> {
                        let (downstream,_)=accepted?;id+=1;let connection=id;let journal=journal.clone();let stop=cancellation.clone();
                        connections.spawn(async move {
                            let upstream=TcpStream::connect(upstream).await?;
                            let (incoming,down_out)=downstream.into_split();let (up_in,outgoing)=upstream.into_split();
                            let result=tokio::select! {
                                ()=stop.cancelled()=>Ok(()),
                                result=relay(incoming,outgoing,connection,"client_to_server",journal.clone())=>result,
                                result=relay(up_in,down_out,connection,"server_to_client",journal.clone())=>result,
                            };
                            let terminal=serde_json::to_vec(&json!({"result":result.as_ref().map_err(ToString::to_string)}))?;
                            journal.lock().unwrap_or_else(|error|error.into_inner()).frame(connection,"relay_terminal",&terminal)?;
                            Ok::<(),io::Error>(())
                        });
                    }
                }
            }
            while let Some(result) = connections.join_next().await {
                result.map_err(io::Error::other)??;
            }
            Ok(())
        });
        Ok(Self {
            address,
            stop,
            task: Some(task),
        })
    }
    pub async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(super::DEADLINE, task).await???;
        }
        Ok(())
    }
}
impl Drop for RecordingProxy {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
async fn relay<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    mut writer: W,
    connection: u64,
    direction: &str,
    journal: Arc<Mutex<Journal>>,
) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut frame = Vec::new();
    loop {
        frame.clear();
        let count = reader.read_until(b'\n', &mut frame).await?;
        if count == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        if count > super::scenarios::ipc_config().max_frame_bytes + 1 {
            return Err(io::Error::other("fixture relay frame limit"));
        }
        journal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .frame(connection, direction, &frame)?;
        writer.write_all(&frame).await?;
    }
}
