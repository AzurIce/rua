//! DataStore：数据面唯一入口——节点正文的并发、缓存与 id 分配。
//!
//! "按 id 读内容"是全系统唯一的通用动作（inspect、装配、详情端点、读进行
//! 中的轮，底层都是它），由这一个组件负责：
//!
//! - [`DataStore::entry`] 注册 / 查找 / 惰性加载（缓存 miss 时建条目读文件）；
//! - 发给调用方的 [`Entry`] 句柄是 Arc + 类型标记，clone 廉价；
//! - `Entry<Turn>::append` 把"文件 IO + 内存更新"放进同一临界区——写穿是
//!   结构属性，不是约定。
//!
//! 并发与锁：目录锁（`maps`）管"找得到"，命中 = 读锁两次哈希；条目锁管
//! "看得一致"，读写分离。锁序单向（目录 → 条目）；读方从目录拿到 Arc 后
//! **先放目录锁再锁条目**。用同步锁是因为 engine 的 sink 是同步上下文。
//! single-flight：并发 miss 时先注册空条目（加载者持条目写锁读文件），
//! 后到者在条目锁上等待，拿到同一份加载结果。加载失败的条目中毒
//! （`load_error`），下一次 `entry()` 会淘汰它并重试。
//!
//! 淘汰（LRU 等）预留不实现：届时只从 map 摘 Arc，持有句柄的调用方无感。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::id::NodeId;
use crate::node::{Data, Kind, Turn, TurnLine};

/// 数据面条目的内存态：身份 + 路径 + 正文。无锁纯状态；锁在 DataStore
/// 的条目上。`load_error` 是加载失败的中毒标记（见模块文档）。
struct NodeData<T: Kind> {
    id: Ulid,
    path: PathBuf,
    data: T::Data,
    load_error: Option<String>,
}

/// 目录：TypeId 分桶 → 桶内裸 Ulid → 擦除了类型的条目 Arc。
type Directory = RwLock<HashMap<TypeId, HashMap<Ulid, Arc<dyn Any + Send + Sync>>>>;

/// 数据面。`Graph` 拥有一个实例；条目按 kind 分桶（TypeId 分发），桶内
/// 以裸 Ulid 索引——传错 kind 的查询在 per-kind map 里天然查不到。
pub struct DataStore {
    root: PathBuf,
    maps: Directory,
}

/// 发给调用方的句柄：Arc + 类型标记，clone 廉价。条目从 map 摘除
/// （淘汰，预留）不影响持有者。
pub struct Entry<T: Kind> {
    inner: Arc<RwLock<NodeData<T>>>,
}

