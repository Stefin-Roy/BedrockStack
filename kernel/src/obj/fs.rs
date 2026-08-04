//! Block-device, mount, directory, and file `Obj` adapters for the filesystems.
//!
//! `BlockNode` wraps a storage `BlockDevice` (exposing `submit`,
//! `sector_count`, `model_string`); `BlockFamilyNode` enumerates the kernel's
//! registered block devices; `MountNode` mounts the first partition of a
//! passed block capability and returns a `DirNode` for the drive's root.
//! `DirNode` resolves children and lists entries via capability-native
//! `traverse`/`readdir` hooks. `FileNode` exposes read/write/size/getattr.

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use spin::Once;

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::{CapRights, ContractRights, Rights};
use super::surface::{SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply, Value};
use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::dentry::{Dentry, dcache};
use crate::filesystems::vfs::types::FileType;

const BLOCK_FAMILY_OBJ_ID: ObjId = ObjId(0x10_0003);
const MOUNT_OBJ_ID: ObjId = ObjId(0x10_0004);
const DIR_OBJ_ID: ObjId = ObjId(0x10_0005);
const FILE_OBJ_ID: ObjId = ObjId(0x10_0006);

// ── BlockNode ──────────────────────────────────────────────────────────

/// A single block device, handed out by the block-family root (§7.11.4).
pub struct BlockNode {
    device: Arc<dyn BlockDevice>,
    model: &'static str,
}

impl BlockNode {
    pub fn new(device: Arc<dyn BlockDevice>) -> Self {
        let model = Box::leak(device.model_string().to_string().into_boxed_str());
        BlockNode { device, model }
    }

    pub fn device(&self) -> Arc<dyn BlockDevice> {
        self.device.clone()
    }
}

pub const BLOCK_CONTRACT: ContractId = ContractId::of("block:storage", &BLOCK_SURFACE, &BLOCK_HOOKS);
pub const BLOCK_SUBMIT: HookId = HookId::of("submit");
pub const BLOCK_SECTOR_COUNT: HookId = HookId::of("sector_count");
pub const BLOCK_MODEL_STRING: HookId = HookId::of("model_string");

pub const BLOCK_DOC: &str = "if you submit(lba, count, is_write, data) you do \
count sectors of IO against this block device and get back (completed, errors); \
sector_count() reports the device size and model_string() its model.";

const BLOCK_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "block:storage",
    attrs: &[],
    events: &[],
};

const BLOCK_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "submit",
        params: &[TypeTag::U64, TypeTag::U64, TypeTag::U64, TypeTag::Buf],
        reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64]),
    },
    HookSignature {
        name: "sector_count",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "model_string",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::Str]),
    },
];

static BLOCK_CONTRACTS: &[ContractId] = &[BLOCK_CONTRACT];

static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(0x10_1000);

impl Obj for BlockNode {
    fn obj_id(&self) -> ObjId {
        ObjId(NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed))
    }

    fn kind(&self) -> &'static str {
        "block:storage"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        BLOCK_CONTRACTS
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == BLOCK_SUBMIT {
            let lba = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let count = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            let is_write = arg_u64(args, 2).ok_or(ObjError::Denied)?;
            let data = arg_buf(args, 3).ok_or(ObjError::Denied)?;
            let req = IoRequest {
                lba,
                count: count as u32,
                buffer: IoBuffer::ConstBuf(data),
                is_write: is_write != 0,
            };
            return match self.device.submit(&[req]) {
                Ok(c) => Ok(Reply::Data(vec![
                    Value::U64(c.completed as u64),
                    Value::U64(c.errors as u64),
                ])),
                Err(_) => Err(ObjError::Denied),
            };
        }
        if hook == BLOCK_SECTOR_COUNT {
            return Ok(Reply::Data(vec![Value::U64(self.device.sector_count())]));
        }
        if hook == BLOCK_MODEL_STRING {
            return Ok(Reply::Data(vec![Value::Str(self.model)]));
        }
        Err(ObjError::NotSupported)
    }
}

