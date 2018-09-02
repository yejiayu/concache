// TODO look into adding a logger (envlogger)

#![feature(integer_atomics)]

extern crate rand;
extern crate crossbeam;

use std::sync::{Mutex, RwLock, atomic::*, Arc};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::thread;
use std::fmt;
use std::mem;
use rand::{thread_rng, Rng};

use crossbeam::epoch::{self, Atomic, Owned};

const AVG_PER_BIN_THRESH : usize = 4;

struct Node {
    kv: (usize, Mutex<usize>),
    active: AtomicBool,
    next: Atomic<Node>,
    prev: Atomic<Node>
}

struct LinkedList {
    first: Atomic<Node>,
}

struct Table {
    bsize: usize,
    mp: Vec<LinkedList>
}

struct HashMap {
    bsize: AtomicUsize,
    size: AtomicUsize,
    table: RwLock<Table>
}

impl Node {
    fn new (k : usize, v : usize) -> Self {
        Node {
            kv: (k, Mutex::new(v)),
            active: AtomicBool::new(true),
            next: Atomic::null(),
            prev: Atomic::null()
        }
    }
}

impl LinkedList {
    fn new () -> Self {
        LinkedList {
            first: Atomic::null()
        }
    }

    fn insert (&self, kv : (usize, usize)) -> bool {
        let guard = epoch::pin();

        let mut node = &self.first;
        loop {
            match node.load(Ordering::Relaxed, &guard) {
                Some(k) => {
                    let mut raw = k.as_raw();
                    let mut cur = unsafe { &*raw };
                    if cur.kv.0 == kv.0 && cur.active.load(Ordering::Relaxed) {
                        let mut change = cur.kv.1.lock().unwrap();
                        *change = kv.1;
                        return false;
                    }
                    node = &k.next;
                },
                None => {
                    break;
                }
            };
        }

        // key does not exist
        let mut ins = Owned::new(Node::new(kv.0, kv.1));
        loop {
            let first = self.first.load(Ordering::Relaxed, &guard);
            ins.next.store_shared(first, Ordering::Relaxed);

            match self.first.cas_and_ref(first, ins, Ordering::Release, &guard) {
                Ok(_) => break,
                Err(owned) => ins = owned
            }
        }

        // update the prev reference of first.next to reform the doubly-linked list
        let first = self.first.load(Ordering::Relaxed, &guard);
        let k = first.unwrap().as_raw();
        let k_raw = unsafe { &*k };
        match k_raw.next.load(Ordering::Relaxed, &guard) {
            Some(next) => {
                let next_raw = unsafe { &*next.as_raw() };
                next_raw.prev.store_shared(first, Ordering::Relaxed);
            },
            None => {}
        }

        return true;
    }

    fn get (&self, key : usize) -> Option<usize> {
        let guard = epoch::pin();

        let mut node = &self.first;
        loop {
            match node.load(Ordering::Relaxed, &guard) {
                Some(k) => {
                    let mut raw = k.as_raw();
                    let mut cur = unsafe { &*raw };
                    if cur.kv.0 == key && cur.active.load(Ordering::Relaxed) {
                        let value = cur.kv.1.lock().unwrap();
                        return Some(*value);
                    }
                    node = &k.next;
                },
                None => {
                    return None;
                }
            };
        }

    }

    fn remove (&self, key : usize) -> bool {
        let guard = epoch::pin();

        let mut node = &self.first;
        loop {
            match node.load(Ordering::Relaxed, &guard) {
                Some(k) => {
                    let mut raw = k.as_raw();
                    let mut cur = unsafe { &*raw };
                    if cur.kv.0 == key && cur.active.load(Ordering::Relaxed) {
                        cur.active.store(false, Ordering::SeqCst);

                        let next = k.next.load(Ordering::Relaxed, &guard);
                        let prev = k.prev.load(Ordering::Relaxed, &guard);

                        node.cas_shared(Some(k), next, Ordering::Release);

                        let mut new_node = node.load(Ordering::Relaxed, &guard).unwrap();
                        let mut new_node_raw_cur = unsafe { &*new_node.as_raw() };

                        if new_node_raw_cur.prev.cas_shared(Some(k), prev, Ordering::Release) {
                            unsafe { guard.unlinked(k) };
                            return true;
                        }
                    }
                    node = &k.next;
                },
                None => {
                    // the node with key key didn't exist
                    return false;
                }
            };
        }
    }
}