impl<T: Kind> Clone for Entry<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl DataStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            maps: RwLock::new(HashMap::new()),
        }
    }

    /// 纯计算：铸 ULID + 类型绑定。不碰文件系统（文件是 Data 的持久化
    /// 细节，首次写时惰性创建）；不会失败。
    pub fn allocate<T: Kind>(&self) -> NodeId<T> {
        NodeId::new()
    }

    /// 条目是否已注册（commit 校验用：有正文的 kind 必须先有条目）。
    pub fn contains<T: Kind>(&self, id: NodeId<T>) -> bool {
        self.maps
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&TypeId::of::<T>())
            .is_some_and(|m| m.contains_key(&id.raw()))
    }

    /// 注册 / 查找 / 惰性加载：miss 时建条目读文件（single-flight，见模块
    /// 文档）。加载失败返回错误并把条目留作中毒态，下一次调用淘汰重试。
    pub fn entry<T: Kind>(&self, id: NodeId<T>) -> Result<Entry<T>> {
        let tid = TypeId::of::<T>();
        let raw = id.raw();

        // 快路径：命中即返回；中毒条目先淘汰再落到慢路径重试。
        let hit = self
            .maps
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&tid)
            .and_then(|m| m.get(&raw))
            .cloned();
        if let Some(arc) = hit {
            let entry = Entry::<T> {
                inner: downcast(arc.clone()),
            };
            let poisoned = entry
                .inner
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .load_error
                .is_some();
            if !poisoned {
                return Ok(entry);
            }
            // 淘汰中毒条目（仅当目录里还是同一条目），然后走慢路径重试。
            let mut maps = self.maps.write().unwrap_or_else(|p| p.into_inner());
            let stale = maps
                .get(&tid)
                .and_then(|m| m.get(&raw))
                .is_some_and(|cur| Arc::ptr_eq(cur, &arc));
            if stale {
                maps.get_mut(&tid).and_then(|m| m.remove(&raw));
            }
            drop(maps);
        }

        // 慢路径：占位 + single-flight。先拿条目写锁（此刻还没人能看到
        // 它），再注册进目录；加载全程持写锁，后到者阻塞在条目锁上，
        // 拿到同一份加载结果。
        let path = T::Data::path(&self.root, raw);
        let arc: Arc<RwLock<NodeData<T>>> = Arc::new(RwLock::new(NodeData {
            id: raw,
            path: path.clone(),
            data: T::Data::default(),
            load_error: None,
        }));
        let mut guard = arc.write().unwrap_or_else(|p| p.into_inner());
        {
            let mut maps = self.maps.write().unwrap_or_else(|p| p.into_inner());
            let per = maps.entry(tid).or_default();
            if let Some(existing) = per.get(&raw) {
                // 并发加载撞车：改用先到者的条目（read 时在其条目锁上等
                // 它加载完成）。
                let existing = downcast(existing.clone());
                drop(maps);
                drop(guard);
                return Ok(Entry { inner: existing });
            }
            per.insert(raw, arc.clone());
        }
        match T::Data::load(&path) {
            Ok(data) => {
                guard.data = data;
            }
            Err(e) => {
                // 缺失文件的 io 错误翻译成带 id 的 NodeNotFound。
                let e = match &e {
                    Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                        Error::NodeNotFound(raw)
                    }
                    _ => e,
                };
                guard.load_error = Some(e.to_string());
                drop(guard);
                return Err(e);
            }
        }
        drop(guard);
        Ok(Entry { inner: arc })
    }

    /// 创建并注册一个带内容的条目：先把正文整体写盘（文件已存在 = 不可变
    /// 违反），再注册。调用方总是用新铸的 id（`allocate` / 克隆重映射），
    /// 不存在并发同名创建。
    pub fn create<T: Kind>(&self, id: NodeId<T>, data: T::Data) -> Result<Entry<T>> {
        let tid = TypeId::of::<T>();
        let raw = id.raw();
        let path = T::Data::path(&self.root, raw);
        if path.exists() {
            return Err(Error::NodeAlreadyCommitted(raw));
        }
        data.save(&path)?;
        let arc: Arc<RwLock<NodeData<T>>> = Arc::new(RwLock::new(NodeData {
            id: raw,
            path,
            data,
            load_error: None,
        }));
        let mut maps = self.maps.write().unwrap_or_else(|p| p.into_inner());
        let per = maps.entry(tid).or_default();
        if per.contains_key(&raw) {
            return Err(Error::NodeAlreadyCommitted(raw));
        }
        per.insert(raw, arc.clone());
        drop(maps);
        Ok(Entry { inner: arc })
    }
}

/// 目录里擦除了类型的条目向下转回具体类型：TypeId 分桶保证同构，
/// 转换不会失败。
fn downcast<T: Kind>(arc: Arc<dyn Any + Send + Sync>) -> Arc<RwLock<NodeData<T>>> {
    arc.downcast::<RwLock<NodeData<T>>>()
        .expect("TypeId-keyed data store: downcast is infallible")
}

impl<T: Kind> Entry<T> {
    /// 该条目的节点 id。
    pub fn id(&self) -> NodeId<T> {
        NodeId::from_raw(self.inner.read().unwrap_or_else(|p| p.into_inner()).id)
    }

    /// 读正文：闭包内访问 `&T::Data`（std 的 RwLock 没有 stable 的映射
    /// 守卫，访问限定在条目锁的作用域内）。中毒条目报错。
    pub fn with<R>(&self, f: impl FnOnce(&T::Data) -> R) -> Result<R> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = &guard.load_error {
            return Err(Error::BodyLoadFailed {
                id: guard.id,
                reason: reason.clone(),
            });
        }
        Ok(f(&guard.data))
    }
}

impl<T: Kind> Entry<T>
where
    T::Data: Clone,
{
    /// 读正文的所有权拷贝（需要跨锁作用域持有内容时用）。
    pub fn cloned(&self) -> Result<T::Data> {
        self.with(Clone::clone)
    }
}

impl Entry<Turn> {
    /// 追加一行：先写文件再更新内存，同一临界区。文件写失败 → 内存不动，
    /// 两侧账本天然一致（失败的那一行进不了任何一侧）。
    pub fn append(&self, line: TurnLine) -> Result<()> {
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        crate::node::turn::append_line(&guard.path, &line)?;
        if let Some(step) = line.into_step() {
            guard.data.steps.push(step);
        }
        Ok(())
    }
}

// Context 正文没有增量操作：写入路径是 `DataStore::create`（整体写，不可变）。
