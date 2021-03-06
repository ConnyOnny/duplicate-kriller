use std::fs;
use std::io;
use file::{FileContent, FileSet};
use std::path::{Path, PathBuf};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::BinaryHeap;
use metadata::Metadata;
use std::rc::Rc;
use std::sync::Mutex;
use std::os::unix::fs::MetadataExt;
use std::collections::hash_map::Entry as HashEntry;
use std::collections::btree_map::Entry as BTreeEntry;
use std::fmt::Debug;
use std::time::{Duration,Instant};

#[derive(Debug)]
pub struct Settings {
    // Ignore files smaller than a filesystem block.
    // Deduping of such files is unlikely to save space.
    pub ignore_small: bool,
    pub dry_run: bool,
}

#[derive(Debug,Default,Copy,Clone)]
#[cfg_attr(feature = "json",derive(Serialize))]
pub struct Stats {
    pub added: usize,
    pub skipped: usize,
    pub dupes: usize,
    pub hardlinks: usize,
}

pub trait ScanListener : Debug {
    fn file_scanned(&mut self, path: &PathBuf, stats: &Stats);
    fn scan_over(&self, scanner: &Scanner, stats: &Stats, scan_duration: Duration);
    fn hardlinked(&mut self, src: &Path, dst: &Path);
    fn duplicate_found(&mut self, src: &Path, dst: &Path);
}

#[derive(Debug)]
struct SilentListener;
impl ScanListener for SilentListener {
    fn file_scanned(&mut self, _: &PathBuf, _: &Stats) {}
    fn scan_over(&self, _: &Scanner, _: &Stats, _: Duration) {}
    fn hardlinked(&mut self, _: &Path, _: &Path) {}
    fn duplicate_found(&mut self, _: &Path, _: &Path) {}
}

#[derive(Debug)]
pub struct Scanner {
    /// All hardlinks of the same inode have to be treated as the same file
    by_inode: HashMap<(u64, u64), Rc<Mutex<FileSet>>>,
    /// See Hasher for explanation
    by_content: BTreeMap<FileContent, Vec<Rc<Mutex<FileSet>>>>,
    /// Directories left to scan. Sorted by inode number.
    /// I'm assuming scanning in this order is faster, since inode is related to file's age,
    /// which is related to its physical position on disk, which makes the scan more sequential.
    to_scan: BinaryHeap<(u64, PathBuf)>,

