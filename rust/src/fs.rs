use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Filesystem, Generation, INodeNo,
    LockOwner, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use indicatif::{ProgressBar, ProgressStyle};

use crate::cache::{NotionCache, Tree};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: INodeNo = INodeNo(1);
const GENERATION: Generation = Generation(0);
const BLOCK_SIZE: u32 = 512;

/// Deterministic inode from path. Uses a hash, reserving 1 for root.
fn path_to_ino(path: &PathBuf) -> INodeNo {
    if path.as_os_str() == "/" {
        return ROOT_INO;
    }
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let h = hasher.finish();
    // Avoid collisions with ROOT_INO (1) and 0
    INodeNo(if h <= 1 { h + 2 } else { h })
}

pub struct NotionFS {
    cache: Arc<NotionCache>,
    rendered: Mutex<HashMap<String, Vec<u8>>>,
    refresh_buf: Mutex<HashMap<String, Vec<u8>>>,
    ino_to_path: RwLock<HashMap<INodeNo, PathBuf>>,
    uid: u32,
    gid: u32,
}

impl NotionFS {
    pub fn new(cache: Arc<NotionCache>) -> Self {
        let mut ino_to_path = HashMap::new();
        let root = PathBuf::from("/");
        ino_to_path.insert(ROOT_INO, root);

        Self {
            cache,
            rendered: Mutex::new(HashMap::new()),
            refresh_buf: Mutex::new(HashMap::new()),
            ino_to_path: RwLock::new(ino_to_path),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn register_ino(&self, path: &PathBuf) -> INodeNo {
        let ino = path_to_ino(path);
        {
            let map = self.ino_to_path.read().unwrap();
            if map.contains_key(&ino) {
                return ino;
            }
        }
        self.ino_to_path
            .write()
            .unwrap()
            .insert(ino, path.clone());
        ino
    }

    fn get_path(&self, ino: INodeNo) -> Option<PathBuf> {
        self.ino_to_path.read().unwrap().get(&ino).cloned()
    }

    fn path_parts(path: &PathBuf) -> Vec<String> {
        path.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => s.to_str().map(|s| s.to_string()),
                _ => None,
            })
            .collect()
    }

    fn dir_attr(&self, ino: INodeNo) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn file_attr(&self, ino: INodeNo) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn is_dir(parts: &[String]) -> bool {
        match parts.len() {
            0 => true,
            1 => true,
            2 => parts[1] != ".refresh",
            3 => true,
            _ => false,
        }
    }

    fn path_exists_in(tree: &Tree, parts: &[String]) -> bool {
        match parts.len() {
            0 => true,
            1 => tree.contains_key(&parts[0]),
            2 => {
                if parts[1] == ".refresh" {
                    return tree.contains_key(&parts[0]);
                }
                tree.get(&parts[0])
                    .map(|a| a.contains_key(&parts[1]))
                    .unwrap_or(false)
            }
            3 => tree
                .get(&parts[0])
                .and_then(|a| a.get(&parts[1]))
                .map(|s| s.contains_key(&parts[2]))
                .unwrap_or(false),
            4 => tree
                .get(&parts[0])
                .and_then(|a| a.get(&parts[1]))
                .and_then(|s| s.get(&parts[2]))
                .map(|tickets| {
                    tickets
                        .iter()
                        .any(|t| format!("{}.md", t.ticket_id) == parts[3])
                })
                .unwrap_or(false),
            _ => false,
        }
    }
}

