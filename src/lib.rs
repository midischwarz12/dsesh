// SPDX-FileCopyrightText: 2026 midischwarz12
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const DETACH: u8 = 0x1c; // Ctrl-\
const SNAPSHOT_PREFIX: &[u8] = b"\x1b[?1049l\x1b[2J\x1b[H";
const SERVER_THREAD_STACK: usize = 256 * 1024;
const EXIT_DRAIN: Duration = Duration::from_millis(50);
const MAX_FRAME_SIZE: usize = 1024 * 1024;

type Payload = Arc<[u8]>;

struct RetainedScreen {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    cached_snapshot: Option<Payload>,
}

impl RetainedScreen {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
            cached_snapshot: None,
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.cached_snapshot = None;
    }

    fn resize(&mut self, rows: u16, cols: u16) -> bool {
        if self.rows == rows && self.cols == cols {
            return false;
        }
        self.parser.set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.cached_snapshot = None;
        true
    }

    fn snapshot(&mut self) -> Payload {
        if let Some(snapshot) = &self.cached_snapshot {
            return Arc::clone(snapshot);
        }

        let mut bytes = Vec::from(SNAPSHOT_PREFIX);
        bytes.extend_from_slice(&self.parser.screen().contents_formatted());
        let snapshot = Payload::from(bytes);
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }
}

#[derive(Debug)]
struct Cli {
    cols: u16,
    rows: u16,
    command: Commands,
}

#[derive(Debug)]
enum Commands {
    New {
        socket: PathBuf,
        command: Vec<String>,
    },
    Attach {
        socket: PathBuf,
    },
    Run {
        socket: PathBuf,
        command: Vec<String>,
    },
    Server {
        socket: PathBuf,
        command: Vec<String>,
    },
}

#[derive(Debug, Clone)]
enum ServerMessage {
    Snapshot(Payload),
    Output(Payload),
    Detached,
    Close,
    Exit(i32),
}

pub fn run() -> Result<()> {
    let cli = parse_cli(std::env::args().skip(1))?;
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

fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut args = args.into_iter().peekable();
    let mut rows = 24;
    let mut cols = 80;

    while let Some(arg) = args.peek() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                let _ = write_stdout_line(format_args!("dsesh {}", env!("CARGO_PKG_VERSION")));
                std::process::exit(0);
            }
            "--rows" => {
                args.next();
                rows = parse_dimension(args.next(), "--rows")?;
            }
            "--cols" => {
                args.next();
                cols = parse_dimension(args.next(), "--cols")?;
            }
            _ => break,
        }
    }

    let subcommand = args.next().context("missing command")?;
    let command = match subcommand.as_str() {
        "new" => {
            let socket = next_path(&mut args, "new requires a socket path")?;
            let command = command_args(args);
            if command.is_empty() {
                bail!("new requires a command after --");
            }
            Commands::New { socket, command }
        }
        "attach" => {
            let socket = next_path(&mut args, "attach requires a socket path")?;
            ensure_no_extra(args)?;
            Commands::Attach { socket }
        }
        "run" => {
            let socket = next_path(&mut args, "run requires a socket path")?;
            let command = command_args(args);
            Commands::Run { socket, command }
        }
        "server" => {
            let socket = next_path(&mut args, "server requires a socket path")?;
            let command = command_args(args);
            if command.is_empty() {
                bail!("server requires a command after --");
            }
            Commands::Server { socket, command }
        }
        other => bail!("unknown command: {other}"),
    };

    Ok(Cli {
        cols,
        rows,
        command,
    })
}

fn parse_dimension(value: Option<String>, flag: &str) -> Result<u16> {
    value
        .with_context(|| format!("{flag} requires a value"))?
        .parse()
        .with_context(|| format!("invalid {flag} value"))
}

fn next_path(args: &mut impl Iterator<Item = String>, message: &str) -> Result<PathBuf> {
    args.next().map(PathBuf::from).context(message.to_owned())
}

fn command_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut args: Vec<String> = args.into_iter().collect();
    if args.first().is_some_and(|arg| arg == "--") {
        args.remove(0);
    }
    args
}

fn ensure_no_extra(mut args: impl Iterator<Item = String>) -> Result<()> {
    if let Some(arg) = args.next() {
        bail!("unexpected argument: {arg}");
    }
    Ok(())
}

fn print_help() {
    let _ = write_stdout_line(format_args!(
        "Usage: dsesh [OPTIONS] <COMMAND>\n\n\
Commands:\n  \
new <SOCKET> -- <COMMAND> [ARGS...]\n  \
attach <SOCKET>\n  \
run <SOCKET> [-- <COMMAND> [ARGS...]]\n\n\
Options:\n  \
--cols <COLS>  Fallback terminal width [default: 80]\n  \
--rows <ROWS>  Fallback terminal height [default: 24]\n  \
-h, --help     Print help\n  \
-V, --version  Print version"
    ));
}

