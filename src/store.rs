use super::address::*;
use super::chunk_storage;
use super::crypto;
use super::fsutil;
use super::hex;
use super::hydrogen;
use failure::Fail;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Fail)]
pub enum StoreError {
    #[fail(display = "path {} already exists, refusing to overwrite it", path)]
    AlreadyExists { path: String },
    #[fail(display = "the store was not initialized properly")]
    NotInitializedProperly,
    #[fail(display = "the store was does not exist")]
    StoreDoesNotExist,
    #[fail(display = "sqlite error while manipulating the database: {}", err)]
    SqliteError { err: rusqlite::Error },
    #[fail(display = "archivist database at unsupported version")]
    UnsupportedSchemaVersion,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum StorageEngineSpec {
    Local,
}

pub struct Store {
    _store_path: PathBuf,
    _gc_lock: FileLock,
    storage_engine: Box<dyn chunk_storage::Engine>,
    pub gc_generation: String,
}

struct FileLock {
    f: fs::File,
}

impl FileLock {
    fn get_exclusive(p: &Path) -> Result<FileLock, std::io::Error> {
        let f = fs::File::open(p)?;
        f.lock_exclusive()?;
        Ok(FileLock { f })
    }

    fn get_shared(p: &Path) -> Result<FileLock, std::io::Error> {
        let f = fs::File::open(p)?;
        f.lock_shared()?;
        Ok(FileLock { f })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.f.unlock();
    }
}

fn new_gc_generation() -> String {
    let mut gen: [u8; 32] = [0; 32];
    hydrogen::random_buf(&mut gen);
    hex::easy_encode_to_string(&gen)
}

impl Store {
    fn ensure_file_exists(p: &Path) -> Result<(), failure::Error> {
        if p.exists() {
            Ok(())
        } else {
            Err(StoreError::NotInitializedProperly.into())
        }
    }

    fn check_store_sane(store_path: &Path) -> Result<(), failure::Error> {
        if !store_path.exists() {
            return Err(StoreError::StoreDoesNotExist.into());
        }
        let mut path_buf = PathBuf::from(store_path);
        path_buf.push("data");
        Store::ensure_file_exists(&path_buf.as_path())?;
        path_buf.pop();
        path_buf.push("archivist.db");
        Store::ensure_file_exists(&path_buf.as_path())?;
        path_buf.pop();
        Ok(())
    }

    pub fn init(store_path: &Path, engine: StorageEngineSpec) -> Result<(), failure::Error> {
        let parent = if store_path.is_absolute() {
            store_path.parent().unwrap().to_owned()
        } else {
            let abs = std::env::current_dir()?.join(store_path);
            let parent = abs.parent().unwrap();
            parent.to_owned()
        };

        let mut path_buf = PathBuf::from(&parent);
        if store_path.exists() {
            return Err(StoreError::AlreadyExists {
                path: store_path.to_string_lossy().to_string(),
            }
            .into());
        }
        let mut tmpname = store_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new(""))
            .to_os_string();
        tmpname.push(".archivist-store-init-tmp");
        path_buf.push(&tmpname);
        if path_buf.exists() {
            return Err(StoreError::AlreadyExists {
                path: path_buf.to_string_lossy().to_string(),
            }
            .into());
        }
        fs::DirBuilder::new().create(path_buf.as_path())?;
        path_buf.push("data");
        fs::DirBuilder::new().create(path_buf.as_path())?;
        path_buf.pop();

        path_buf.push("gc.lock");
        fsutil::create_empty_file(path_buf.as_path())?;
        path_buf.pop();

        let mut conn = Store::open_db(&path_buf)?;

        conn.query_row("pragma journal_mode=WAL;", rusqlite::NO_PARAMS, |_r| Ok(()))?;
        let tx = conn.transaction()?;

        tx.execute(
            "create table ArchivistMeta(Key, Value, UNIQUE(key, value));",
            rusqlite::NO_PARAMS,
        )?;
        tx.execute(
            "insert into ArchivistMeta(Key, Value) values(?, ?);",
            rusqlite::params!["schema-version", 0],
        )?;
        tx.execute(
            "insert into ArchivistMeta(Key, Value) values(?, ?);",
            rusqlite::params!["gc-generation", new_gc_generation()],
        )?;
        tx.execute(
            "insert into ArchivistMeta(Key, Value) values(?, ?);",
            rusqlite::params!["storage-engine", serde_json::to_string(&engine)?],
        )?;
        tx.execute(
            "create table Items(Address, MasterKeyID, EncryptionHeader);",
            rusqlite::NO_PARAMS,
        )?;

