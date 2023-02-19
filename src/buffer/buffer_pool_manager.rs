use std::collections::{HashMap, LinkedList};
use std::sync::{RwLock, Arc, Mutex};
use std::vec::{Vec};
use crate::common::{FrameID, PageID, INVALID_PAGE_ID};
use crate::storage::page::page::{Page};
use crate::storage::disk::disk_manager::{DiskManager};

use super::lru_k_replacer::{LRUKReplacer};

struct MetaData {
    pub page_id: PageID,
    pub pin_count: i32,
    pub is_dirty: bool,
}

#[derive(Default)]
struct BPMFields {
    pool_size: usize,
    next_page_id: PageID,

    // log_manager,
    disk_manager: DiskManager,
    page_table: HashMap<PageID, FrameID>,
    meta_data: HashMap<FrameID, MetaData>,
    replacer: LRUKReplacer,
    free_list: LinkedList<FrameID>,
    pages: Vec<Arc<RwLock<Page>>>,
}

pub struct BufferPoolManager {
    pool_size: usize,
    next_page_id: PageID,

    // log_manager,
    disk_manager: DiskManager,
    page_table: HashMap<PageID, FrameID>,
    meta_data: HashMap<FrameID, MetaData>,
    replacer: LRUKReplacer,
    free_list: LinkedList<FrameID>,
    pages: Vec<Page>,

    pages_concurrent: Vec<Arc<RwLock<Page>>>,
    fields: Mutex<BPMFields>,
}

impl BufferPoolManager {
    pub fn new(pool_size: usize, disk_manager: DiskManager, replacer_k: usize) -> Self {
        let mut this = Self {
            pool_size,
            next_page_id: 0,
            disk_manager,
            page_table: HashMap::new(),
            meta_data: HashMap::new(),
            replacer: LRUKReplacer::new(pool_size, replacer_k),
            free_list: LinkedList::new(),
            pages: vec![Page::new(); pool_size],
            pages_concurrent: Vec::new(),
            fields: Mutex::new(BPMFields::default()),
        };
        
        for i in 0..pool_size {
            this.free_list.push_back(i as FrameID);
            this.pages_concurrent.push(Arc::new(RwLock::new(Page::new())));
        }

        return this;
    }

    /**
     * @brief Create a new page in the buffer pool. Set page_id to the new page's id, or nullptr if all frames
     * are currently in use and not evictable (in another word, pinned).
     *
     * You should pick the replacement frame from either the free list or the replacer (always find from the free list
     * first), and then call the AllocatePage() method to get a new page id. If the replacement frame has a dirty page,
     * you should write it back to the disk first. You also need to reset the memory and metadata for the new page.
     *
     * Remember to "Pin" the frame by calling replacer.SetEvictable(frame_id, false)
     * so that the replacer wouldn't evict the frame before the buffer pool manager "Unpin"s it.
     * Also, remember to record the access history of the frame in the replacer for the lru-k algorithm to work.
     *
     * @param[out] page_id id of created page
     * @return nullptr if no new pages could be created, otherwise pointer to new page
     */
    pub fn new_page(&mut self, page_id: &mut PageID) -> Option<&mut Page> {
        let mut frame_id: FrameID = -1;

        if !self.free_list.is_empty() {
            // if free frames exist
            frame_id = *self.free_list.front().unwrap();
            self.free_list.pop_front();
        } else {
            // all frames are occupied, need eviction
            if !self.replacer.evict(&mut frame_id) {
                return None;
            }
            let evicted_page = &self.pages[frame_id as usize];
            if evicted_page.is_dirty {
                self.disk_manager.write_page(evicted_page.page_id, &evicted_page.data);
            }
            self.page_table.remove(&evicted_page.page_id);
        }

        *page_id = self.allocate_page();
        self.page_table.insert(*page_id, frame_id);

        self.replacer.record_access(frame_id);
        self.replacer.set_evictable(frame_id, false);

        self.pages[frame_id as usize].pin_count = 1;
        self.pages[frame_id as usize].page_id = *page_id;

        return Some(&mut self.pages[frame_id as usize]);
    }

    pub fn new_page_concurrent(&mut self, page_id: &mut PageID) -> Option<&mut Page> {
        let mut fields = self.fields.lock().expect("fields lock failed");

        let mut frame_id: FrameID = -1;

        if !fields.free_list.is_empty() {
            // if free frames exist
            frame_id = *fields.free_list.front().unwrap();
            fields.free_list.pop_front();
        } else {
            // all frames are occupied, need eviction
            if !fields.replacer.evict(&mut frame_id) {
                return None;
            }
            let evicted_page_ptr = fields.pages[frame_id as usize].clone();
            if fields.meta_data[&frame_id].is_dirty {
                drop(fields);
                let evicted_page = evicted_page_ptr.read().expect("evicted page rLock failed");
                self.disk_manager.write_page(evicted_page.page_id, &evicted_page.data);
                fields = self.fields.lock().expect("fields lock failed");
            }
            let id = fields.meta_data[&frame_id].page_id;
            fields.page_table.remove(&id);
        }

        // *page_id = self.allocate_page();
        fields.page_table.insert(*page_id, frame_id);

        fields.replacer.record_access(frame_id);
        fields.replacer.set_evictable(frame_id, false);

        fields.meta_data.get_mut(&frame_id).unwrap().pin_count = 1;
        fields.meta_data.get_mut(&frame_id).unwrap().page_id = *page_id;

        return Some(&mut self.pages[frame_id as usize]);
    }

