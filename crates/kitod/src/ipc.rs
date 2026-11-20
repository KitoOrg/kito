//! Where a repository's daemon is reached on this machine (ADR 0002): a
//! Unix socket, or on Windows a named pipe. The same lines go through
//! either, and the same key opens them.
//!
//! On Windows a file, `.kito/kitod.pipe`, stands where the socket would: it
//! is there while the daemon serves, and gone once its store is closed,
//! which is what `kito daemon stop` waits for.

#[cfg(windows)]
use kito_core::paths::Canonical as _;
use std::io;
use std::path::{Path, PathBuf};

/// A connection to a daemon, as its clients see it.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Stream = std::fs::File;

/// The name of a repository's pipe: the same for whoever opens the same
/// directory, whatever path they took to it.
#[cfg(windows)]
pub fn pipe_name(root: &Path) -> String {
    let root = root.canonical().unwrap_or_else(|_| root.to_owned());
    let lowered = root.to_string_lossy().to_lowercase();
    format!(r"\\.\pipe\kito-{}", kito_core::Hash::of(lowered.as_bytes()).short())
}

/// Connects to the daemon of the repository in `root`.
#[cfg(unix)]
pub fn connect(root: &Path) -> io::Result<Stream> {
    std::os::unix::net::UnixStream::connect(crate::rpc::socket_path(root))
}

#[cfg(windows)]
pub fn connect(root: &Path) -> io::Result<Stream> {
    // The daemon is there only while its file is.
    if !crate::rpc::socket_path(root).exists() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no daemon"));
    }
    let name = pipe_name(root);
    let mut tries = 0;
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
            Ok(file) => return Ok(file),
            // Every instance busy (ERROR_PIPE_BUSY): the daemon opens the
            // next one at once.
            Err(e) if e.raw_os_error() == Some(231) && tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// What a daemon reads requests from and writes answers to.
pub trait Connection: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Connection for T {}

/// Where a daemon accepts its clients.
pub struct Listener {
    /// The socket, or the file that stands for the pipe.
    pub path: PathBuf,
    #[cfg(unix)]
    socket: tokio::net::UnixListener,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(windows)]
    name: String,
}

impl Listener {
    #[cfg(unix)]
    pub fn bind(root: &Path) -> io::Result<Listener> {
        let path = crate::rpc::socket_path(root);
        let _ = std::fs::remove_file(&path);
        let socket = tokio::net::UnixListener::bind(&path)?;
        Ok(Listener { path, socket })
    }

    #[cfg(windows)]
    pub fn bind(root: &Path) -> io::Result<Listener> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = pipe_name(root);
        // The first instance: another daemon of the same directory makes
        // this fail.
        let next = ServerOptions::new().first_pipe_instance(true).create(&name)?;
        let path = crate::rpc::socket_path(root);
        std::fs::write(&path, &name)?;
        Ok(Listener { path, next, name })
    }

    /// The next client.
    #[cfg(unix)]
    pub async fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
        let (stream, _) = self.socket.accept().await?;
        Ok(Box::new(stream))
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
        use tokio::net::windows::named_pipe::ServerOptions;
        self.next.connect().await?;
        // The connected instance is this client's; the next one waits.
        let fresh = ServerOptions::new().create(&self.name)?;
        Ok(Box::new(std::mem::replace(&mut self.next, fresh)))
    }
}

/// A client of a daemon that serves each in a thread of its own: what it
/// reads from, what it writes to.
pub type Halves = (Box<dyn io::Read + Send>, Box<dyn io::Write + Send>);

/// The clients of a daemon served in threads, one after the other.
pub type Clients = Box<dyn Iterator<Item = io::Result<Halves>>>;

/// The clients of a daemon that serves them in threads (a directory
/// connected to a server), one after the other, for ever. Returns where
/// the daemon is reached, and the clients.
#[cfg(unix)]
pub fn blocking(root: &Path) -> io::Result<(PathBuf, Clients)> {
    let path = crate::rpc::socket_path(root);
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let clients = std::iter::from_fn(move || {
        Some(listener.accept().and_then(|(stream, _)| {
            let read = stream.try_clone()?;
            Ok((Box::new(read) as Box<dyn io::Read + Send>, Box::new(stream) as Box<dyn io::Write + Send>))
        }))
    });
    Ok((path, Box::new(clients)))
}

#[cfg(windows)]
pub fn blocking(root: &Path) -> io::Result<(PathBuf, Clients)> {
    use tokio_util::io::SyncIoBridge;
    // The pipe is served by a runtime of its own; each client is read and
    // written through it from its thread.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
    let handle = runtime.handle().clone();
    let mut listener = {
        let _inside = runtime.enter();
        Listener::bind(root)?
    };
    let path = listener.path.clone();
    let clients = std::iter::from_fn(move || {
        let _keep = &runtime;
        Some(runtime.block_on(listener.accept()).map(|connection| {
            let (read, write) = tokio::io::split(connection);
            (
                Box::new(SyncIoBridge::new_with_handle(read, handle.clone())) as Box<dyn io::Read + Send>,
                Box::new(SyncIoBridge::new_with_handle(write, handle.clone())) as Box<dyn io::Write + Send>,
            )
        }))
    });
    Ok((path, Box::new(clients)))
}