fn arg_u64(args: &Args, i: usize) -> Option<u64> {
    match args.vals.get(i) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

fn arg_buf(args: &Args, i: usize) -> Option<&Vec<u8>> {
    match args.vals.get(i) {
        Some(Value::Buf(b)) => Some(b),
        _ => None,
    }
}

// ── BlockFamilyNode ────────────────────────────────────────────────────

/// The block-device family root: `first` returns the first registered block
/// device as a `BlockNode` capability.
pub struct BlockFamilyNode;

pub const BLOCK_FAMILY_CONTRACT: ContractId =
    ContractId::of("block:family", &BLOCK_FAMILY_SURFACE, &BLOCK_FAMILY_HOOKS);
pub const BLOCK_FAMILY_FIRST: HookId = HookId::of("first");

pub const BLOCK_FAMILY_DOC: &str = "if you first(), you get a capability to \
the first registered block device as a block:storage node.";

const BLOCK_FAMILY_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "block:family",
    attrs: &[],
    events: &[],
};

const BLOCK_FAMILY_HOOKS: &[HookSignature] = &[HookSignature {
    name: "first",
    params: &[],
    reply: ReplyTag::Caps,
}];

static BLOCK_FAMILY_CONTRACTS: &[ContractId] = &[BLOCK_FAMILY_CONTRACT];

impl Obj for BlockFamilyNode {
    fn obj_id(&self) -> ObjId {
        BLOCK_FAMILY_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "block:family"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        BLOCK_FAMILY_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == BLOCK_FAMILY_FIRST {
            let dev = crate::filesystems::blockdriver::driver::BLOCK_DEVICES
                .lock()
                .first()
                .cloned();
            return match dev {
                Some(dev) => {
                    let node = Arc::new(BlockNode::new(dev));
                    let rights = rights
                        .attune(Rights::INVOKE, ContractRights::empty())
                        .unwrap_or(CapRights::new(Rights::INVOKE, ContractRights::empty()));
                    Ok(Reply::Caps(vec![CapHandle {
                        id: CapId(0),
                        node,
                        rights,
                        state: HandleState::Live,
                    }]))
                }
                None => Err(ObjError::NotSupported),
            };
        }
        Err(ObjError::NotSupported)
    }
}

pub fn block_family_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(BlockFamilyNode)).clone()
}

// ── MountNode ──────────────────────────────────────────────────────────

/// The mount root: `mount(fstype, block_cap_id)` mounts the first partition
/// of the given block capability and returns a `DirNode` for the drive root.
pub struct MountNode;

pub const MOUNT_CONTRACT: ContractId = ContractId::of("fs:mount", &MOUNT_SURFACE, &MOUNT_HOOKS);
pub const MOUNT_HOOK: HookId = HookId::of("mount");

pub const MOUNT_DOC: &str = "if you mount(fstype, block_cap_id), the first \
partition of the block capability is mounted as B> and a DirNode capability \
to its root is replied.";

const MOUNT_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "fs:mount",
    attrs: &[],
    events: &[],
};

const MOUNT_HOOKS: &[HookSignature] = &[HookSignature {
    name: "mount",
    params: &[TypeTag::Str, TypeTag::U64],
    reply: ReplyTag::Caps,
}];

static MOUNT_CONTRACTS: &[ContractId] = &[MOUNT_CONTRACT];

impl Obj for MountNode {
    fn obj_id(&self) -> ObjId {
        MOUNT_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "fs:mount"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        MOUNT_CONTRACTS
    }

    fn dispatch(
        &self,
        caller: &super::table::CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == MOUNT_HOOK {
            let fstype = match args.vals.get(0) {
                Some(Value::Str(s)) => *s,
                _ => return Err(ObjError::Denied),
            };
            let id = match args.vals.get(1) {
                Some(Value::U64(id)) => *id,
                _ => return Err(ObjError::Denied),
            };
            // Handle tmpfs without a block device cap (P4-S3, §7.11)
            if fstype == "tmpfs" && id == 0 {
                // Check if drive A: is already mounted (vfs::init may have
                // done the ambient mount). Only mount if not already present.
                if crate::filesystems::vfs::DRIVE_MAP.lookup('A').is_err() {
                    crate::filesystems::vfs::mount("tmpfs", None, 'A')
                        .map_err(|_| ObjError::Denied)?;
                }
                let root = crate::filesystems::vfs::DRIVE_MAP
                    .lookup('A')
                    .map_err(|_| ObjError::Denied)?
                    .root.clone();
                let node: Arc<dyn Obj> = Arc::new(DirNode::new(root));
                let child_rights = rights
                    .attune(Rights::INVOKE, ContractRights::empty())
                    .unwrap_or(CapRights::new(Rights::INVOKE, ContractRights::empty()));
                return Ok(Reply::Caps(vec![CapHandle {
                    id: CapId(0),
                    node,
                    rights: child_rights,
                    state: HandleState::Live,
                }]));
            }
            if id == 0 {
                return Err(ObjError::NotSupported);
            }
            let dev_node = caller.get(CapId(id))?;
            let block = dev_node
                .as_any()
                .and_then(|a| a.downcast_ref::<BlockNode>())
                .ok_or(ObjError::Denied)?;
            let device = block.device();
            crate::filesystems::partition::mount_first_partition(device, fstype, 'B')
                .map_err(|_| ObjError::Denied)?;
            let root = crate::filesystems::vfs::DRIVE_MAP
                .lookup('B')
                .map_err(|_| ObjError::Denied)?
                .root.clone();
            let node = Arc::new(DirNode::new(root));
            let rights = rights
                .attune(Rights::INVOKE, ContractRights::empty())
                .unwrap_or(CapRights::new(Rights::INVOKE, ContractRights::empty()));
            return Ok(Reply::Caps(vec![CapHandle {
                id: CapId(0),
                node,
                rights,
                state: HandleState::Live,
            }]));
        }
        Err(ObjError::NotSupported)
    }
}

