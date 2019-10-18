use tokio_threadpool::blocking;
use tokio::prelude::*;
use std::io::{self,SeekFrom};
use fs2::*;
use std::path::*;
use crate::storage::{layer, Label};
use tokio::fs;

// todo not here
pub fn read_label_file<R: AsyncRead+Send>(r: R, name: &str) -> impl Future<Item=(R,Label),Error=io::Error>+Send {
    let name = name.to_owned();
    tokio::io::read_to_end(r, Vec::new())
        .and_then(move |(r,data)| {
            let s = String::from_utf8_lossy(&data);
            let lines: Vec<&str> = s.lines().collect();
            if lines.len() != 2 {
                let err = io::Error::new(io::ErrorKind::InvalidData, format!("expected label file to have two lines. contents were ({:?})",lines));

                return future::Either::A(future::err(err));
            }
            let version_str = &lines[0];
            let layer_str = &lines[1];

            let version = u64::from_str_radix(version_str,10);
            if version.is_err() {
                let err = io::Error::new(io::ErrorKind::InvalidData, format!("expected first line of label file to be a number but it was {}", version_str));

                return future::Either::A(future::err(err));
            }

            if layer_str.len() == 0 {
                future::Either::A(future::ok((r, Label {
                    name,
                    layer: None,
                    version: version.unwrap()
                })))
            }
            else {
                let layer = layer::string_to_name(layer_str);
                future::Either::B(layer.into_future()
                          .map(move |layer| (r, Label {
                              name,
                              layer: Some(layer),
                              version: version.unwrap()
                         })))
            }

        })
}


pub struct LockedFileLockFuture {
    file: Option<std::fs::File>,
    exclusive: bool
}

impl LockedFileLockFuture {
    fn new_shared(file: std::fs::File) -> Self {
        Self {
            file: Some(file),
            exclusive: false
        }
    }

    fn new_exclusive(file: std::fs::File) -> Self {
        Self {
            file: Some(file),
            exclusive: true
        }
    }
}

impl Future for LockedFileLockFuture {
    type Item = std::fs::File;
    type Error = io::Error;

    fn poll(&mut self) -> Result<Async<std::fs::File>, io::Error> {
        if self.file.is_none() {
            panic!("polled LockedFileLockFuture after completion");
        }

        match blocking(||if self.exclusive {
            self.file.as_ref().unwrap().lock_exclusive().expect("failed to acquire exclusive lock")
        } else {
            self.file.as_ref().unwrap().lock_shared().expect("failed to acquire exclusive lock")
        }) {
            Ok(Async::Ready(_)) => {
                let mut file = None;
                std::mem::swap(&mut file, &mut self.file);
                Ok(Async::Ready(file.unwrap()))
            },
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => panic!("polled LockedFileLockFuture outside of a tokio threadpool context")
        }
    }
}

pub struct LockedFile {
    file: Option<fs::File>
}

impl LockedFile {
    pub fn open<P:'static+AsRef<Path>+Send>(path: P) -> impl Future<Item=Option<Self>,Error=io::Error>+Send {
        fs::OpenOptions::new().read(true).open(path)
            .map(|f| f.into_std())
            .and_then(|f| match f.try_lock_shared() {
                Ok(()) => future::Either::A(future::ok(f)),
                Err(_) => future::Either::B(LockedFileLockFuture::new_shared(f))
            })
            .map(|f| Some(LockedFile { file: Some(fs::File::from_std(f)) }))
            .or_else(|e| match e.kind() {
                io::ErrorKind::NotFound => future::ok(None),
                _ => future::err(e)
            })
    }

    pub fn seek(mut self, pos: SeekFrom) -> impl Future<Item=LockedFile, Error=io::Error> {
        let mut file = None;
        std::mem::swap(&mut file, &mut self.file);
        let file = file.expect("tried to seek in dropped LockedFile");
        file.seek(pos)
            .map(|(file,_)| LockedFile { file: Some(file) })
    }
}

