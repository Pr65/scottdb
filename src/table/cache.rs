use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::ops::Deref;

use lru::LruCache;
use crc::crc32;

use crate::table::sctable::ScTableFile;

use crate::table::tablefmt::{ScTableCatalogItem,
                             TABLE_MIN_SIZE, TABLE_MAGIC_SIZE, TABLE_MAGIC,
                             TABLE_INDEX_SIZE, TABLE_HEAD_SIZE};
use crate::error::Error;
use crate::encode::decode_fixed32;

pub(crate) struct ScTableCache<'a> {
    catalog: Vec<ScTableCatalogItem>,
    data: Vec<u8>,
    quota: CacheQuota<'a>
}

impl<'a> ScTableCache<'a> {
    pub(crate) fn from_raw(raw: &[u8], quota: CacheQuota<'a>) -> Result<ScTableCache<'a>, Error> {
        if raw.len() < TABLE_MIN_SIZE {
            return Err(Error::sc_table_corrupt("too small to be a table file".into()))
        }

        if &raw[raw.len()-TABLE_MAGIC_SIZE .. raw.len()] != TABLE_MAGIC {
            return Err(Error::sc_table_corrupt("incorrect table magic".into()))
        }

        let kv_catalog_size = decode_fixed32(&raw[0..4]) as usize;
        let data_size = decode_fixed32(&raw[4..8]) as usize;

        if kv_catalog_size % TABLE_INDEX_SIZE != 0 {
            return Err(Error::sc_table_corrupt("catalog size should be multiplication of 16".into()))
        }

        if (kv_catalog_size + data_size + TABLE_MIN_SIZE) != raw.len() {
            return Err(Error::sc_table_corrupt("incorrect table size".into()))
        }

        let kv_catalog_crc = decode_fixed32(&raw[8..12]);
        let data_crc = decode_fixed32(&raw[12..16]);

        let kv_catalog = &raw[TABLE_HEAD_SIZE..TABLE_HEAD_SIZE+ kv_catalog_size];
        let data = &raw[TABLE_HEAD_SIZE+ kv_catalog_size..TABLE_HEAD_SIZE+ kv_catalog_size +data_size];

        if crc32::checksum_ieee(kv_catalog) != kv_catalog_crc {
            return Err(Error::sc_table_corrupt("incorrect kv_catalog crc".into()))
        }

        if crc32::checksum_ieee(data) != data_crc {
            return Err(Error::sc_table_corrupt("incorrect data crc".into()))
        }

        let mut catalog_item = Vec::new();
        for i in 0..kv_catalog_size / TABLE_INDEX_SIZE {
            let base = i * TABLE_INDEX_SIZE;
            let index =
                ScTableCatalogItem::deserialize(&kv_catalog[base..base + TABLE_INDEX_SIZE]);
            if (index.key_off + index.key_len) as usize >= data.len()
                || (index.value_off + index.value_len) as usize >= data.len() {
                return Err(Error::sc_table_corrupt("incorrect key/value catalog data".into()))
            }
            catalog_item.push(index)
        }

        Ok(Self { catalog: catalog_item, data: data.to_vec(), quota })
    }
}


pub(crate) struct CacheQuota<'a> {
    cache_manager: &'a TableCacheManager<'a>
}

impl<'a> CacheQuota<'a> {
    fn new(cache_manager: &'a TableCacheManager<'a>) -> Self {
        Self { cache_manager }
    }
}

impl<'a> Drop for CacheQuota<'a> {
    fn drop(&mut self) {
        self.cache_manager.on_cache_released()
    }
}

pub(crate) struct TableCacheManager<'a> {
    lru: Mutex<LruCache<ScTableFile, Arc<ScTableCache<'a>>>>,
    cache_count: usize,
    current_cache_count: AtomicUsize
}

impl<'a> TableCacheManager<'a> {
    pub(crate) fn new(cache_count: usize) -> Self {
        TableCacheManager {
            lru: Mutex::new(LruCache::new(cache_count)),
            cache_count,
            current_cache_count: AtomicUsize::new(0)
        }
    }

    pub(crate) fn allocate_quota(&'a self) -> CacheQuota<'a> {
        while self.current_cache_count.load(Ordering::SeqCst) >= self.cache_count {
        }
        self.current_cache_count.fetch_add(1, Ordering::SeqCst);
        CacheQuota::new(self)
    }

    pub(crate) fn add_cache(&'a self, table_file: ScTableFile, table_cache: ScTableCache<'a>) -> Arc<ScTableCache<'a>> {
        let ret = Arc::new(table_cache);
        self.lru.lock().unwrap().put(table_file, ret.clone());
        ret
    }

    pub(crate) fn get_cache(&'a self, table_file: ScTableFile) -> Option<Arc<ScTableCache<'a>>> {
        self.lru.lock().unwrap().get(&table_file).and_then(|arc| Some(arc.clone()))
    }

    fn on_cache_released(&self) {
        self.current_cache_count.fetch_sub(1, Ordering::SeqCst);
    }
}
