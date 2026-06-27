use anyhow::{Context, Result, bail};
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

pub struct InstanceLock {
    _socket: UnixDatagram,
}

pub fn acquire() -> Result<InstanceLock> {
    let address = SocketAddr::from_abstract_name(b"rinertia-single-instance")
        .context("could not create rinertia instance-lock address")?;

    let socket = match UnixDatagram::bind_addr(&address) {
        Ok(socket) => socket,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            bail!("another rinertia instance is already running");
        }
        Err(error) => {
            return Err(error).context("could not acquire rinertia instance lock");
        }
    };

    Ok(InstanceLock { _socket: socket })
}