impl fmt::Display for LinkedList {
    fn fmt (&self, f : &mut fmt::Formatter) -> fmt::Result {
        let guard = epoch::pin();

        let mut ret = String::new();
        let mut node = &self.first;
        loop {
            match node.load(Ordering::Relaxed, &guard) {
                Some(k) => {
                    let mut raw = k.as_raw();
                    let mut cur = unsafe { &*raw };
                    if cur.active.load(Ordering::Relaxed) {
                        let key = cur.kv.0;
                        println!("Taking lock for value");
                        let value = cur.kv.1.lock().unwrap();
                        println!("Took lock for value");

                        ret.push_str("(");
                        ret.push_str(&key.to_string());
                        ret.push_str(", ");
                        ret.push_str(&value.to_string());
                        ret.push_str("), ");

                        println!("Releasing lock for value");
                    }
                    node = &k.next;
                },
                None => {
                    break;
                }
            };
        }

        write!(f, "{}", ret)
    }
}

impl Table {
    fn new (nbuckets : usize) -> Self {
        let mut v = Vec::with_capacity(nbuckets);

        for _i in 0..nbuckets {
            v.push(LinkedList::new());
        }

        let ret = Table {
            bsize: nbuckets,
            mp: v
        };

        ret
    }

    fn resize (&mut self, nbuckets : usize) {
        let guard = epoch::pin();

        let new = Table::new(nbuckets);
        for i in 0..self.bsize {

            let ll = &self.mp[i];
            let mut node = &ll.first;
            loop {
                match node.load(Ordering::Relaxed, &guard) {
                    Some(k) => {
                        let mut raw = k.as_raw();
                        let mut cur = unsafe { &*raw };
                        if cur.active.load(Ordering::Relaxed) {
                            new.insert(cur.kv.0, *cur.kv.1.lock().unwrap());
                        }
                        node = &k.next;
                    },
                    None => {
                        break;
                    }
                };
            }
        }

        mem::replace(&mut self.mp, new.mp);
    }

    fn insert (&self, key : usize, value : usize) -> bool {
        let mut hsh = DefaultHasher::new();
        hsh.write_usize(key);
        let h = hsh.finish() as usize;

        let ndx = h % self.bsize;
        println!("{} {} {}", ndx, self.mp.capacity(), self.bsize);
        self.mp[ndx].insert((key, value))
    }

    fn get (&self, key : usize) -> Option<usize> {
        let mut hsh = DefaultHasher::new();
        hsh.write_usize(key);
        let h = hsh.finish() as usize;

        let ndx = h % self.bsize;

        self.mp[ndx].get(key)
    }

    fn remove (&self, key : usize) -> bool {
        let mut hsh = DefaultHasher::new();
        hsh.write_usize(key);
        let h = hsh.finish() as usize;

        let ndx = h % self.bsize;

        self.mp[ndx].remove(key)
    }
}

impl fmt::Display for Table {
    fn fmt (&self, f : &mut fmt::Formatter) -> fmt::Result {
        let mut all = String::new();
        for i in 0..self.bsize {
            all.push_str(&(&self).mp[i].to_string());
        }
        let ret : String = all.chars().skip(0).take(all.len() - 2).collect();
        write!(f, "[{}]", ret)
    }
}

impl HashMap {
    fn new () -> Self {
        HashMap {
            bsize: AtomicUsize::new(1),
            size: AtomicUsize::new(0),
            table: RwLock::new(Table::new(1))
        }
    }

