use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// A directory handle used as the authority for later relative opens.
///
/// On Unix every component is opened with `openat(2)` and `O_NOFOLLOW`, so a
/// resource cannot be redirected outside its advertised skill after discovery.
pub(crate) struct SecureDir {
    file: File,
}

impl SecureDir {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        open_root(path).map(|file| Self { file })
    }

    pub(crate) fn open_dir(&self, name: &str) -> io::Result<Self> {
        validate_component(name)?;
        open_relative_dir(&self.file, name).map(|file| Self { file })
    }

    pub(crate) fn open_file(&self, path: &Path) -> io::Result<File> {
        let mut components = path.components().peekable();
        let mut directory = self.file.try_clone()?;
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resource paths must contain only normal relative components",
                ));
            };
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "resource path is not UTF-8")
            })?;
            validate_component(name)?;
            if components.peek().is_some() {
                directory = open_relative_dir(&directory, name)?;
            } else {
                return open_relative_file(&directory, name);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "resource path is empty",
        ))
    }
}

fn validate_component(component: &str) -> io::Result<()> {
    if component.is_empty() || component == "." || component == ".." {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid path component",
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn open_root(path: &Path) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `path` is a valid NUL-terminated string and the returned fd is
    // immediately owned by `File` on success.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `open` returned a fresh, owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn open_relative_dir(parent: &File, name: &str) -> io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
}

#[cfg(unix)]
fn open_relative_file(parent: &File, name: &str) -> io::Result<File> {
    use std::os::fd::AsRawFd;

    let file = open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
    )?;
    // `O_NOFOLLOW` prevents a final symlink; `fstat` also excludes devices,
    // sockets, and directories from skill resources.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and the fd is live.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `fstat` initialized the value.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "resource is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_at(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: both the directory fd and C string are valid for this call.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `openat` returned a fresh, owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(not(unix))]
fn open_root(path: &Path) -> io::Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill root must be a real directory",
        ));
    }
    File::open(path)
}

#[cfg(not(unix))]
fn open_relative_dir(parent: &File, _name: &str) -> io::Result<File> {
    let _ = parent;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure relative directory access is unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn open_relative_file(parent: &File, _name: &str) -> io::Result<File> {
    let _ = parent;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure relative file access is unavailable on this platform",
    ))
}