pub fn mount_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(MountNode)).clone()
}

// ── DirNode ─────────────────────────────────────────────────────────────

/// A directory node over a mounted drive's root dentry. `traverse(name)`
/// resolves a child and returns a DirNode or FileNode cap; `readdir()` lists
/// child caps; `label()` returns the directory name.
pub struct DirNode {
    root: Arc<Dentry>,
}

impl DirNode {
    pub fn new(root: Arc<Dentry>) -> Self {
        DirNode { root }
    }

    pub fn root(&self) -> Arc<Dentry> {
        self.root.clone()
    }

    /// Resolve a child dentry by name: children map → dcache → FS driver.
    fn resolve_child(&self, name: &str) -> Result<Arc<Dentry>, ObjError> {
        // 1. Check the dentry's children map
        {
            let children = self.root.children.lock();
            if let Some(child) = children.get(name) {
                return Ok(child.clone());
            }
        }
        // 2. Check the dcache
        let parent_ino = self.root.inode.lock().as_ref().map(|i| i.ino).unwrap_or(0);
        if let Some(cached) = dcache().lookup(parent_ino, name) {
            return Ok(cached);
        }
        // 3. Ask the FS driver
        let inode_lock = self.root.inode.lock();
        let parent_inode = inode_lock.as_ref().ok_or(ObjError::Denied)?;
        let child_ops = parent_inode.ops.lookup(name).map_err(|_| ObjError::Denied)?;
        let child_inode = Arc::new(crate::filesystems::vfs::inode::Inode::new(child_ops));
        let child_dentry = Dentry::new(name, Some(child_inode));
        *child_dentry.parent.lock() = Arc::downgrade(&self.root);
        let p_ino = parent_inode.ino;
        drop(inode_lock);
        dcache().insert(p_ino, String::from(name), Arc::downgrade(&child_dentry));
        self.root.children.lock().insert(String::from(name), child_dentry.clone());
        Ok(child_dentry)
    }

    /// Wrap a child dentry in the appropriate capability node, attuned to the
    /// caller's rights.
    fn materialize_child(
        &self,
        name: &str,
        child: Arc<Dentry>,
        rights: &CapRights,
    ) -> Result<CapHandle, ObjError> {
        let child_rights = rights
            .attune(Rights::INVOKE, ContractRights::empty())
            .unwrap_or(CapRights::new(Rights::INVOKE, ContractRights::empty()));
        let lock = child.inode.lock();
        let inode = lock.as_ref().ok_or(ObjError::Denied)?;
        match inode.file_type {
            FileType::Directory => {
                drop(lock);
                let node: Arc<dyn Obj> = Arc::new(DirNode::new(child));
                Ok(CapHandle { id: CapId(0), node, rights: child_rights, state: HandleState::Live })
            }
            FileType::Regular => {
                let inode = lock.clone().unwrap();
                drop(lock);
                let node: Arc<dyn Obj> = Arc::new(FileNode::new(String::from(name), inode));
                Ok(CapHandle { id: CapId(0), node, rights: child_rights, state: HandleState::Live })
            }
        }
    }
}

pub const DIR_CONTRACT: ContractId = ContractId::of("fs:dir", &DIR_SURFACE, &DIR_HOOKS);
pub const DIR_TRAVERSE: HookId = HookId::of("traverse");
pub const DIR_READDIR: HookId = HookId::of("readdir");
pub const DIR_LABEL: HookId = HookId::of("label");
pub const DIR_MKDIR: HookId = HookId::of("mkdir");

