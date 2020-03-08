use crate::buffer;
use crate::data_type::{EntryT, ValueT, TOMBSTONE};
use crate::level;
use crate::level::Level;
use crate::merge;
use crate::run;
use bit_vec::Iter;
use rand::distributions::weighted::WeightedError::TooMany;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ptr::null;
use std::sync::{Arc, Mutex};
use threadpool::ThreadPool;

pub struct LSMTree {
    //TODO need threadpool, multiple-levels, in-memory buffer
    levels: Vec<level::Level>,
    buffer: buffer::Buffer,
    worker_pool: threadpool::ThreadPool,
    bf_bits_per_entry: u64, //used for bloom filter initialization
}

impl LSMTree {
    pub fn new() {}

    pub fn get_run(&self, run_id: usize) -> Option<run::Run> {
        let mut index = run_id;
        for level in self.levels.iter() {
            if run_id < level.runs.len() {
                level.runs.get(index).unwrap();
            } else {
                index -= level.runs.len();
            }
        }
        None
    }

    //compact level i data to level i+1
    pub fn merge_down(&mut self, current: usize) {
        let mut merge_ctx: merge::MergeContextT = merge::MergeContextT::new();
        let mut entry: EntryT;
        let mut next: usize;
        //assert!(current >= self.levels.iter());
        if self.levels[current].remaining() > 0 {
            //no need for compaction and merge down
            return;
        } else if current == self.levels.len() - 1 {
            //can not merge down anymore
            println!("No more space in tree");
            return;
        } else {
            next = current + 1;
        }

        /*
         * If the next level does not have space for the current level,
         * recursively merge the next level downwards to create some
         */
        if (self.levels[next].remaining() == 0) {
            self.merge_down(next);
            //ensure that after merge down, level next has free space now.
            assert!(self.levels[next].remaining() > 0)
        }

        /*
         * Merge all runs in the current level into the first
         * run in the next level
         */
        for mut run in self.levels[current].runs {
            //add all entries in current levels for merging
            merge_ctx.add(run.map_read_default(), run.size as usize);
        }

        self.levels[next].runs.push_front(run::Run::new(
            self.levels[next].max_run_size as u64,
            self.bf_bits_per_entry,
        ));
        //start writing back this compacted run in next level to a new file on disk
        self.levels[next].runs[0].map_write();
        while !merge_ctx.done() {
            entry = merge_ctx.next();
            if (!(next == self.levels.len() - 1 && entry.value == TOMBSTONE.as_bytes().to_vec())) {
                self.levels[next].runs[0].put(&entry.key, &entry.value);
            }
        }
        self.levels[next].runs[0].unmap();
        //finish writing back for compacted run

        //unmap the old runs and clear these files
        for mut run in self.levels[current].runs {
            run.unmap();
        }
        self.levels[current].runs.clear();
    }

    pub fn put(&mut self, entry: EntryT) -> bool {
        //TODO entry must be fixed size for easier put implementation.
        if self.buffer.full() == false {
            //put to buffer success
            self.buffer.put(entry.key, entry.value);
            return true;
        } else {
            /*
             * If the buffer is full, flush level 0 if necessary
             * to create space
             */

            //self.merge_down(&self.levels[0]);
            //self.merge_down();

            /*
             * Flush the buffer to level 0.
             */
            let size = self.levels[0].max_run_size as u64;
            self.levels[0]
                .runs
                .push_front(run::Run::new(size, self.bf_bits_per_entry));
            self.levels[0].runs[0].map_write();

            for entry_in_buf in self.buffer.entries.iter() {
                self.levels[0].runs[0].put(&entry_in_buf.key, &entry_in_buf.value);
            }
            self.levels[0].runs[0].unmap();

            //buffer already written to levels.front().runs.front(). We can clear it now for inserting new entry.
            self.buffer.empty();
            self.buffer.put(entry.key, entry.value);
            true
        }
    }

    pub fn get(&self, key: &Vec<u8>) -> Option<ValueT> {
        //read from buffer first. then from level 0 to max_level. return first match entry.
        let mut latest_val: ValueT = ValueT::new();
        let mut latest_run: i32 = -1;
        let counter = Arc::new(Mutex::new(0)); //TODO counter should be atomic<usize> according to c++ codebase.
        match self.buffer.get(key) {
            Some(v) => {
                //found in buffer, return the result;
                if v != TOMBSTONE.as_bytes().to_vec() {
                    return Some(v);
                } else {
                    return None;
                }
            }
            _ => {
                //not found in buffer, start searching in vector<Level>

                for current_run in 0..10 {
                    let current_val: ValueT;
                    //let mut run: run::Run;
                    if latest_run >= 0 || (self.get_run(current_run).is_none()) {
                        // Stop search if we discovered a key in another run, or
                        // if there are no more runs to search
                        //TODO how to terminate this task thread here?
                    } else {
                        let run = self.get_run(current_run).unwrap();
                        if run.get(key).is_none() {
                            // Couldn't find the key in the current run, so we need
                            // to keep searching.
                            //search(); //TODO how to call this task again??? in c++ codebase, the search is task abstraction for threadpool to execute
                        } else {
                            // Update val if the run is more recent than the
                            // last, then stop searching since there's no need
                            // to search later runs.
                            current_val = run.get(key).unwrap();
                            if latest_run < 0 || current_run < latest_run as usize {
                                latest_run = current_run as i32;
                                latest_val = current_val;
                            }
                            break; //find the newest entry and break the for loop.
                        }
                    }
                }

                if latest_run >= 0 && latest_val != TOMBSTONE.as_bytes().to_vec() {
                    return Some(latest_val);
                }
            }
        }
        None
    }

    pub fn range(&self, start: &Vec<u8>, end: &Vec<u8>) -> Option<Vec<ValueT>> {
        //TODO deal with invalid input case
        // if end <= start {
        //     None
        // }

        //let mut counter = Arc::new(Mutex::new(0)); //TODO counter should be atomic
        let mut buffer_range: Vec<EntryT>;
        let mut ranges: HashMap<usize, Vec<EntryT>> = HashMap::new(); //record candidates in each level.

        //search in buffer and record result
        ranges.insert(0, self.buffer.range(start, end));

        //search in runs
        for current_run in 0..10 {
            match self.get_run(current_run) {
                Some(r) => {
                    //start and end are used multiple times which causes "use of moved value"
                    ranges.insert(current_run + 1, r.range(start, end));
                }
                _ => {}
            }
        }

        //TODO Merge ranges and return values. because there could be old values in ranges to be eliminated.
        // Only the latest values should be kept

        None
    }

    pub fn del(&mut self, key: Vec<u8>) {
        let entry = EntryT::new(key, TOMBSTONE.as_bytes().to_vec());

        self.put(entry);
    }

    //load lsm tree from disk file
    //    pub fn load(&mut self, filename : &str){
    //
    //    }
}