    /**
     * @brief Fetch the requested page from the buffer pool. Return nullptr if page_id needs to be fetched from the disk
     * but all frames are currently in use and not evictable (in another word, pinned).
     *
     * First search for page_id in the buffer pool. If not found, pick a replacement frame from either the free list or
     * the replacer (always find from the free list first), read the page from disk by calling disk_manager_->ReadPage(),
     * and replace the old page in the frame. Similar to NewPage(), if the old page is dirty, you need to write it back
     * to disk and update the metadata of the new page
     *
     * In addition, remember to disable eviction and record the access history of the frame like you did for NewPage().
     *
     * @param page_id id of page to be fetched
     * @return nullptr if page_id cannot be fetched, otherwise pointer to the requested page
     */
    pub fn fetch_page(&mut self, page_id: PageID) -> Option<&mut Page> {
        let mut frame_id: FrameID = -1;

        // if page is already in the buffer pool
        if self.page_table.contains_key(&page_id) {
            frame_id = self.page_table[&page_id];
            self.replacer.record_access(frame_id);
            self.replacer.set_evictable(frame_id, false);
            self.pages[frame_id as usize].pin_count += 1;
            return Some(&mut self.pages[frame_id as usize])
        }

        // page not buffered, need to read page
        if !self.free_list.is_empty() {
            // if free frames exist
            frame_id = *self.free_list.front().unwrap();
            self.free_list.pop_front();
        } else {
            // all frames are occupied, need eviction
            if !self.replacer.evict(&mut frame_id) {
                return None;
            }
            let evicted_page = &self.pages[frame_id as usize];
            if evicted_page.is_dirty {
                self.disk_manager.write_page(evicted_page.page_id, &evicted_page.data);
            }
            self.page_table.remove(&evicted_page.page_id);
        }

        self.disk_manager.read_page(page_id, &mut self.pages[frame_id as usize].data);
        self.page_table.insert(page_id, frame_id);

        self.replacer.record_access(frame_id);
        self.replacer.set_evictable(frame_id, false);

        self.pages[frame_id as usize].pin_count = 1;
        self.pages[frame_id as usize].page_id = page_id;

        return Some(&mut self.pages[frame_id as usize]);
    }

    /**
     * @brief Unpin the target page from the buffer pool. If page_id is not in the buffer pool or its pin count is already
     * 0, return false.
     *
     * Decrement the pin count of a page. If the pin count reaches 0, the frame should be evictable by the replacer.
     * Also, set the dirty flag on the page to indicate if the page was modified.
     *
     * @param page_id id of page to be unpinned
     * @param is_dirty true if the page should be marked as dirty, false otherwise
     * @return false if the page is not in the page table or its pin count is <= 0 before this call, true otherwise
     */
    pub fn unpin_page(&mut self, page_id: PageID, is_dirty: bool) -> bool {
        if !self.page_table.contains_key(&page_id) {
            return false;
        }

        let frame_id: FrameID = self.page_table[&page_id];
        if self.pages[frame_id as usize].pin_count == 0 {
            return false;
        }

        self.pages[frame_id as usize].is_dirty = is_dirty;
        self.pages[frame_id as usize].pin_count -= 1;

        if self.pages[frame_id as usize].pin_count == 0 {
            self.replacer.set_evictable(frame_id, true);
        }

        return true;
    }

    /**
     * @brief Flush the target page to disk.
     *
     * Use the DiskManager::WritePage() method to flush a page to disk, REGARDLESS of the dirty flag.
     * Unset the dirty flag of the page after flushing.
     *
     * @param page_id id of page to be flushed, cannot be INVALID_PAGE_ID
     * @return false if the page could not be found in the page table, true otherwise
     */
    pub fn flush_page(&mut self, page_id: PageID) -> bool {
        if !self.page_table.contains_key(&page_id) {
            return false;
        }

        let frame_id: FrameID = self.page_table[&page_id];
        self.disk_manager.write_page(page_id, &mut self.pages[frame_id as usize].data);
        self.pages[frame_id as usize].is_dirty = false;

        return true;
    }

    /**
     * @brief Flush all the pages in the buffer pool to disk.
     */
    pub fn flush_all_pages(&mut self) {
        for i in 0..self.pool_size {
            let page = &mut self.pages[i];
            if page.page_id != INVALID_PAGE_ID {
                if !self.page_table.contains_key(&page.page_id) {
                    continue;
                }
                let frame_id: FrameID = self.page_table[&page.page_id];
                self.disk_manager.write_page(page.page_id, &mut self.pages[frame_id as usize].data);
                self.pages[frame_id as usize].is_dirty = false;
            }
        }
    }

    /**
     * @brief Delete a page from the buffer pool. If page_id is not in the buffer pool, do nothing and return true. If the
     * page is pinned and cannot be deleted, return false immediately.
     *
     * After deleting the page from the page table, stop tracking the frame in the replacer and add the frame
     * back to the free list. Also, reset the page's memory and metadata. Finally, you should call DeallocatePage() to
     * imitate freeing the page on the disk.
     *
     * @param page_id id of page to be deleted
     * @return false if the page exists but could not be deleted, true if the page didn't exist or deletion succeeded
     */
    pub fn delete_page(&mut self, page_id: PageID) -> bool {
        if !self.page_table.contains_key(&page_id) {
            return false;
        }

        let frame_id: FrameID = self.page_table[&page_id];
        if self.pages[frame_id as usize].pin_count > 0 {
            return false;
        }

        self.page_table.remove(&page_id);
        self.replacer.remove(frame_id);
        self.free_list.push_back(frame_id);
        let page = &mut self.pages[frame_id as usize];
        page.page_id = INVALID_PAGE_ID;
        page.is_dirty = false;
        page.pin_count = 0;
        page.reset_memory();
        self.deallocate_page();

        return true;
    }

    fn allocate_page(&mut self) -> PageID {
        self.next_page_id += 1;
        return self.next_page_id - 1;
    }

    fn deallocate_page(&mut self) {
        
    }
}