#[macro_use]
extern crate wascc_codec as codec;

#[macro_use]
extern crate log;

use chunks::Chunks;
use codec::blobstore::*;
use codec::capabilities::{CapabilityProvider, Dispatcher, NullDispatcher};
use codec::core::{OP_BIND_ACTOR, OP_REMOVE_ACTOR};
use codec::{deserialize, serialize};
use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use wascc_codec::core::CapabilityConfiguration;

mod chunks;

#[cfg(not(feature = "static_plugin"))]
capability_provider!(FileSystemProvider, FileSystemProvider::new);

const CAPABILITY_ID: &str = "wascc:blobstore";
const SYSTEM_ACTOR: &str = "system";
const FIRST_SEQ_NBR: u64 = 1;

pub struct FileSystemProvider {
    dispatcher: Arc<RwLock<Box<dyn Dispatcher>>>,
    rootdir: RwLock<PathBuf>,
    upload_chunks: RwLock<HashMap<String, (u64, Vec<FileChunk>)>>,
}

impl Default for FileSystemProvider {
    fn default() -> Self {
        let _ = env_logger::builder().format_module_path(false).try_init();

        FileSystemProvider {
            dispatcher: Arc::new(RwLock::new(Box::new(NullDispatcher::new()))),
            rootdir: RwLock::new(PathBuf::new()),
            upload_chunks: RwLock::new(HashMap::new()),
        }
    }
}

impl FileSystemProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn configure(&self, config: CapabilityConfiguration) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut lock = self.rootdir.write().unwrap();
        let root_dir = config.values["ROOT"].clone();
        info!("File System Blob Store Container Root: '{}'", root_dir);
        *lock = PathBuf::from(root_dir);

        Ok(vec![])
    }

    fn create_container(
        &self,
        _actor: &str,
        container: Container,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let container = sanitize_container(&container);
        let cdir = self.container_to_path(&container);
        std::fs::create_dir_all(cdir)?;
        Ok(serialize(&container)?)
    }

    fn remove_container(
        &self,
        _actor: &str,
        container: Container,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let container = sanitize_container(&container);
        let cdir = self.container_to_path(&container);
        std::fs::remove_dir(cdir)?;
        Ok(vec![])
    }

    fn start_upload(&self, _actor: &str, blob: FileChunk) -> Result<Vec<u8>, Box<dyn Error>> {
        let blob = Blob {
            byte_size: 0,
            id: blob.id,
            container: blob.container,
        };
        let blob = sanitize_blob(&blob);
        info!("Starting upload: {}/{}", blob.container, blob.id);
        let bfile = self.blob_to_path(&blob);
        std::fs::write(bfile, &[])?;
        Ok(vec![])
    }

    fn remove_object(&self, _actor: &str, blob: Blob) -> Result<Vec<u8>, Box<dyn Error>> {
        let blob = sanitize_blob(&blob);
        let bfile = self.blob_to_path(&blob);
        std::fs::remove_file(&bfile)?;
        Ok(vec![])
    }

    fn get_object_info(&self, _actor: &str, blob: Blob) -> Result<Vec<u8>, Box<dyn Error>> {
        let blob = sanitize_blob(&blob);
        let bfile = self.blob_to_path(&blob);
        let blob: Blob = if bfile.exists() {
            Blob {
                id: blob.id,
                container: blob.container,
                byte_size: bfile.metadata().unwrap().len(),
            }
        } else {
            Blob {
                id: "none".to_string(),
                container: "none".to_string(),
                byte_size: 0,
            }
        };
        Ok(serialize(&blob)?)
    }

    fn list_objects(&self, _actor: &str, container: Container) -> Result<Vec<u8>, Box<dyn Error>> {
        let container = sanitize_container(&container);
        let cpath = self.container_to_path(&container);
        let (blobs, _errors): (Vec<_>, Vec<_>) = std::fs::read_dir(&cpath)?
            .map(|e| {
                e.map(|e| Blob {
                    id: e.file_name().into_string().unwrap(),
                    container: container.id.to_string(),
                    byte_size: e.metadata().unwrap().len(),
                })
            })
            .partition(Result::is_ok);
        let blobs = blobs.into_iter().map(Result::unwrap).collect();
        let bloblist = BlobList { blobs };
        Ok(serialize(&bloblist)?)
    }

    fn upload_chunk(&self, actor: &str, chunk: FileChunk) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut upload_chunks = self.upload_chunks.write().unwrap();
        let key = actor.to_string() + &sanitize_id(&chunk.container) + &sanitize_id(&chunk.id);
        let total_chunk_count = chunk.total_bytes / chunk.chunk_size;

        let (expected_sequence_no, chunks) = upload_chunks
            .entry(key.clone())
            .or_insert((FIRST_SEQ_NBR, vec![]));
        chunks.push(chunk);

        while let Some(i) = chunks
            .iter()
            .position(|fc| fc.sequence_no == *expected_sequence_no)
        {
            let chunk = chunks.get(i).unwrap();
            let bpath = Path::join(
                &Path::join(&self.rootdir.read().unwrap(), sanitize_id(&chunk.container)),
                sanitize_id(&chunk.id),
            );
            let mut file = OpenOptions::new().create(false).append(true).open(bpath)?;
            info!(
                "Receiving file chunk: {} for {}/{}",
                chunk.sequence_no, chunk.container, chunk.id
            );

            let count = file.write(chunk.chunk_bytes.as_ref())?;
            if count != chunk.chunk_bytes.len() {
                let msg = format!(
                    "Failed to fully write chunk: {} of {} bytes",
                    count,
                    chunk.chunk_bytes.len()
                );
                error!("{}", &msg);
                return Err(msg.into());
            }

            chunks.remove(i);
            *expected_sequence_no += 1;
        }

        if *expected_sequence_no - 1 == total_chunk_count {
            upload_chunks.remove(&key);
        }

        Ok(vec![])
    }

    fn start_download(
        &self,
        actor: &str,
        request: StreamRequest,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        info!("Received request to start download : {:?}", request);
        let actor = actor.to_string();
        let bpath = Path::join(
            &Path::join(
                &self.rootdir.read().unwrap(),
                sanitize_id(&request.container),
            ),
            sanitize_id(&request.id),
        );
        let byte_size = &bpath.metadata()?.len();
        let bfile = std::fs::File::open(bpath)?;
        let chunk_size = if request.chunk_size == 0 {
            chunks::DEFAULT_CHUNK_SIZE
        } else {
            request.chunk_size as usize
        };
        let xfer = Transfer {
            blob_id: sanitize_id(&request.id),
            container: sanitize_id(&request.container),
            total_size: *byte_size,
            chunk_size: chunk_size as _,
            total_chunks: *byte_size / chunk_size as u64,
        };
        let iter = Chunks::new(bfile, chunk_size);
        let d = self.dispatcher.clone();
        std::thread::spawn(move || {
            iter.enumerate().for_each(|(i, chunk)| {
                dispatch_chunk(&xfer, &actor, i, d.clone(), chunk);
            });
        });

        Ok(vec![])
    }

    fn blob_to_path(&self, blob: &Blob) -> PathBuf {
        let cdir = Path::join(&self.rootdir.read().unwrap(), blob.container.to_string());
        Path::join(&cdir, blob.id.to_string())
    }

    fn container_to_path(&self, container: &Container) -> PathBuf {
        Path::join(&self.rootdir.read().unwrap(), container.id.to_string())
    }
}
fn sanitize_container(container: &Container) -> Container {
    Container {
        id: sanitize_id(&container.id),
    }
}
fn sanitize_blob(blob: &Blob) -> Blob {
    Blob {
        id: sanitize_id(&blob.id),
        byte_size: blob.byte_size,
        container: sanitize_id(&blob.container),
    }
}

