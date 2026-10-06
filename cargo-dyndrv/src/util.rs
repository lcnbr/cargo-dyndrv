use std::{
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use harmonia_store_derivation::placeholder::Placeholder;

pub trait CloneBytes {
    fn clone_bytes(&self) -> bytes::Bytes;
}

pub trait IntoBytes {
    fn into_bytes(self) -> bytes::Bytes;
}

impl IntoBytes for PathBuf {
    fn into_bytes(self) -> bytes::Bytes {
        self.into_os_string().into_vec().into()
    }
}

impl IntoBytes for Placeholder {
    fn into_bytes(self) -> bytes::Bytes {
        self.render().into_bytes()
    }
}

impl<T> CloneBytes for T
where
    T: AsRef<Path>,
{
    fn clone_bytes(&self) -> bytes::Bytes {
        self.as_ref().as_os_str().as_bytes().to_owned().into()
    }
}
