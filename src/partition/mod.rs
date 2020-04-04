use std::collections::BTreeMap;
use std::sync::{Mutex, RwLock, atomic::AtomicU64, Condvar};
use std::marker::PhantomData;
use std::cmp::Ordering;
use std::ptr::NonNull;
use std::ops::Deref;

use crate::{Comparator, Options, DefaultComparator};
use crate::table::{Table, tablefmt::{TABLE_CATALOG_ITEM_SIZE, TABLE_MIN_SIZE}};
use crate::table::cache::TableCacheManager;
use crate::io::IOManager;
use crate::error::Error;
use std_semaphore::Semaphore;

pub(crate) enum UserKey<Comp: Comparator> {
    Owned(Vec<u8>, PhantomData<Comp>),
    Borrow(NonNull<[u8]>)
}

impl<Comp: Comparator> Clone for UserKey<Comp> {
    fn clone(&self) -> Self {
        match self {
            UserKey::Owned(data, _) => UserKey::Owned(data.clone(), PhantomData),
            UserKey::Borrow(ptr) => UserKey::Borrow(ptr.clone())
        }
    }
}

impl<Comp: Comparator> UserKey<Comp> {
    pub(crate) fn new_owned(vec: Vec<u8>) -> Self {
        UserKey::Owned(vec, PhantomData)
    }

    pub(crate) fn new_borrow(slice: &[u8]) -> Self {
        UserKey::Borrow(unsafe { NonNull::new_unchecked(slice as *const [u8] as _) })
    }

    fn key(&self) -> &[u8]{
        match self {
            UserKey::Owned(k, _) => k.as_slice(),
            UserKey::Borrow(b) => unsafe { b.as_ref() }
        }
    }

    fn is_owned(&self) -> bool {
        if let UserKey::Owned(_, _) = self {
            true
        } else {
            false
        }
    }
}

impl<Comp: Comparator> Ord for UserKey<Comp> {
    fn cmp(&self, other: &Self) -> Ordering {
        Comp::compare(&self.key(), &other.key())
    }
}

impl<Comp: Comparator> PartialOrd for UserKey<Comp> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<Comp: Comparator> PartialEq for UserKey<Comp> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<Comp: Comparator> Eq for UserKey<Comp> {}

type DefaultUserKey = UserKey<DefaultComparator>;

pub(crate) struct InternalKey<Comp: Comparator> {
    seq: u64,
    pub(crate) user_key: UserKey<Comp>
}

impl<Comp: Comparator> InternalKey<Comp> {
    pub(crate) fn new(seq: u64, user_key: UserKey<Comp>) -> Self {
        Self { seq, user_key }
    }
}

impl<Comp: Comparator> Ord for InternalKey<Comp> {
    fn cmp(&self, other: &Self) -> Ordering {
        let ord =  self.seq.cmp(&other.seq);
        if ord == Ordering::Equal {
            self.user_key.cmp(&other.user_key)
        } else {
            ord
        }
    }
}

impl<Comp: Comparator> PartialOrd for InternalKey<Comp> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<Comp: Comparator> PartialEq for InternalKey<Comp> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<Comp: Comparator> Eq for InternalKey<Comp> {}

type MemTable<Comp> = BTreeMap<InternalKey<Comp>, Vec<u8>>;

type Level<Comp> = Vec<Box<dyn Table<Comp>>>;

pub(crate) struct Partition<'a, Comp: Comparator> {
    concrete: RwLock<PartitionImpl<'a, Comp>>,

    seq: &'a AtomicU64,
    cache_manager: &'a TableCacheManager,
    io_manager: &'a IOManager,
    options: &'a Options
}

impl<'a, Comp: Comparator> Partition<'a, Comp> {
    fn new(options: &'a Options,
           seq: &'a AtomicU64,
           cache_manager: &'a TableCacheManager,
           io_manager: &'a IOManager) -> Self {
        Self {
            concrete: RwLock::new(PartitionImpl::new(options)),
            seq,
            cache_manager,
            io_manager,
            options
        }
    }
}

