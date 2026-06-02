use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};

const DETACH: u8 = 0x1c; // Ctrl-\
const SNAPSHOT_PREFIX: &[u8] = b"\x1b[?1049l\x1b[2J\x1b[H";

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, default_value_t = 80)]
    cols: u16,
    #[arg(long, default_value_t = 24)]
    rows: u16,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start a new session and attach to it.
    New {
        socket: PathBuf,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Attach to an existing session.
    Attach { socket: PathBuf },
    /// Attach to an existing socket, or start a new session first.
    Run {
        socket: PathBuf,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(hide = true)]
    Server {
        socket: PathBuf,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum ClientMessage {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ServerMessage {
    Snapshot(Vec<u8>),
    Output(Vec<u8>),
    Exit(i32),
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::New { socket, command } => {
            start_server(&socket, cli.rows, cli.cols, &command)?;
            attach(&socket, cli.rows, cli.cols)
        }
        Commands::Attach { socket } => attach(&socket, cli.rows, cli.cols),
        Commands::Run { socket, command } => {
            if UnixStream::connect(&socket).is_err() {
                start_server(&socket, cli.rows, cli.cols, &command)?;
            }
            attach(&socket, cli.rows, cli.cols)
        }
        Commands::Server { socket, command } => server(&socket, cli.rows, cli.cols, &command),
    }
}

fn start_server(socket: &Path, rows: u16, cols: u16, command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("missing command");
    }
    if socket.exists() {
        bail!("socket already exists: {}", socket.display());
    }

    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut child = Command::new(exe);
    child
        .arg("--rows")
        .arg(rows.to_string())
        .arg("--cols")
        .arg(cols.to_string())
        .arg("server")
        .arg(socket)
        .arg("--")
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    child.spawn().context("spawn dsesh server")?;
    wait_for_socket(socket, Duration::from_secs(5))
}

fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("timed out waiting for server socket: {}", socket.display())
}

fn server(socket: &Path, rows: u16, cols: u16, command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("missing command");
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket parent {}", parent.display()))?;
    }
    if socket.exists() {
        fs::remove_file(socket).with_context(|| format!("remove stale {}", socket.display()))?;
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;

    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    let mut child = pair.slave.spawn_command(builder).context("spawn command")?;
    drop(pair.slave);

    let master = Arc::new(Mutex::new(pair.master));
    let mut pty_reader = master
        .lock()
        .map_err(|_| anyhow!("pty lock poisoned"))?
        .try_clone_reader()
        .context("clone pty reader")?;
    let pty_writer = Arc::new(Mutex::new(
        master
            .lock()
            .map_err(|_| anyhow!("pty lock poisoned"))?
            .take_writer()
            .context("take pty writer")?,
    ));

    let clients: Arc<Mutex<Vec<mpsc::Sender<ServerMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

    {
        let clients = Arc::clone(&clients);
        let screen = Arc::clone(&screen);
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = screen.lock() {
                            parser.process(&buf[..n]);
                        }
                        broadcast(&clients, ServerMessage::Output(buf[..n].to_vec()));
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
    }

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind unix socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;

    let exit_clients = Arc::clone(&clients);
    let exit_socket = socket.to_owned();
    thread::spawn(move || {
        let code = child
            .wait()
            .ok()
            .map(|status| status.exit_code().min(i32::MAX as u32) as i32)
            .unwrap_or(1);
        broadcast(&exit_clients, ServerMessage::Exit(code));
        let _ = fs::remove_file(exit_socket);
        std::process::exit(code);
    });

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let screen = Arc::clone(&screen);
                let clients = Arc::clone(&clients);
                let pty_writer = Arc::clone(&pty_writer);
                let master = Arc::clone(&master);
                thread::spawn(move || {
                    let _ = handle_client(stream, screen, clients, pty_writer, master);
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err).context("accept client"),
        }
    }
}