pub const DIR_DOC: &str = "traverse(name) resolves a child of this directory \
and readdir() lists its entries; both return DirNode/FileNode caps. \
mkdir(name) creates a subdirectory and returns a DirNode cap. \
label() returns the directory name.";

const DIR_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "fs:dir",
    attrs: &[],
    events: &[],
};

const DIR_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "traverse",
        params: &[TypeTag::Str],
        reply: ReplyTag::Caps,
    },
    HookSignature {
        name: "readdir",
        params: &[],
        reply: ReplyTag::Caps,
    },
    HookSignature {
        name: "label",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::Str]),
    },
    HookSignature {
        name: "mkdir",
        params: &[TypeTag::Str],
        reply: ReplyTag::Caps,
    },
];

static DIR_CONTRACTS: &[ContractId] = &[DIR_CONTRACT];

impl Obj for DirNode {
    fn obj_id(&self) -> ObjId {
        ObjId(NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed))
    }

    fn kind(&self) -> &'static str {
        "fs:dir"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        DIR_CONTRACTS
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == DIR_TRAVERSE {
            let name = match args.vals.get(0) {
                Some(Value::Str(s)) => *s,
                _ => return Err(ObjError::Denied),
            };
            let child = self.resolve_child(name)?;
            let cap = self.materialize_child(name, child, rights)?;
            return Ok(Reply::Caps(vec![cap]));
        }
        if hook == DIR_READDIR {
            let inode_lock = self.root.inode.lock();
            let parent_inode = inode_lock.as_ref().ok_or(ObjError::Denied)?;
            let entries = parent_inode.ops.readdir().map_err(|_| ObjError::Denied)?;
            drop(inode_lock);

            let mut caps: Vec<CapHandle> = Vec::new();
            for entry in &entries {
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                let child = match self.resolve_child(&entry.name) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Ok(cap) = self.materialize_child(&entry.name, child, rights) {
                    caps.push(cap);
                }
            }
            return Ok(Reply::Caps(caps));
        }
        if hook == DIR_LABEL {
            let name = self.root.name.lock().clone();
            let name_ref: &'static str = Box::leak(name.into_boxed_str());
            return Ok(Reply::Data(vec![Value::Str(name_ref)]));
        }
        if hook == DIR_MKDIR {
            let name = match args.vals.get(0) {
                Some(Value::Str(s)) => *s,
                _ => return Err(ObjError::Denied),
            };
            // Ask the FS driver to create the directory.
            {
                let inode_lock = self.root.inode.lock();
                let parent_inode = inode_lock.as_ref().ok_or(ObjError::Denied)?;
                parent_inode.ops.mkdir(name).map_err(|_| ObjError::Denied)?;
            }
            // resolve_child will find it via the FS driver and cache it.
            let child = self.resolve_child(name)?;
            let cap = self.materialize_child(name, child, rights)?;
            return Ok(Reply::Caps(vec![cap]));
        }
        Err(ObjError::NotSupported)
    }
}

// ── FileNode ────────────────────────────────────────────────────────────

/// A file node wrapping an `Inode`. Exposes read/write/size/getattr/label
/// hooks so file operations go through capabilities.
pub struct FileNode {
    name: String,
    inode: Arc<crate::filesystems::vfs::inode::Inode>,
}

impl FileNode {
    pub fn new(name: String, inode: Arc<crate::filesystems::vfs::inode::Inode>) -> Self {
        FileNode { name, inode }
    }
}

pub const FILE_CONTRACT: ContractId = ContractId::of("fs:file", &FILE_SURFACE, &FILE_HOOKS);
pub const FILE_READ_AT: HookId = HookId::of("read_at");
pub const FILE_WRITE_AT: HookId = HookId::of("write_at");
pub const FILE_SIZE: HookId = HookId::of("size");
pub const FILE_GETATTR: HookId = HookId::of("getattr");
pub const FILE_LABEL: HookId = HookId::of("label");

pub const FILE_DOC: &str = "read_at(offset, buf) reads bytes from the file; \
write_at(offset, data) writes bytes; size() returns the file size in bytes; \
getattr() returns (ino, size, file_type); label() returns the file name.";

const FILE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "fs:file",
    attrs: &[],
    events: &[],
};

const FILE_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "read_at",
        params: &[TypeTag::U64, TypeTag::Buf],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "write_at",
        params: &[TypeTag::U64, TypeTag::Buf],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "size",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "getattr",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64, TypeTag::U64]),
    },
    HookSignature {
        name: "label",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::Str]),
    },
];