impl Filesystem for NotionFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let child_name = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let child_path = parent_path.join(child_name);
        let parts = Self::path_parts(&child_path);
        let tree = self.cache.get_tree();

        if !Self::path_exists_in(&tree, &parts) {
            reply.error(Errno::ENOENT);
            return;
        }

        let ino = self.register_ino(&child_path);
        let attr = if Self::is_dir(&parts) {
            self.dir_attr(ino)
        } else {
            self.file_attr(ino)
        };

        reply.entry(&TTL, &attr, GENERATION);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parts = Self::path_parts(&path);
        let tree = self.cache.get_tree();

        if !Self::path_exists_in(&tree, &parts) {
            reply.error(Errno::ENOENT);
            return;
        }

        let attr = if Self::is_dir(&parts) {
            self.dir_attr(ino)
        } else {
            self.file_attr(ino)
        };

        reply.attr(&TTL, &attr);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parts = Self::path_parts(&path);
        let tree = self.cache.get_tree();

        let mut entries: Vec<(String, FileType)> = vec![
            (".".to_string(), FileType::Directory),
            ("..".to_string(), FileType::Directory),
        ];

        match parts.len() {
            0 => {
                for proj in tree.keys() {
                    entries.push((proj.clone(), FileType::Directory));
                }
            }
            1 => {
                if let Some(assignees) = tree.get(&parts[0]) {
                    entries.push((".refresh".to_string(), FileType::RegularFile));
                    for assignee in assignees.keys() {
                        entries.push((assignee.clone(), FileType::Directory));
                    }
                }
            }
            2 => {
                if let Some(statuses) = tree.get(&parts[0]).and_then(|a| a.get(&parts[1])) {
                    for status in statuses.keys() {
                        entries.push((status.clone(), FileType::Directory));
                    }
                }
            }
            3 => {
                if let Some(tickets) = tree
                    .get(&parts[0])
                    .and_then(|a| a.get(&parts[1]))
                    .and_then(|s| s.get(&parts[2]))
                {
                    for ticket in tickets {
                        entries.push((
                            format!("{}.md", ticket.ticket_id),
                            FileType::RegularFile,
                        ));
                    }
                }
            }
            _ => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        }

        entries[2..].sort_by(|a, b| a.0.cmp(&b.0));

        let child_paths: Vec<(PathBuf, INodeNo)> = entries
            .iter()
            .map(|(name, _)| {
                let cp = if name == "." {
                    path.clone()
                } else if name == ".." {
                    path.parent().unwrap_or(&path).to_path_buf()
                } else {
                    path.join(name)
                };
                let ino = path_to_ino(&cp);
                (cp, ino)
            })
            .collect();

        {
            let mut map = self.ino_to_path.write().unwrap();
            for (cp, ino) in &child_paths {
                map.entry(*ino).or_insert(cp.clone());
            }
        }

        for (i, ((_, file_type), (_, child_ino))) in entries
            .iter()
            .zip(child_paths.iter())
            .enumerate()
            .skip(offset as usize)
        {
            if reply.add(*child_ino, (i + 1) as u64, *file_type, &entries[i].0) {
                break;
            }
        }

        reply.ok();
    }

    fn open(&self, _req: &Request, _ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        let access_mode = flags.0 & libc::O_ACCMODE;
        if access_mode != libc::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parts = Self::path_parts(&path);
        let offset = offset as usize;
        let size = size as usize;

        // .refresh file
        if parts.len() == 2 && parts[1] == ".refresh" {
            let proj_slug = &parts[0];

            if offset == 0 {
                let slug_map = self.cache.get_slug_map();
                let display_name = slug_map
                    .get(proj_slug.as_str())
                    .cloned()
                    .unwrap_or_else(|| proj_slug.clone());

                // Estimate total from current cache for progress bar
                let estimated_total = {
                    let tree = self.cache.get_tree();
                    tree.get(proj_slug.as_str())
                        .map(|a| {
                            a.values()
                                .flat_map(|s| s.values().map(|v| v.len()))
                                .sum::<usize>()
                        })
                        .unwrap_or(0) as u64
                };

                let pb = ProgressBar::new(estimated_total);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template("{spinner:.green} {msg} [{bar:30.cyan/dim}] {pos}/{len} tickets ({eta})")
                        .unwrap()
                        .progress_chars("=> "),
                );
                pb.set_message(format!("Refreshing {}", display_name));
                pb.enable_steady_tick(Duration::from_millis(100));

                let pb_ref = &pb;
                let count = self.cache.refresh(
                    Some(&display_name),
                    Some(&|tickets_so_far| {
                        pb_ref.set_position(tickets_so_far as u64);
                    }),
                );

                pb.finish_and_clear();
                let msg = format!("Refreshed {}: {} tickets\n", display_name, count);
                let msg_bytes = msg.into_bytes();

                self.refresh_buf
                    .lock()
                    .unwrap()
                    .insert(proj_slug.to_string(), msg_bytes);

                self.rendered.lock().unwrap().clear();

                let cache = self.cache.clone();
                let slug = proj_slug.to_string();
                std::thread::spawn(move || {
                    cache.save_project_cache(&slug);
                });
            }

            let buf = self.refresh_buf.lock().unwrap();
            if let Some(data) = buf.get(proj_slug.as_str()) {
                let end = data.len().min(offset + size);
                if offset >= data.len() {
                    reply.data(&[]);
                } else {
                    reply.data(&data[offset..end]);
                }
            } else {
                reply.data(&[]);
            }
            return;
        }

        // Ticket file
        if parts.len() == 4 {
            let (proj, assignee, status, filename) = (&parts[0], &parts[1], &parts[2], &parts[3]);

            let tree = self.cache.get_tree();
            let ticket = tree
                .get(proj.as_str())
                .and_then(|a| a.get(assignee.as_str()))
                .and_then(|s| s.get(status.as_str()))
                .and_then(|tickets| {
                    tickets
                        .iter()
                        .find(|t| format!("{}.md", t.ticket_id) == *filename)
                })
                .cloned();

            match ticket {
                Some(mut ticket) => {
                    let page_id = ticket.page_id.clone();

                    let cached = self.rendered.lock().unwrap().get(&page_id).cloned();
                    let data = if let Some(data) = cached {
                        data
                    } else {
                        if ticket.description.is_empty() {
                            match self.cache.fetch_description(&page_id) {
                                Ok(desc) => ticket.description = desc,
                                Err(e) => {
                                    eprintln!(
                                        "Failed to fetch description for {}: {}",
                                        page_id, e
                                    );
                                }
                            }
                        }

                        let rendered = ticket.render();
                        self.rendered
                            .lock()
                            .unwrap()
                            .insert(page_id, rendered.clone());
                        rendered
                    };

                    let end = data.len().min(offset + size);
                    if offset >= data.len() {
                        reply.data(&[]);
                    } else {
                        reply.data(&data[offset..end]);
                    }
                }
                None => reply.error(Errno::ENOENT),
            }
            return;
        }

        reply.error(Errno::ENOENT);
    }

    // -----------------------------------------------------------------------
    // Read-only stubs
    // -----------------------------------------------------------------------

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }
}