fn sanitize_id(id: &str) -> String {
    let bad_prefixes: &[_] = &['/', '.'];
    let s = id.trim_start_matches(bad_prefixes);
    let s = s.replace("..", "");
    s.replace("/", "_")
}

fn dispatch_chunk(
    xfer: &Transfer,
    actor: &str,
    i: usize,
    d: Arc<RwLock<Box<dyn Dispatcher>>>,
    chunk: Result<Vec<u8>, std::io::Error>,
) {
    if let Ok(chunk) = chunk {
        let fc = FileChunk {
            sequence_no: i as u64,
            container: xfer.container.to_string(),
            id: xfer.blob_id.to_string(),
            chunk_bytes: chunk,
            chunk_size: xfer.chunk_size,
            total_bytes: xfer.total_size,
        };
        let buf = serialize(&fc).unwrap();
        let _ = d.read().unwrap().dispatch(actor, OP_RECEIVE_CHUNK, &buf);
    }
}

impl CapabilityProvider for FileSystemProvider {
    fn capability_id(&self) -> &'static str {
        CAPABILITY_ID
    }

    // Invoked by the runtime host to give this provider plugin the ability to communicate
    // with actors
    fn configure_dispatch(&self, dispatcher: Box<dyn Dispatcher>) -> Result<(), Box<dyn Error>> {
        trace!("Dispatcher received.");
        let mut lock = self.dispatcher.write().unwrap();
        *lock = dispatcher;

        Ok(())
    }

    fn name(&self) -> &'static str {
        "waSCC Blob Store Provider (File System)"
    }

    // Invoked by host runtime to allow an actor to make use of the capability
    // All providers MUST handle the "configure" message, even if no work will be done
    fn handle_call(&self, actor: &str, op: &str, msg: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        trace!("Received host call from {}, operation - {}", actor, op);

        match op {
            OP_BIND_ACTOR if actor == SYSTEM_ACTOR => self.configure(deserialize(msg)?),
            OP_REMOVE_ACTOR if actor == SYSTEM_ACTOR => Ok(vec![]),
            OP_CREATE_CONTAINER => self.create_container(actor, deserialize(msg)?),
            OP_REMOVE_CONTAINER => self.remove_container(actor, deserialize(msg)?),
            OP_REMOVE_OBJECT => self.remove_object(actor, deserialize(msg)?),
            OP_LIST_OBJECTS => self.list_objects(actor, deserialize(msg)?),
            OP_UPLOAD_CHUNK => self.upload_chunk(actor, deserialize(msg)?),
            OP_START_DOWNLOAD => self.start_download(actor, deserialize(msg)?),
            OP_START_UPLOAD => self.start_upload(actor, deserialize(msg)?),
            OP_GET_OBJECT_INFO => self.get_object_info(actor, deserialize(msg)?),
            _ => Err("bad dispatch".into()),
        }
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::{sanitize_blob, sanitize_container};
    use crate::FileSystemProvider;
    use codec::blobstore::{Blob, Container};
    use std::collections::HashMap;
    use std::env::temp_dir;
    use std::fs::File;
    use std::io::{BufReader, Read};
    use std::path::{Path, PathBuf};
    use wascc_codec::blobstore::FileChunk;
    use wascc_codec::core::CapabilityConfiguration;

    #[test]
    fn no_hacky_hacky() {
        let container = Container {
            id: "/etc/h4x0rd".to_string(),
        };
        let blob = Blob {
            byte_size: 0,
            id: "../passwd".to_string(),
            container: "/etc/h4x0rd".to_string(),
        };
        let c = sanitize_container(&container);
        let b = sanitize_blob(&blob);

        // the resulting tricksy blob should end up in ${ROOT}/etc_h4x0rd/passwd and
        // thereby not expose anything sensitive
        assert_eq!(c.id, "etc_h4x0rd");
        assert_eq!(b.id, "passwd");
        assert_eq!(b.container, "etc_h4x0rd");
    }

    #[test]
    fn test_start_upload() {
        let actor = "actor1";
        let container = "container".to_string();
        let id = "blob".to_string();

        let fs = FileSystemProvider::new();
        let root_dir = setup_test_start_upload(&fs);
        let upload_dir = Path::join(&root_dir, &container);
        let bpath = create_dir(&upload_dir, &id);

        let total_bytes = 5;
        let chunk_size = 2;

        let chunk1 = FileChunk {
            sequence_no: 1,
            container: container.clone(),
            id: id.clone(),
            total_bytes,
            chunk_size,
            chunk_bytes: vec![1, 1],
        };
        let chunk2 = FileChunk {
            sequence_no: 2,
            container: container.clone(),
            id: id.clone(),
            total_bytes,
            chunk_size,
            chunk_bytes: vec![2, 2],
        };
        let chunk3 = FileChunk {
            sequence_no: 3,
            container: container.clone(),
            id: id.clone(),
            total_bytes,
            chunk_size,
            chunk_bytes: vec![3],
        };
        let chunk3_dup = FileChunk {
            sequence_no: 3,
            container: container.clone(),
            id: id.clone(),
            total_bytes,
            chunk_size,
            chunk_bytes: vec![3],
        };

        assert!(fs.upload_chunk(actor, chunk3).is_ok());
        assert!(fs.upload_chunk(actor, chunk2).is_ok());
        assert!(fs.upload_chunk(actor, chunk1).is_ok());
        assert!(fs.upload_chunk(actor, chunk3_dup).is_ok());

        // check file contents
        let mut reader = BufReader::new(File::open(&bpath).unwrap());
        let mut buffer = [0; 5];

        teardown_test_start_upload(&bpath, &upload_dir);

        assert!(reader.read(&mut buffer).is_ok());
        assert_eq!(vec![1, 1, 2, 2, 3], buffer);
        // the last duplicate is not cleaned up because it can't tell the
        // difference between a late duplicate chunk and an early out of order chunk
        assert_eq!(1, fs.upload_chunks.read().unwrap().len());
    }

    #[allow(dead_code)]
    fn setup_test_start_upload(fs: &FileSystemProvider) -> PathBuf {
        let mut config = HashMap::new();
        let root_dir = temp_dir();

        config.insert("ROOT".to_string(), String::from(root_dir.to_str().unwrap()));
        fs.configure(CapabilityConfiguration {
            module: "test_start_upload-module".to_string(),
            values: config,
        })
        .unwrap();

        root_dir
    }

    #[allow(dead_code)]
    fn teardown_test_start_upload(file: &PathBuf, upload_dir: &PathBuf) {
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir_all(upload_dir).unwrap();
    }

    #[allow(dead_code)]
    fn create_dir(dir: &PathBuf, id: &String) -> PathBuf {
        let bpath = Path::join(&dir, &id);
        let _res = std::fs::create_dir(&dir);
        drop(File::create(&bpath).unwrap());
        bpath
    }
}