static FILE_CONTRACTS: &[ContractId] = &[FILE_CONTRACT];

impl Obj for FileNode {
    fn obj_id(&self) -> ObjId {
        ObjId(NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed))
    }

    fn kind(&self) -> &'static str {
        "fs:file"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        FILE_CONTRACTS
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == FILE_READ_AT {
            let offset = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let data = arg_buf(args, 1).ok_or(ObjError::Denied)?;
            let mut buf = data.clone();
            let bytes = self.inode.ops.read_at(offset, &mut buf).map_err(|_| ObjError::Denied)?;
            return Ok(Reply::Data(vec![Value::U64(bytes as u64), Value::Buf(buf)]));
        }
        if hook == FILE_WRITE_AT {
            let offset = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let data = arg_buf(args, 1).ok_or(ObjError::Denied)?;
            let data_clone = data.clone();
            let bytes = self.inode.ops.write_at(offset, &data_clone).map_err(|_| ObjError::Denied)?;
            if offset + bytes as u64 > self.inode.size.load(Ordering::Relaxed) {
                self.inode.size.store(offset + bytes as u64, Ordering::Relaxed);
            }
            return Ok(Reply::Data(vec![Value::U64(bytes as u64)]));
        }
        if hook == FILE_SIZE {
            let sz = self.inode.size.load(Ordering::Relaxed);
            return Ok(Reply::Data(vec![Value::U64(sz)]));
        }
        if hook == FILE_GETATTR {
            let ft = match self.inode.file_type {
                FileType::Directory => 1u64,
                FileType::Regular => 0u64,
            };
            let sz = self.inode.size.load(Ordering::Relaxed);
            return Ok(Reply::Data(vec![
                Value::U64(self.inode.ino),
                Value::U64(sz),
                Value::U64(ft),
            ]));
        }
        if hook == FILE_LABEL {
            let name_ref: &'static str = Box::leak(self.name.clone().into_boxed_str());
            return Ok(Reply::Data(vec![Value::Str(name_ref)]));
        }
        Err(ObjError::NotSupported)
    }
}

// ── Contract definitions (§7.2.4, §7.8) ──────────────────────────────────

static BLOCK_CONTRACT_DEF: Contract = Contract {
    id: BLOCK_CONTRACT,
    name: "block:storage",
    surface: &BLOCK_SURFACE,
    hooks: BLOCK_HOOKS,
    doc: BLOCK_DOC,
};

static BLOCK_FAMILY_CONTRACT_DEF: Contract = Contract {
    id: BLOCK_FAMILY_CONTRACT,
    name: "block:family",
    surface: &BLOCK_FAMILY_SURFACE,
    hooks: BLOCK_FAMILY_HOOKS,
    doc: BLOCK_FAMILY_DOC,
};

static MOUNT_CONTRACT_DEF: Contract = Contract {
    id: MOUNT_CONTRACT,
    name: "fs:mount",
    surface: &MOUNT_SURFACE,
    hooks: MOUNT_HOOKS,
    doc: MOUNT_DOC,
};

static DIR_CONTRACT_DEF: Contract = Contract {
    id: DIR_CONTRACT,
    name: "fs:dir",
    surface: &DIR_SURFACE,
    hooks: DIR_HOOKS,
    doc: DIR_DOC,
};

/// The canonical definition of the block:storage contract (§7.8).
pub fn block_contract_def() -> &'static Contract {
    &BLOCK_CONTRACT_DEF
}

/// The canonical definition of the block:family contract (§7.8).
pub fn block_family_contract_def() -> &'static Contract {
    &BLOCK_FAMILY_CONTRACT_DEF
}

/// The canonical definition of the fs:mount contract (§7.8).
pub fn mount_contract_def() -> &'static Contract {
    &MOUNT_CONTRACT_DEF
}

/// The canonical definition of the fs:dir contract (§7.8).
pub fn dir_contract_def() -> &'static Contract {
    &DIR_CONTRACT_DEF
}

static FILE_CONTRACT_DEF: Contract = Contract {
    id: FILE_CONTRACT,
    name: "fs:file",
    surface: &FILE_SURFACE,
    hooks: FILE_HOOKS,
    doc: FILE_DOC,
};

/// The canonical definition of the fs:file contract (§7.8).
pub fn file_contract_def() -> &'static Contract {
    &FILE_CONTRACT_DEF
}