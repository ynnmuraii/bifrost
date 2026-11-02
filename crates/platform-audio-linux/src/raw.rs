use audio_backend::BackendError;
use pulseaudio::{protocol as p, Client};
use std::{
    ffi::CString,
    future::Future,
    io::{Cursor, Read},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};
use zeroize::Zeroizing;
pub(crate) const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONTROL_FRAME: usize = 1024 * 1024;
pub(crate) struct Connection {
    pub client: Client,
    socket: UnixStream,
}
impl Connection {
    pub fn close(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}
pub(crate) struct Config {
    path: PathBuf,
    cookie: Zeroizing<Vec<u8>>,
}
impl Config {
    pub fn from_env() -> Result<Self, BackendError> {
        let path = match std::env::var_os("PULSE_SERVER") {
            Some(server) => local_server(server.to_str().ok_or(BackendError::ServerUnavailable)?)?,
            None => pulseaudio::socket_path_from_env().ok_or(BackendError::ServerUnavailable)?,
        };
        if !path.is_absolute() {
            return Err(BackendError::ServerUnavailable);
        }
        let cookie = match pulseaudio::cookie_path_from_env() {
            Some(path) => {
                let mut file =
                    std::fs::File::open(path).map_err(|_| BackendError::PermissionDenied)?;
                let mut bytes = Zeroizing::new(Vec::new());
                file.by_ref()
                    .take(257)
                    .read_to_end(&mut bytes)
                    .map_err(|_| BackendError::PermissionDenied)?;
                if bytes.len() > 256 {
                    return Err(BackendError::PermissionDenied);
                }
                bytes
            }
            None => Zeroizing::new(Vec::new()),
        };
        Ok(Self { path, cookie })
    }
    fn socket(&self) -> Result<UnixStream, BackendError> {
        let socket =
            UnixStream::connect(&self.path).map_err(|_| BackendError::ServerUnavailable)?;
        socket
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|_| BackendError::ServerUnavailable)?;
        socket
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|_| BackendError::ServerUnavailable)?;
        Ok(socket)
    }
    pub fn client(&self) -> Result<Connection, BackendError> {
        let socket = self.socket()?;
        let control = socket
            .try_clone()
            .map_err(|_| BackendError::ServerUnavailable)?;
        let client = Client::new_unix(c"Bifrost audio", socket, Some(self.cookie.as_slice()))
            .map_err(client_error)?;
        Ok(Connection {
            client,
            socket: control,
        })
    }
    pub fn raw(&self) -> Result<Raw, BackendError> {
        let mut raw = Raw {
            socket: self.socket()?,
            version: p::MAX_VERSION,
            next: 0,
        };
        let reply: p::AuthReply = raw.request(p::Command::Auth(p::AuthParams {
            version: p::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie: self.cookie.to_vec(),
        }))?;
        raw.version = reply.version.min(p::MAX_VERSION);
        let mut props = p::Props::new();
        props.set(p::Prop::ApplicationName, c"Bifrost audio control");
        let _: p::SetClientNameReply = raw.request(p::Command::SetClientName(props))?;
        Ok(raw)
    }
}
fn local_server(server: &str) -> Result<PathBuf, BackendError> {
    let path = server.strip_prefix("unix:").unwrap_or(server);
    if !path.starts_with('/') || path.chars().any(char::is_whitespace) || path.contains('\0') {
        return Err(BackendError::ServerUnavailable);
    }
    Ok(PathBuf::from(path))
}
pub(crate) fn object_serial(props: &p::Props) -> Option<u64> {
    let bytes = props.get_bytes(c"object.serial")?.strip_suffix(&[0])?;
    let serial = std::str::from_utf8(bytes).ok()?.parse::<u64>().ok()?;
    (serial != 0).then_some(serial)
}
pub(crate) fn client_error(error: pulseaudio::ClientError) -> BackendError {
    match error {
        pulseaudio::ClientError::ServerError(p::PulseError::NoEntity) => BackendError::DeviceGone,
        pulseaudio::ClientError::ServerError(p::PulseError::AccessDenied) => {
            BackendError::PermissionDenied
        }
        _ => BackendError::ServerUnavailable,
    }
}
pub(crate) fn wait<T>(
    future: impl Future<Output = Result<T, pulseaudio::ClientError>>,
) -> Result<T, BackendError> {
    wait_for(future, TIMEOUT)
}
pub(crate) fn wait_for<T>(
    future: impl Future<Output = Result<T, pulseaudio::ClientError>>,
    timeout: Duration,
) -> Result<T, BackendError> {
    futures::executor::block_on(async {
        futures::pin_mut!(future);
        let delay = futures_timer::Delay::new(timeout);
        futures::pin_mut!(delay);
        match futures::future::select(future, delay).await {
            futures::future::Either::Left((result, _)) => result.map_err(client_error),
            futures::future::Either::Right(_) => Err(BackendError::Timeout),
        }
    })
}
pub(crate) struct Raw {
    pub socket: UnixStream,
    pub version: u16,
    next: u32,
}
impl Raw {
    fn send(&mut self, command: &p::Command) -> Result<u32, BackendError> {
        let seq = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(BackendError::ServerUnavailable)?;
        p::write_command_message(&mut self.socket, seq, command, self.version)
            .map_err(|_| BackendError::ServerUnavailable)?;
        Ok(seq)
    }
    pub fn request<T: p::CommandReply>(&mut self, command: p::Command) -> Result<T, BackendError> {
        let seq = self.send(&command)?;
        let frame = read_frame(&mut self.socket)?;
        let (received, reply) = p::read_reply_message::<T>(&mut Cursor::new(frame), self.version)
            .map_err(|_| BackendError::ServerUnavailable)?;
        if received != seq {
            return Err(BackendError::ServerUnavailable);
        }
        Ok(reply)
    }
    pub fn ack(&mut self, command: p::Command) -> Result<(), BackendError> {
        let seq = self.send(&command)?;
        let frame = read_frame(&mut self.socket)?;
        let received = p::read_ack_message(&mut Cursor::new(frame))
            .map_err(|_| BackendError::ServerUnavailable)?;
        if received != seq {
            return Err(BackendError::ServerUnavailable);
        }
        Ok(())
    }
    pub fn subscribe(&mut self) -> Result<(), BackendError> {
        self.ack(p::Command::Subscribe(
            p::SubscriptionMask::SINK | p::SubscriptionMask::SOURCE | p::SubscriptionMask::SERVER,
        ))?;
        self.socket
            .set_read_timeout(None)
            .map_err(|_| BackendError::ServerUnavailable)
    }
    pub fn event(&mut self) -> Result<p::SubscriptionEvent, BackendError> {
        let frame = read_frame(&mut self.socket)?;
        let (_, command) = p::read_command_message(&mut Cursor::new(frame), self.version)
            .map_err(|_| BackendError::ServerUnavailable)?;
        match command {
            p::Command::SubscribeEvent(event) => Ok(event),
            _ => Err(BackendError::ServerUnavailable),
        }
    }
}
fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, BackendError> {
    let mut descriptor = [0; 20];
    reader
        .read_exact(&mut descriptor)
        .map_err(|_| BackendError::ServerUnavailable)?;
    let size = u32::from_be_bytes(descriptor[..4].try_into().expect("four bytes")) as usize;
    if size > MAX_CONTROL_FRAME || descriptor[4..8] != u32::MAX.to_be_bytes() {
        return Err(BackendError::ServerUnavailable);
    }
    let mut frame = vec![0; 20 + size];
    frame[..20].copy_from_slice(&descriptor);
    reader
        .read_exact(&mut frame[20..])
        .map_err(|_| BackendError::ServerUnavailable)?;
    Ok(frame)
}
pub(crate) fn name(value: &str) -> Result<CString, BackendError> {
    CString::new(value).map_err(|_| BackendError::InvalidArgument)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_and_ambiguous_servers_are_refused_without_fallback() {
        for server in [
            "tcp:192.168.1.2",
            "192.168.1.2",
            "unix:relative",
            "unix:/tmp/one unix:/tmp/two",
            "",
        ] {
            assert_eq!(local_server(server), Err(BackendError::ServerUnavailable));
        }
        assert_eq!(
            local_server("unix:/tmp/pulse-test/native").unwrap(),
            PathBuf::from("/tmp/pulse-test/native")
        );
        assert!(local_server("/tmp/pulse-test/native").is_ok());
    }
    #[test]
    fn oversized_control_frames_are_refused_before_reading_payload() {
        let mut descriptor = [0; 20];
        descriptor[..4].copy_from_slice(&((MAX_CONTROL_FRAME + 1) as u32).to_be_bytes());
        descriptor[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            read_frame(&mut Cursor::new(descriptor)),
            Err(BackendError::ServerUnavailable)
        );
    }
}