impl<'a, Comp: Comparator> PartialOrd for Partition<'a, Comp> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let g1 = self.concrete.read().unwrap();
        let g2 = other.concrete.read().unwrap();
        let (self_lower, self_upper) = g1.bounds();
        let (other_lower, other_upper) = g2.bounds();

        if self_upper.unwrap().cmp(&other_lower.unwrap()) == Ordering::Less {
            return Some(Ordering::Less)
        } else if self_lower.unwrap().cmp(&other_upper.unwrap()) == Ordering::Greater {
            return Some(Ordering::Greater)
        } else {
            None
        }
    }
}

impl<'a, Comp: Comparator> Ord for Partition<'a, Comp> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl<'a, Comp: Comparator> PartialEq for Partition<'a, Comp> {
    fn eq(&self, other: &Self) -> bool {
        debug_assert!(Self::debug_never_eq_sanity_check(self, other));
        false
    }
}

impl<'a, Comp: Comparator> Partition<'a, Comp> {
    fn debug_never_eq_sanity_check(&self, other: &Self) -> bool {
        if self as *const Self == other as *const Self {
            return false;
        }

        let g1 = self.concrete.read().unwrap();
        let g2 = other.concrete.read().unwrap();
        let (self_lower, self_upper) = g1.bounds();
        let (other_lower, other_upper) = g2.bounds();
        if self_lower.is_some() && self_upper.is_some()
           && other_lower.is_some() && other_upper.is_some()
           && (self_lower.unwrap().cmp(other_lower.unwrap()) == Ordering::Equal
               || self_upper.unwrap().cmp(other_upper.unwrap()) == Ordering::Equal) {
            return false;
        }

        true
    }
}

impl<'a, Comp: Comparator> Eq for Partition<'a, Comp> {}

pub(crate) struct PartitionImpl<'a, Comp: Comparator> {
    mem_table: MemTable<Comp>,
    mem_table_data_size: usize,

    imm_table: Option<MemTable<Comp>>,
    levels: Vec<Level<Comp>>,

    lower_bound: Option<UserKey<Comp>>,
    upper_bound: Option<UserKey<Comp>>,

    sem: Semaphore,

    options: &'a Options
}

impl<'a, Comp: Comparator> PartitionImpl<'a, Comp> {
    fn new(options: &'a Options) -> Self {
        Self {
            mem_table: MemTable::new(),
            mem_table_data_size: 0,
            imm_table: None,
            levels: Vec::new(),
            lower_bound: None,
            upper_bound: None,
            sem: Semaphore,
            options
        }
    }

    fn get(&self, key: InternalKey<Comp>) -> Option<Vec<u8>> {
        if let Some(v) = self.mem_table.get(&key) {
            return Some(v.clone());
        }
        if let Some(imm) = &self.imm_table {
            if let Some(v) = imm.get(&key) {
                return Some(v.clone());
            }
        }
        unimplemented!()
    }

    fn put(&mut self, key: InternalKey<Comp>, value: Vec<u8>) {
        debug_assert!(key.user_key.is_owned());

        let kv_size = key.user_key.key().len() + value.len() + TABLE_CATALOG_ITEM_SIZE;
        if self.memtable_size() + kv_size > self.options.table_size {
            let guard = self.sem.access();
            assert!(self.imm_table.is_none());

            // TODO MakeRoomForWrite
            // TODO schedule the compaction, requires a `BackgroundTaskManager`.
        }

        if self.lower_bound == None && self.upper_bound == None {
            self.lower_bound.replace(key.user_key.clone());
            self.upper_bound.replace(key.user_key.clone());
        }
        self.mem_table.insert(key, value);
    }

    fn memtable_size(&self) -> usize {
        self.mem_table_data_size + self.mem_table.len() * TABLE_CATALOG_ITEM_SIZE + TABLE_MIN_SIZE
    }

    fn bounds(&self) -> (Option<&UserKey<Comp>>, Option<&UserKey<Comp>>) {
        (self.lower_bound.as_ref(), self.upper_bound.as_ref())
    }

    fn set_lower_bound(&mut self, lower_bound: UserKey<Comp>) {
        debug_assert!(lower_bound.is_owned());
        self.lower_bound.replace(lower_bound);
    }

    fn set_upper_bound(&mut self, upper_bound: UserKey<Comp>) {
        debug_assert!(upper_bound.is_owned());
        self.upper_bound.replace(upper_bound);
    }

    fn debug_bounds_sanity_check(&self) -> bool {
        self.lower_bound.is_some() == self.upper_bound.is_some()
    }
}