        tx.commit()?;
        drop(conn);

        fsutil::sync_dir(&path_buf)?;
        std::fs::rename(&path_buf, store_path)?;
        Ok(())
    }

    fn gc_lock_path(store_path: &Path) -> PathBuf {
        let mut lock_path = store_path.to_path_buf();
        lock_path.push("gc.lock");
        lock_path
    }

    fn open_db(store_path: &Path) -> rusqlite::Result<rusqlite::Connection> {
        let mut db_path = store_path.to_path_buf();
        db_path.push("archivist.db");
        let conn = rusqlite::Connection::open(db_path)?;
        conn.query_row("pragma busy_timeout=3600000;", rusqlite::NO_PARAMS, |_r| {
            Ok(())
        })?;
        Ok(conn)
    }

    pub fn open(store_path: &Path) -> Result<Store, failure::Error> {
        Store::check_store_sane(&store_path)?;

        let gc_lock = FileLock::get_shared(&Store::gc_lock_path(&store_path))?;

        let conn = Store::open_db(store_path)?;
        let v: i32 = conn.query_row(
            "select value from ArchivistMeta where Key='schema-version';",
            rusqlite::NO_PARAMS,
            |row| row.get(0),
        )?;
        if v != 0 {
            return Err(StoreError::UnsupportedSchemaVersion.into());
        }

        let engine_meta: String = conn.query_row(
            "select value from ArchivistMeta where Key='storage-engine';",
            rusqlite::NO_PARAMS,
            |row| row.get(0),
        )?;

        let gc_generation: String = conn.query_row(
            "select value from ArchivistMeta where Key='gc-generation';",
            rusqlite::NO_PARAMS,
            |row| row.get(0),
        )?;

        let spec: StorageEngineSpec = serde_json::from_str(&engine_meta)?;

        let storage_engine: Box<dyn chunk_storage::Engine> = match spec {
            StorageEngineSpec::Local => {
                let mut data_dir = store_path.to_path_buf();
                data_dir.push("data");

                // XXX fixme, how many workers do we want?
                // configurable?
                Box::new(chunk_storage::LocalStorage::new(&data_dir, 4))
            }
        };

        Ok(Store {
            _store_path: store_path.to_path_buf(),
            _gc_lock: gc_lock,
            gc_generation,
            storage_engine,
        })
    }

    pub fn add_item(
        &mut self,
        addr: Address,
        hdr: crypto::VersionedEncryptionHeader,
    ) -> Result<(), failure::Error> {
        let mut conn = Store::open_db(&self._store_path)?;
        let tx = conn.transaction()?;

        tx.execute(
            "insert into Items(Address, MasterKeyID, EncryptionHeader) values(?, ?, ?);",
            &[
                format!("{}", addr),
                hex::easy_encode_to_string(&hdr.master_key_id()),
                serde_json::to_string(&hdr)?,
            ],
        )?;

        tx.commit()?;
        drop(conn);
        Ok(())
    }

    pub fn add_chunk(&mut self, addr: Address, buf: Vec<u8>) -> Result<(), failure::Error> {
        self.storage_engine.add_chunk(addr, buf)
    }

    pub fn get_chunk(&mut self, addr: Address) -> Result<Vec<u8>, failure::Error> {
        self.storage_engine.get_chunk(addr)
    }

    pub fn sync(&mut self) -> Result<(), failure::Error> {
        self.storage_engine.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_get_chunk() {
        let tmp_dir = tempdir::TempDir::new("store_test_repo").unwrap();
        let mut path_buf = PathBuf::from(tmp_dir.path());
        path_buf.push("store");
        Store::init(path_buf.as_path(), StorageEngineSpec::Local).unwrap();
        let mut store = Store::open(path_buf.as_path()).unwrap();
        let addr = Address::default();
        store.add_chunk(addr, vec![1]).unwrap();
        store.sync().unwrap();
        store.add_chunk(addr, vec![2]).unwrap();
        store.sync().unwrap();
        let v = store.get_chunk(addr).unwrap();
        assert_eq!(v, vec![1]);
    }
}