fn handle_client(
    stream: UnixStream,
    screen: Arc<Mutex<vt100::Parser>>,
    clients: Arc<Mutex<Vec<mpsc::Sender<ServerMessage>>>>,
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    clients
        .lock()
        .map_err(|_| anyhow!("clients lock poisoned"))?
        .push(tx.clone());

    tx.send(ServerMessage::Snapshot(snapshot(&screen)?))
        .context("queue snapshot")?;

    let mut writer_stream = stream.try_clone().context("clone stream writer")?;
    thread::spawn(move || {
        for message in rx {
            if write_frame(&mut writer_stream, &message).is_err() {
                break;
            }
            if matches!(message, ServerMessage::Exit(_)) {
                break;
            }
        }
        let _ = writer_stream.shutdown(Shutdown::Both);
    });

    let mut reader_stream = stream;
    loop {
        match read_frame::<ClientMessage>(&mut reader_stream) {
            Ok(ClientMessage::Input(bytes)) => {
                let mut writer = pty_writer
                    .lock()
                    .map_err(|_| anyhow!("pty writer lock poisoned"))?;
                writer.write_all(&bytes).context("write input to pty")?;
                writer.flush().context("flush pty input")?;
            }
            Ok(ClientMessage::Resize { rows, cols }) => {
                if let Ok(mut parser) = screen.lock() {
                    parser.set_size(rows, cols);
                }
                master
                    .lock()
                    .map_err(|_| anyhow!("pty lock poisoned"))?
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .context("resize pty")?;
            }
            Ok(ClientMessage::Detach) => break,
            Err(_) => break,
        }
    }

    let _ = reader_stream.shutdown(Shutdown::Both);
    Ok(())
}

fn snapshot(screen: &Mutex<vt100::Parser>) -> Result<Vec<u8>> {
    let parser = screen.lock().map_err(|_| anyhow!("screen lock poisoned"))?;
    let mut bytes = Vec::from(SNAPSHOT_PREFIX);
    bytes.extend_from_slice(&parser.screen().contents_formatted());
    Ok(bytes)
}

fn broadcast(clients: &Mutex<Vec<mpsc::Sender<ServerMessage>>>, message: ServerMessage) {
    if let Ok(mut clients) = clients.lock() {
        clients.retain(|client| client.send(message.clone()).is_ok());
    }
}

fn attach(socket: &Path, fallback_rows: u16, fallback_cols: u16) -> Result<()> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    let (cols, rows) = size().unwrap_or((fallback_cols, fallback_rows));
    write_frame(&mut stream, &ClientMessage::Resize { rows, cols })?;

    let raw = io::stdin().is_terminal() && io::stdout().is_terminal();
    let _guard = if raw { Some(RawMode::enter()?) } else { None };

    let mut input_stream = stream.try_clone().context("clone stream input")?;
    let input = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(pos) = buf[..n].iter().position(|byte| *byte == DETACH) {
                        if pos > 0 {
                            let _ = write_frame(
                                &mut input_stream,
                                &ClientMessage::Input(buf[..pos].to_vec()),
                            );
                        }
                        let _ = write_frame(&mut input_stream, &ClientMessage::Detach);
                        break;
                    }
                    if write_frame(&mut input_stream, &ClientMessage::Input(buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    let exit = read_server_output(&mut stream)?;
    let _ = input.join();
    match exit {
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code),
    }
}

fn read_server_output(stream: &mut UnixStream) -> Result<Option<i32>> {
    let mut stdout = io::stdout().lock();
    loop {
        match read_frame::<ServerMessage>(stream) {
            Ok(ServerMessage::Snapshot(bytes) | ServerMessage::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Ok(ServerMessage::Exit(code)) => return Ok(Some(code)),
            Err(_) => return Ok(None),
        }
    }
}

fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value).context("serialize frame")?;
    let len = u32::try_from(body.len()).context("frame too large")?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len).context("read frame length")?;
    let len = u32::from_be_bytes(len) as usize;
    let mut body = vec![0; len];
    reader.read_exact(&mut body).context("read frame body")?;
    serde_json::from_slice(&body).context("decode frame")
}

struct RawMode;

impl RawMode {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
