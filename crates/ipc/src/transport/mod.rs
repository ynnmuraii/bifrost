//! Local transport endpoints, one module per platform.
//!
//! Both platforms expose the same names, so application code is written once:
//!
//! | item | Windows | Linux |
//! |---|---|---|
//! | [`Endpoint`] | a `\\.\pipe\…` name | a socket path |
//! | [`listen`] | creates the pipe with the current user's DACL | binds the socket 0600 in a 0700 directory |
//! | [`GuiListener::accept`] | the DACL already ran in the kernel | `SO_PEERCRED` uid check after `accept` |
//! | [`connect`] | `CreateFile` on the pipe | `connect(2)` on the socket |
//! | [`agent_pair`] | a private single-instance pipe pair | `socketpair(AF_UNIX)` |
//!
//! See the platform module documentation for the exact security properties and
//! for the one documented deviation (the agent pair on Windows).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    agent_pair, connect, gui_socket_path, listen, peer_credentials, Endpoint, GuiListener,
    DIR_MODE, SOCKET_MODE,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    agent_pair, connect, current_user_sid_string, gui_pipe_name, gui_sddl, listen, Endpoint,
    GuiListener, PipeOptions, PipeSecurity, PIPE_NAME_PREFIX, PIPE_PATH_PREFIX,
};

#[cfg(not(any(unix, windows)))]
compile_error!(
    "the ipc crate implements the local transport only for Windows (named pipes) and \
     Unix (Unix sockets), which is what docs/03-architecture.md specifies"
);