impl Read for LockedFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        self.file.as_mut().expect("tried to read from dropped LockedFile").read(buf)
    }
}

impl AsyncRead for LockedFile {
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let mut file = None;
        std::mem::swap(&mut file, &mut self.file);
        if file.is_some() {
            file.unwrap().into_std().unlock().unwrap();
        }
    }
}

pub struct LockedFileWriter {
    file: Option<fs::File>
}
impl LockedFileWriter {
    pub fn open<P:'static+AsRef<Path>+Send>(path: P) -> impl Future<Item=Self,Error=io::Error> {
        fs::OpenOptions::new().read(true).write(true).open(path)
            .map(|f| f.into_std())
            .and_then(|f| match f.try_lock_exclusive() {
                Ok(()) => Box::new(future::ok(f)) as Box<dyn Future<Item=std::fs::File,Error=io::Error>>,
                Err(_) => Box::new(LockedFileLockFuture::new_exclusive(f))
            })
            .map(|f| LockedFileWriter { file: Some(fs::File::from_std(f)) })
    }

    pub fn write_label(self, label: &Label) -> impl Future<Item=bool, Error=io::Error> {
        let version = label.version;
        let contents = match label.layer {
            None => format!("{}\n\n", label.version).into_bytes(),
            Some(layer) => format!("{}\n{}\n", label.version, layer::name_to_string(layer)).into_bytes()
        };

        read_label_file(self, &label.name)
            .and_then(move |(w,l)| {
                if l.version > version {
                    // someone else updated ahead of us. return false
                    future::Either::A(future::ok(false))
                }
                else if l.version == version {
                    // if version matches exactly, there's no need to do anything but nothing got ahead of us
                    future::Either::A(future::ok(true))
                }
                else {
                    future::Either::B(tokio::io::write_all(w, contents)
                                      .and_then(|(mut w,_)| w.shutdown())
                                      .map(|_| true))
                }
            })
    }
}

impl Read for LockedFileWriter {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        self.file.as_mut().expect("tried to read from dropped LockedFile").read(buf)
    }
}

impl AsyncRead for LockedFileWriter {
}

impl Write for LockedFileWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.file.as_mut().expect("tried to write to dropped LockedFileWriter").write(buf)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.file.as_mut().expect("tried to flush dropped LockedFileWrite").flush()
    }
}

impl AsyncWrite for LockedFileWriter {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.file.as_mut().expect("tried to shutdown dropped LockedFileWriter").shutdown()
    }
}

impl Drop for LockedFileWriter {
    fn drop(&mut self) {
        let mut file = None;
        std::mem::swap(&mut file, &mut self.file);
        if file.is_some() {
            file.unwrap().into_std().unlock().unwrap();
        }
    }
}

pub struct LockedFileAppender {
    file: Option<fs::File>
}
impl LockedFileAppender {
    pub fn open<P:'static+AsRef<Path>+Send>(path: P) -> impl Future<Item=Self,Error=io::Error> {
        fs::OpenOptions::new().append(true).open(path)
            .map(|f| f.into_std())
            .and_then(|f| match f.try_lock_exclusive() {
                Ok(()) => Box::new(future::ok(f)) as Box<dyn Future<Item=std::fs::File,Error=io::Error>>,
                Err(_) => Box::new(LockedFileLockFuture::new_exclusive(f))
            })
            .map(|f| LockedFileAppender { file: Some(fs::File::from_std(f)) })
    }
}

impl Write for LockedFileAppender {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.file.as_mut().expect("tried to write to dropped LockedFileAppender").write(buf)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.file.as_mut().expect("tried to flush dropped LockedFileWrite").flush()
    }
}

impl AsyncWrite for LockedFileAppender {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.file.as_mut().expect("tried to shutdown dropped LockedFileAppender").shutdown()
    }
}

impl Drop for LockedFileAppender {
    fn drop(&mut self) {
        let mut file = None;
        std::mem::swap(&mut file, &mut self.file);
        if file.is_some() {
            file.unwrap().into_std().unlock().unwrap();
        }
    }
}