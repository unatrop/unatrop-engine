use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Stdio;
use std::ptr;

use tokio::process::Command;

/// Открытая пара PTY (master + slave) с безопасным владением файловыми дескрипторами
pub struct PtyPair {
    pub master_reader: File,
    pub master_writer: File,
    slave_fd: OwnedFd,
}

/// Создаёт новую пару псевдотерминала (24x120) через `libc::openpty`
pub fn open_pty() -> io::Result<PtyPair> {
    let mut master_raw: libc::c_int = -1;
    let mut slave_raw: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: 24,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let rc = unsafe {
        libc::openpty(
            &mut master_raw,
            &mut slave_raw,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::addr_of_mut!(winsize),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `openpty` при возврате `0` отдаёт два валидных открытых fd.
    let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };
    let slave_fd = unsafe { OwnedFd::from_raw_fd(slave_raw) };

    let master_writer_fd = master_fd.try_clone()?;
    let master_reader = File::from(master_fd);
    let master_writer = File::from(master_writer_fd);

    Ok(PtyPair {
        master_reader,
        master_writer,
        slave_fd,
    })
}

/// Подключает slave-конец PTY к `stdin`/`stdout`/`stderr` дочерней команды
/// и настраивает управляющий терминал (`setsid` + `TIOCSCTTY`) в `pre_exec`.
pub fn configure_pty_command(cmd: &mut Command, pty: PtyPair) -> io::Result<(File, File)> {
    let stdin_fd = pty.slave_fd.try_clone()?;
    let stdout_fd = pty.slave_fd.try_clone()?;
    let stderr_fd = pty.slave_fd;

    cmd.stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::from(stdout_fd))
        .stderr(Stdio::from(stderr_fd));

    // SAFETY: в `pre_exec` вызываем только async-signal-safe `setsid` и `ioctl(TIOCSCTTY)`.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok((pty.master_reader, pty.master_writer))
}