    fn insert (&self, key : usize, val : usize) {
        let size = self.size.load(Ordering::Relaxed);
        let bsize = self.bsize.load(Ordering::Relaxed);
        if size / bsize >= AVG_PER_BIN_THRESH {
            self.resize();
        }

        let t = self.table.write().unwrap();
        if t.insert(key, val) {
            self.size.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn get (&self, key : usize) -> Option<usize> {
        println!("Taking read lock for get");
        let t = (&self).table.read().unwrap();
        println!("Took read lock for get");
        let ret = t.get(key);
        println!("Releasing read lock for get");
        ret
    }

    fn remove (&self, key : usize) {
        println!("Taking read lock for remove");
        let t = (&self).table.read().unwrap();
        println!("Took read lock for remove");
        if t.remove(key) {
            self.size.fetch_sub(1, Ordering::Relaxed);
        }
        println!("Releasing read lock for remove");
    }

    fn resize (&self) {
        // TODO make sure we don't over-resize
        let bsize = self.bsize.load(Ordering::Relaxed);
        println!("Taking write lock");
        let mut t = (&self).table.write().unwrap();
        println!("Took write lock");
        t.resize(bsize * 2);
        t.bsize = bsize * 2;
        println!("Releasing write lock");
        self.bsize.store(bsize * 2, Ordering::Relaxed);
    }

    fn size (&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }
}

impl fmt::Display for HashMap {
    fn fmt (&self, f : &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", (&self).table.read().unwrap().to_string())
    }
}

fn main() {
    // let mut new_linked_list = LinkedList::new();
    // println!("{:?}", new_linked_list);
    // new_linked_list.insert((3, 2));
    // new_linked_list.insert((3, 4));
    // new_linked_list.insert((5, 8));
    // new_linked_list.insert((4, 6));
    // new_linked_list.insert((1, 8));
    // new_linked_list.insert((6, 6));
    // new_linked_list.print();

    // assert_eq!(new_linked_list.get(3).unwrap(), 4);
    // assert_eq!(new_linked_list.get(5).unwrap(), 8);
    // assert_eq!(new_linked_list.get(2), None);

    println!("Started");
    let mut new_HashMap = HashMap::new(); //init with 16 buckets
    // new_HashMap.mp[0].push((1,2)); //manually push
    //input values
    new_HashMap.insert(1, 1);
    new_HashMap.insert(2, 5);
    new_HashMap.insert(12, 5);
    new_HashMap.insert(12, 7);
    new_HashMap.insert(0, 0);

    println!("testing for 4");
    println!("{}", new_HashMap.to_string());
    assert_eq!(new_HashMap.size(), 4); //should be 4 after you attempt the 5th insert

    new_HashMap.insert(20, 3);
    new_HashMap.insert(3, 2);
    new_HashMap.insert(4, 1);
    new_HashMap.insert(5, 5);

    new_HashMap.insert(20, 5); //repeated
    new_HashMap.insert(3, 8); //repeated
    println!("testing for 8");
    assert_eq!(new_HashMap.size(), 8);

    new_HashMap.remove(20);
    println!("{} {}", new_HashMap.to_string(), new_HashMap.size());

    println!("Finished.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn HashMap_basics() {
        let mut new_HashMap = HashMap::new(); //init with 2 buckets
        //input values

        new_HashMap.insert(1, 1);
        new_HashMap.insert(2, 5);
        new_HashMap.insert(12, 5);
        new_HashMap.insert(13, 7);
        new_HashMap.insert(0, 0);

        assert_eq!(new_HashMap.size(), 5); //should be 5 after you attempt the 5th insert

        new_HashMap.insert(20, 3);
        new_HashMap.insert(3, 2);
        new_HashMap.insert(4, 1);
        new_HashMap.insert(5, 5);

        new_HashMap.insert(20, 5); //repeated
        new_HashMap.insert(3, 8); //repeated
        assert_eq!(new_HashMap.size(), 9); //should be 9 after you attempt the 11th insert

        assert_eq!(new_HashMap.get(20).unwrap(), 5);
        assert_eq!(new_HashMap.get(12).unwrap(), 5);
        assert_eq!(new_HashMap.get(1).unwrap(), 1);
        assert_eq!(new_HashMap.get(0).unwrap(), 0);
        assert!(new_HashMap.get(3).unwrap() != 2); // test that it changed

        assert_eq!(new_HashMap.table.read().unwrap().mp.capacity(), 4); //make sure it is correct length

        // try the same assert_eqs
        assert_eq!(new_HashMap.get(20).unwrap(), 5);
        assert_eq!(new_HashMap.get(12).unwrap(), 5);
        assert_eq!(new_HashMap.get(1).unwrap(), 1);
        assert_eq!(new_HashMap.get(0).unwrap(), 0);
        assert!(new_HashMap.get(3).unwrap() != 2); // test that it changed
    }

    #[test]
    fn HashMap_concurr() {
        let mut new_HashMap = Arc::new(HashMap::new()); //init with 16 buckets                                                   // new_HashMap.mp[0].push((1,2));
        let mut threads = vec![];
        let nthreads = 8;
        for _ in 0..nthreads {
            let new_HashMap = new_HashMap.clone();

            threads.push(thread::spawn(move || {
                for _ in 1..1000 {
                    let val = thread_rng().gen_range(0, 256);
                    if val % 3 == 0 {
                        new_HashMap.insert(val, val);
                    } else if val % 3 == 1 {
                        let v = new_HashMap.get(val);
                        if v != None {
                            assert_eq!(v.unwrap(), val);
                        }
                    } else {
                        new_HashMap.remove(val);
                    }
                    println!("here");
                }
            }));
        }
        for t in threads {
            println!("here");
            t.join().unwrap();
        }
    }
}