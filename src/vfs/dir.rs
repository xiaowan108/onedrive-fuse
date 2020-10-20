use crate::{
    error::{Error, Result},
    vfs::inode,
};
use lru_cache::LruCache;
use onedrive_api::{
    option::ObjectOption, resource::DriveItemField, ItemId, ItemLocation, OneDrive, Tag,
};
use serde::Deserialize;
use sharded_slab::Slab;
use std::{
    collections::HashMap,
    convert::TryFrom,
    ffi::OsString,
    sync::{Arc, Mutex as SyncMutex},
};

#[derive(Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub name: OsString,
    pub is_directory: bool,
}

#[derive(Deserialize)]
pub struct Config {
    lru_cache_size: usize,
}

pub struct DirPool {
    opened_handles: Slab<Arc<DirSnapshot>>,
    /// Inode -> DirSnapshot
    lru_cache: SyncMutex<LruCache<u64, Arc<DirSnapshot>>>,
}

struct DirSnapshot {
    c_tag: Tag,
    entries: Vec<DirEntry>,
    /// name -> index of `entries`
    name_map: HashMap<String, usize>,
}

impl DirPool {
    pub fn new(config: Config) -> Self {
        Self {
            opened_handles: Slab::new(),
            lru_cache: SyncMutex::new(LruCache::new(config.lru_cache_size)),
        }
    }

    fn key_to_fh(key: usize) -> u64 {
        u64::try_from(key).unwrap()
    }

    fn fh_to_key(fh: u64) -> usize {
        usize::try_from(fh).unwrap()
    }

    fn alloc(&self, snapshot: Arc<DirSnapshot>) -> usize {
        self.opened_handles.insert(snapshot).expect("Pool is full")
    }

    pub async fn open(
        &self,
        ino: u64,
        item_id: ItemId,
        inode_pool: &inode::InodePool,
        onedrive: &OneDrive,
    ) -> Result<u64> {
        // Check directory content cache of the given inode.
        if let Some(snapshot) = self.lru_cache.lock().unwrap().get_mut(&ino).cloned() {
            return Ok(Self::key_to_fh(self.alloc(snapshot)));
        }

        log::debug!("open_dir: cache miss");

        // FIXME: Incremental fetching.
        let dir = onedrive
            .get_item_with_option(
                ItemLocation::from_id(&item_id),
                ObjectOption::new()
                    .select(&[
                        // `id` is required, or we'll get 400 Bad Request.
                        DriveItemField::id,
                        DriveItemField::c_tag,
                        DriveItemField::children,
                    ])
                    .expand(
                        DriveItemField::children,
                        // FIXME: Use `DriveItemField`.
                        Some(&[
                            "name",
                            // For InodeAttr.
                            "id",
                            "size",
                            "lastModifiedDateTime",
                            "createdDateTime",
                            "folder",
                        ]),
                    ),
            )
            .await?
            .expect("No If-None-Match");

        let c_tag = dir.c_tag.unwrap();

        let mut entries = Vec::new();
        for item in dir.children.unwrap() {
            let (child_id, child_attr) =
                inode::InodeAttr::parse_drive_item(&item).expect("Invalid DriveItem");
            let ino = inode_pool.touch(child_id).await;
            // FIXME: Cache InodeAttr.
            entries.push(DirEntry {
                ino,
                name: item.name.unwrap().into(),
                is_directory: child_attr.is_directory,
            });
        }

        let name_map = entries
            .iter()
            .enumerate()
            .map(|(idx, ent)| (ent.name.to_str().unwrap().to_owned(), idx))
            .collect();

        let snapshot = Arc::new(DirSnapshot {
            c_tag,
            entries,
            name_map,
        });

        self.lru_cache.lock().unwrap().insert(ino, snapshot.clone());
        Ok(Self::key_to_fh(self.alloc(snapshot)))
    }

    pub fn free(&self, fh: u64) -> Result<()> {
        if self.opened_handles.remove(Self::fh_to_key(fh)) {
            Ok(())
        } else {
            Err(Error::InvalidHandle(fh))
        }
    }

    pub async fn read(&self, fh: u64, offset: u64) -> Result<impl AsRef<[DirEntry]>> {
        let snapshot = self
            .opened_handles
            .get(Self::fh_to_key(fh))
            .ok_or(Error::InvalidHandle(fh))?
            .clone();

        // FIXME: Avoid copy.
        Ok(snapshot.entries[offset as usize..].to_owned())
    }
}