    scan_listener: Box<ScanListener>,
    stats: Stats,
    pub settings: Settings,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner {
            settings: Settings {
                ignore_small: true,
                dry_run: false,
            },
            by_inode: HashMap::new(),
            by_content: BTreeMap::new(),
            to_scan: BinaryHeap::new(),
            scan_listener: Box::new(SilentListener),
            stats: Stats::default(),
        }
    }

    /// Set the scan listener. Caution: This overrides previously set listeners!
    /// Use a multiplexing listener if multiple listeners are required.
    pub fn set_listener(&mut self, listener: Box<ScanListener>) {
        self.scan_listener = listener;
    }

    /// Scan any file or directory for dupes.
    /// Dedupe is done within the path as well as against all previously added paths.
    pub fn scan<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.enqueue(path)?;
        self.flush()?;
        Ok(())
    }

    pub fn enqueue<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = fs::canonicalize(path)?;
        let metadata = fs::symlink_metadata(&path)?;
        self.add(path, metadata)?;
        Ok(())
    }

    /// Drains the queue of directories to scan
    pub fn flush(&mut self) -> io::Result<()> {
        let start_time = Instant::now();
        while let Some((_, path)) = self.to_scan.pop() {
            self.scan_dir(path)?;
        }
        let scan_duration = Instant::now().duration_since(start_time);
        self.scan_listener.scan_over(&self, &self.stats, scan_duration);
        Ok(())
    }

    fn scan_dir(&mut self, path: PathBuf) -> io::Result<()> {
        /// Errors are ignored here, since it's super common to find permission denied and unreadable symlinks,
        /// and it'd be annoying if that aborted the whole operation.
        // FIXME: store the errors somehow to report them in a controlled manner
        for entry in fs::read_dir(path)?.filter_map(|p|p.ok()) {
            let path = entry.path();
            self.add(path, entry.metadata()?).unwrap_or_else(|e| println!("{:?}", e));
        }
        Ok(())
    }


    fn add(&mut self, path: PathBuf, metadata: fs::Metadata) -> io::Result<()> {
        self.scan_listener.file_scanned(&path, &self.stats);

        let ty = metadata.file_type();
        if ty.is_dir() {
            // Inode is truncated to group scanning of roughly close inodes together,
            // But still preserve some directory traversal order.
            // Negation to scan from the highest (assuming latest) first.
            let order_key = !(metadata.ino() >> 8);
            self.to_scan.push((order_key, path));
            return Ok(());
        } else if ty.is_symlink() {
            // Support for traversing symlinks would require preventing loops
            self.stats.skipped += 1;
            return Ok(());
        } else if !ty.is_file() {
            // Deduping /dev/ would be funny
            self.stats.skipped += 1;
            return Ok(());
        }

        if metadata.size() == 0 || (self.settings.ignore_small && metadata.size() < metadata.blksize()) {
            self.stats.skipped += 1;
            return Ok(());
        }

        self.stats.added += 1;

        let path_hardlinks = metadata.nlink();
        let m = (metadata.dev(), metadata.ino());

        // That's handling hardlinks
        let fileset = match self.by_inode.entry(m) {
            HashEntry::Vacant(e) => {
                let fileset = Rc::new(Mutex::new(FileSet::new(path.clone(), path_hardlinks)));
                e.insert(fileset.clone()); // clone just bumps a refcount here
                fileset
            },
            HashEntry::Occupied(mut e) => {
                self.stats.hardlinks += 1;
                let mut t = e.get_mut().lock().unwrap();
                t.push(path, path_hardlinks);
                return Ok(());
            }
        };

        // Here's where all the magic happens
        match self.by_content.entry(FileContent::new(path, Metadata::new(&metadata))) {
            BTreeEntry::Vacant(e) => {
                // Seems unique so far
                e.insert(vec![fileset]);
            },
            BTreeEntry::Occupied(mut e) => {
                // Found a dupe!
                self.stats.dupes += 1;
                let filesets = e.get_mut();
                filesets.push(fileset);
                Self::dedupe(filesets, self.settings.dry_run, &mut self.scan_listener)?;
            },
        }
        Ok(())
    }

    fn dedupe(filesets: &mut Vec<Rc<Mutex<FileSet>>>, dry_run: bool, scan_listener: &mut Box<ScanListener>) -> io::Result<()> {
        // Find file with the largest number of hardlinks, since it's less work to merge a small group into a large group
        let (largest_idx, merged_fileset) = filesets.iter().enumerate().max_by_key(|&(i,f)| (f.lock().unwrap().links(),!i)).expect("fileset can't be empty");

        // The set is still going to be in use! So everything has to be updated to make sense for the next call
        let merged_paths = &mut merged_fileset.lock().unwrap().paths;
        let source_path = merged_paths[0].clone();
        for (i, set) in filesets.iter().enumerate() {
            // We don't want to merge the set with itself
            if i == largest_idx {continue;}

            let paths = &mut set.lock().unwrap().paths;
            // dest_path will be "lost" on error, but that's fine, since we don't want to dedupe it if it causes errors
            for dest_path in paths.drain(..) {
                assert_ne!(&source_path, &dest_path);
                debug_assert_ne!(fs::symlink_metadata(&source_path)?.ino(), fs::symlink_metadata(&dest_path)?.ino());

                if dry_run {
                    scan_listener.duplicate_found(&dest_path, &source_path);
                    merged_paths.push(dest_path);
                    continue;
                }

                let temp_path = dest_path.with_file_name(".tmp-dupe-e1iIQcBFn5pC4MUSm-xkcd-221");
                debug_assert!(!temp_path.exists());
                debug_assert!(source_path.exists());
                debug_assert!(dest_path.exists());

                // In posix link guarantees not to overwrite, and mv guarantes to move atomically
                // so this two-step replacement is pretty robust
                if let Err(err) = fs::hard_link(&source_path, &temp_path) {
                    println!("unable to hardlink {} {} due to {:?}", source_path.display(), temp_path.display(), err);
                    fs::remove_file(temp_path).ok();
                    return Err(err);
                }
                if let Err(err) = fs::rename(&temp_path, &dest_path) {
                    println!("unable to rename {} {} due to {:?}", temp_path.display(), dest_path.display(), err);
                    fs::remove_file(temp_path).ok();
                    return Err(err);
                }
                debug_assert!(!temp_path.exists());
                debug_assert!(source_path.exists());
                debug_assert!(dest_path.exists());
                scan_listener.hardlinked(&dest_path, &source_path);
                merged_paths.push(dest_path);
            }
        }
        Ok(())
    }

    pub fn dupes(&self) -> Vec<FileSet> {
        self.by_inode.iter().map(|(_,d)|{
            let tmp = d.lock().unwrap();
            (*tmp).clone()
        }).collect()
    }
}