fn start_server(socket: &Path, rows: u16, cols: u16, command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("missing command");
    }
    if socket.exists() {
        bail!("socket already exists: {}", socket.display());
    }

    let cwd = std::env::current_dir().context("resolve current directory")?;
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
        .env("MALLOC_ARENA_MAX", "1")
        .current_dir(&cwd)
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
    builder.cwd(std::env::current_dir().context("resolve server current directory")?);
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
    let screen = Arc::new(Mutex::new(RetainedScreen::new(rows, cols)));

    {
        let clients = Arc::clone(&clients);
        let screen = Arc::clone(&screen);
        spawn_server_thread("pty-reader", move || {
            let mut buf = [0; 8192];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut screen) = screen.lock() {
                            screen.process(&buf[..n]);
                        }
                        broadcast(&clients, ServerMessage::Output(Payload::from(&buf[..n])));
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;
    }

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind unix socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;

    loop {
        if let Some(status) = child.try_wait().context("poll child process")? {
            let code = status.exit_code().min(i32::MAX as u32) as i32;
            broadcast(&clients, ServerMessage::Exit(code));
            let _ = fs::remove_file(socket);
            thread::sleep(EXIT_DRAIN);
            std::process::exit(code);
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let screen = Arc::clone(&screen);
                let clients = Arc::clone(&clients);
                let pty_writer = Arc::clone(&pty_writer);
                let master = Arc::clone(&master);
                spawn_server_thread("client-reader", move || {
                    let _ = handle_client(stream, screen, clients, pty_writer, master);
                })?;
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
    screen: Arc<Mutex<RetainedScreen>>,
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
    spawn_server_thread("client-writer", move || {
        for message in rx {
            if matches!(message, ServerMessage::Close) {
                break;
            }
            if write_server_frame(&mut writer_stream, &message).is_err() {
                break;
            }
            if matches!(message, ServerMessage::Detached | ServerMessage::Exit(_)) {
                break;
            }
        }
        let _ = writer_stream.shutdown(Shutdown::Both);
    })?;

    let mut reader_stream = stream;
    loop {
        match read_frame_header(&mut reader_stream) {
            Ok(FrameHeader {
                tag: 1,
                payload_len,
            }) => {
                let mut writer = pty_writer
                    .lock()
                    .map_err(|_| anyhow!("pty writer lock poisoned"))?;
                copy_exact(&mut reader_stream, writer.as_mut(), payload_len)
                    .context("write input to pty")?;
                writer.flush().context("flush pty input")?;
            }
            Ok(FrameHeader {
                tag: 2,
                payload_len: 4,
            }) => {
                let (rows, cols) = read_client_resize_payload(&mut reader_stream)?;
                let resized = screen
                    .lock()
                    .map(|mut screen| screen.resize(rows, cols))
                    .unwrap_or(true);
                if resized {
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
            }
            Ok(FrameHeader {
                tag: 3,
                payload_len: 0,
            }) => {
                let _ = tx.send(ServerMessage::Detached);
                break;
            }
            Ok(_) => {
                let _ = tx.send(ServerMessage::Close);
                break;
            }
            Err(_) => {
                let _ = tx.send(ServerMessage::Close);
                break;
            }
        }
    }

    let _ = reader_stream.shutdown(Shutdown::Both);
    Ok(())
}

fn snapshot(screen: &Mutex<RetainedScreen>) -> Result<Payload> {
    Ok(screen
        .lock()
        .map_err(|_| anyhow!("screen lock poisoned"))?
        .snapshot())
}

fn broadcast(clients: &Mutex<Vec<mpsc::Sender<ServerMessage>>>, message: ServerMessage) {
    if let Ok(mut clients) = clients.lock() {
        clients.retain(|client| client.send(message.clone()).is_ok());
    }
}

fn spawn_server_thread(
    name: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("dsesh-{name}"))
        .stack_size(SERVER_THREAD_STACK)
        .spawn(f)
        .with_context(|| format!("spawn {name} thread"))
}

fn attach(socket: &Path, fallback_rows: u16, fallback_cols: u16) -> Result<()> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    let (cols, rows) = terminal_size().unwrap_or((fallback_cols, fallback_rows));
    write_client_resize_frame(&mut stream, rows, cols)?;

    let raw = io::stdin().is_terminal() && io::stdout().is_terminal();
    let guard = if raw { Some(RawMode::enter()?) } else { None };

    let mut input_stream = stream.try_clone().context("clone stream input")?;
    let (detach_tx, detach_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(pos) = buf[..n].iter().position(|byte| *byte == DETACH) {
                        if pos > 0 {
                            let _ = write_client_input_frame(&mut input_stream, &buf[..pos]);
                        }
                        let _ = write_client_detach_frame(&mut input_stream);
                        let _ = detach_tx.send(());
                        break;
                    }
                    if write_client_input_frame(&mut input_stream, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    let output = read_server_output(&mut stream)?;
    let detached = detach_rx.try_recv().is_ok();
    drop(guard);

    if detached || matches!(output, OutputResult::Detached) {
        write_stdout_line(format_args!("[detached - {}]", socket.display()))?;
    } else if matches!(output, OutputResult::Exit(_)) {
        write_stdout_line(format_args!("[EOF - ended session]"))?;
    }

    match output {
        OutputResult::Exit(0) | OutputResult::Disconnected | OutputResult::Detached => Ok(()),
        OutputResult::Exit(code) => std::process::exit(code),
    }
}

enum OutputResult {
    Detached,
    Disconnected,
    Exit(i32),
}

fn read_server_output(stream: &mut UnixStream) -> Result<OutputResult> {
    let mut stdout = io::stdout().lock();
    loop {
        match read_frame_header(stream) {
            Ok(FrameHeader {
                tag: 1 | 2,
                payload_len,
            }) => {
                if copy_exact(stream, &mut stdout, payload_len).is_err() {
                    return Ok(OutputResult::Disconnected);
                }
                if stdout.flush().is_err() {
                    return Ok(OutputResult::Disconnected);
                }
            }
            Ok(FrameHeader {
                tag: 3,
                payload_len: 0,
            }) => return Ok(OutputResult::Detached),
            Ok(FrameHeader {
                tag: 4,
                payload_len: 0,
            }) => return Ok(OutputResult::Disconnected),
            Ok(FrameHeader {
                tag: 5,
                payload_len: 4,
            }) => {
                let mut code = [0; 4];
                stream.read_exact(&mut code).context("read exit status")?;
                return Ok(OutputResult::Exit(i32::from_be_bytes(code)));
            }
            Err(_) => return Ok(OutputResult::Disconnected),
            Ok(_) => return Ok(OutputResult::Disconnected),
        }
    }
}

fn write_client_input_frame(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    write_frame(writer, 1, bytes)
}

fn write_stdout_line(args: fmt::Arguments<'_>) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_fmt(args)
        .and_then(|()| stdout.write_all(b"\n"))
        .or_else(ignore_broken_pipe)
        .context("write stdout")
}

fn ignore_broken_pipe(err: io::Error) -> io::Result<()> {
    if err.kind() == io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(err)
    }
}

fn write_client_resize_frame(writer: &mut impl Write, rows: u16, cols: u16) -> Result<()> {
    let mut payload = [0; 4];
    payload[..2].copy_from_slice(&rows.to_be_bytes());
    payload[2..].copy_from_slice(&cols.to_be_bytes());
    write_frame(writer, 2, &payload)
}

fn write_client_detach_frame(writer: &mut impl Write) -> Result<()> {
    write_frame(writer, 3, &[])
}

fn write_server_frame(writer: &mut impl Write, value: &ServerMessage) -> Result<()> {
    match value {
        ServerMessage::Snapshot(bytes) => write_frame(writer, 1, bytes.as_ref()),
        ServerMessage::Output(bytes) => write_frame(writer, 2, bytes.as_ref()),
        ServerMessage::Detached => write_frame(writer, 3, &[]),
        ServerMessage::Close => write_frame(writer, 4, &[]),
        ServerMessage::Exit(code) => write_frame(writer, 5, &code.to_be_bytes()),
    }
}

fn write_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<()> {
    let body_len = payload.len().checked_add(1).context("frame too large")?;
    if body_len > MAX_FRAME_SIZE {
        bail!("frame too large");
    }
    let len = u32::try_from(body_len).context("frame too large")?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&[tag])?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_client_resize_payload(reader: &mut impl Read) -> Result<(u16, u16)> {
    let mut payload = [0; 4];
    reader
        .read_exact(&mut payload)
        .context("read resize payload")?;
    Ok((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

struct FrameHeader {
    tag: u8,
    payload_len: usize,
}

fn read_frame_header(reader: &mut impl Read) -> Result<FrameHeader> {
    let mut len = [0; 4];
    reader.read_exact(&mut len).context("read frame length")?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        bail!("frame too large");
    }
    let mut tag = [0; 1];
    reader.read_exact(&mut tag).context("read frame tag")?;
    Ok(FrameHeader {
        tag: tag[0],
        payload_len: len - 1,
    })
}

fn copy_exact(
    reader: &mut impl Read,
    writer: &mut (impl Write + ?Sized),
    mut len: usize,
) -> Result<()> {
    let mut buf = [0; 8192];
    while len > 0 {
        let chunk = len.min(buf.len());
        reader
            .read_exact(&mut buf[..chunk])
            .context("read frame payload")?;
        writer
            .write_all(&buf[..chunk])
            .context("write frame payload")?;
        len -= chunk;
    }
    Ok(())
}

struct RawMode {
    fd: i32,
    original: libc::termios,
}

impl RawMode {
    fn enter() -> Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let original = termios(fd).context("read terminal mode")?;
        let mut raw = original;

        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cflag |= libc::CS8;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        set_termios(fd, &raw).context("enable terminal raw mode")?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = set_termios(self.fd, &self.original);
    }
}

fn terminal_size() -> Option<(u16, u16)> {
    let fd = io::stdout().as_raw_fd();
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 && size.ws_row > 0 {
        Some((size.ws_col, size.ws_row))
    } else {
        None
    }
}

fn termios(fd: i32) -> io::Result<libc::termios> {
    let mut termios = std::mem::MaybeUninit::uninit();
    let result = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if result == 0 {
        Ok(unsafe { termios.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_termios(fd: i32, termios: &libc::termios) -> io::Result<()> {
    let result = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, termios) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
