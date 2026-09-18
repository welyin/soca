//! 分段内容仓的字节侧（§9.3、§10.4）。
//!
//! §9.3 把"事件与不可变内容大块"放在一个**分段内容仓**里，与 SQLite 的事务元数据分开。
//! 分开的理由是实的：事务库要小、要快、要能整块备份，而一段录屏或一份长文档塞进去会把
//! 这三件事一起毁掉。[`crate::blobs`] 持有它的目录（引用、校验和、保留期），本模块持有字节。
//!
//! 三条本模块负责、且都能被测试证伪的性质：
//!
//! 1. **先耐久化，再交出引用。** §9.3："内容对象先写临时文件、完成校验和耐久化，再提交
//!    数据库引用；崩溃留下的孤儿对象由 GC 回收。" 少了 `sync_all` 那一步，崩溃之后可能留下
//!    一个**改名成功但内容没落盘**的对象，而它的校验和已经写进元数据了——元数据说它在那儿，
//!    磁盘上却没有。
//! 2. **读回来的内容必须与引用里声明的摘要一致。** 内容被换掉比缺失更糟：缺失会被发现，
//!    被换掉的内容看起来是好的。
//! 3. **引用不能当路径用。** 引用是从外部字符串解出来的，而它被拼进了文件路径。类别名必须是
//!    四个已知值之一，摘要必须是 64 位小写十六进制——否则一个 `blob:../../x:...` 就能读写
//!    仓外的任何文件。这一条是**安全边界**，不是参数校验。

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use soca_contracts::{BlobRef, DataClass, Sha256Hex};

use crate::error::StorageError;

/// 一次内容写入的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredContent {
    /// 对象引用。按内容寻址：同一份内容在同一个类别下总是同一个引用。
    pub blob_ref: BlobRef,
    /// 内容摘要。
    pub sha256: Sha256Hex,
    /// 字节数。
    pub bytes: u64,
}

/// 一次内容回收的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentGc {
    /// 回收的孤儿对象数（磁盘上有、元数据里没有）。
    pub orphans: usize,
    /// 清理的中断残留数（写入到一半就崩了留下的临时文件）。
    pub stray_temporaries: usize,
    /// 释放的字节数。
    pub bytes_freed: u64,
}

impl ContentGc {
    /// 本次是否什么都没做。
    pub fn is_empty(&self) -> bool {
        self.orphans == 0 && self.stray_temporaries == 0
    }
}

/// 分段内容仓。
///
/// 目录布局：`<root>/<数据类别>/<摘要前两位>/<摘要>`。两层分片是为了不让一个目录里堆进
/// 几十万个文件；类别那一层是 §9.3 的"按租户和数据类别隔离"——**它是布局层面的隔离，
/// 不是访问控制边界**。真正拦住越权读取的是引用解析（见模块文档第 3 条）与元数据里的保留期。
#[derive(Debug)]
pub struct ContentStore {
    root: PathBuf,
    /// 根目录是不是本进程建的。只有它才在析构时被删掉。
    owned: bool,
}

impl Drop for ContentStore {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

impl ContentStore {
    /// 打开（或创建）一个内容仓。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root, owned: false })
    }

    /// 一个只属于本次运行的临时内容仓，析构时自动删除。
    ///
    /// 给测试与内存存储用。它的存在是有代价的——**重启之后内容就没了**——所以它不该是
    /// 生产环境的默认值：[`crate::Store::location`] 有路径时，调用方应当把内容仓放在旁边。
    pub fn temporary() -> Result<Self, StorageError> {
        let unique = format!(
            "soca-content-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let root = std::env::temp_dir().join(unique);
        fs::create_dir_all(&root)?;
        Ok(Self { root, owned: true })
    }

    /// 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 写入内容，返回引用与摘要。
    ///
    /// 顺序是本模块存在的理由：写临时文件 → `sync_all` → **改名**（原子）→ 交出引用。
    /// 改名之后才返回，所以调用方拿到引用的那一刻，磁盘上一定已经有完整的字节了。
    pub fn put(&self, bytes: &[u8], class: DataClass) -> Result<StoredContent, StorageError> {
        let sha256 = Sha256Hex::of_bytes(bytes);
        let blob_ref = BlobRef::new(format!("blob:{}:{}", class.as_str(), sha256))?;
        let target = self.path_of(&blob_ref)?;

        if target.exists() {
            // 按内容寻址：同一份内容重复写入是幂等的，而且不必再写一遍。这里**不**重新校验
            // 已有文件——校验发生在读取时（见 `get`），而那才是内容真的被用到的时候。
            return Ok(StoredContent {
                blob_ref,
                sha256,
                bytes: bytes.len() as u64,
            });
        }

        let directory = target.parent().ok_or(StorageError::FailClosed(
            "内容仓目标路径没有父目录，无法创建分片目录",
        ))?;
        fs::create_dir_all(directory)?;

        // 临时名带上摘要：同一份内容被两个进程同时写入时，它们各自的临时文件互不干扰，
        // 而两次改名落到同一个最终路径上——内容相同，所以谁赢都一样。
        let temporary = directory.join(format!(".tmp-{sha256}"));
        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &target)?;

        Ok(StoredContent {
            blob_ref,
            sha256,
            bytes: bytes.len() as u64,
        })
    }

    /// 读取内容。不存在返回 `None`。
    ///
    /// 读回来的字节必须与引用里声明的摘要一致。不一致时报错而**不是**返回内容：一份被替换过的
    /// 内容看起来是好的，而 §9.3 要求"已有引用指向缺失对象时返回可诊断缺失，**不能伪造证据**"。
    /// 内容被换掉是同一件事的更坏版本。
    pub fn get(&self, blob_ref: &BlobRef) -> Result<Option<Vec<u8>>, StorageError> {
        let path = self.path_of(blob_ref)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        if Sha256Hex::of_bytes(&bytes).to_string() != declared_digest(blob_ref)? {
            return Err(StorageError::ContentCorrupted {
                blob_ref: blob_ref.to_string(),
            });
        }
        Ok(Some(bytes))
    }

    /// 对象是否在仓库里。
    pub fn contains(&self, blob_ref: &BlobRef) -> Result<bool, StorageError> {
        Ok(self.path_of(blob_ref)?.is_file())
    }

    /// 删除一个对象。返回它此前是否存在。
    pub fn remove(&self, blob_ref: &BlobRef) -> Result<bool, StorageError> {
        let path = self.path_of(blob_ref)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// 列出仓库里的全部对象引用。
    pub fn list(&self) -> Result<Vec<BlobRef>, StorageError> {
        let mut found = Vec::new();
        for (class_dir, class) in self.class_directories()? {
            for shard in read_directories(&class_dir)? {
                for entry in fs::read_dir(&shard)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if name.starts_with('.') {
                        continue;
                    }
                    found.push(BlobRef::new(format!("blob:{}:{name}", class.as_str()))?);
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// 仓库占用的字节数。
    pub fn bytes(&self) -> Result<u64, StorageError> {
        let mut total = 0u64;
        for reference in self.list()? {
            total = total.saturating_add(self.file_bytes(&reference)?);
        }
        Ok(total)
    }

    /// 回收孤儿对象与中断残留（§9.3："崩溃留下的孤儿对象由 GC 回收"）。
    ///
    /// `known` 是元数据表里仍然在册的引用。**不在册的一律删除**——这正是"孤儿"的定义，也是
    /// 一次崩溃最容易留下的东西：内容写完了、改名成功了，而元数据那一行还没提交。
    ///
    /// 反过来的情况（元数据在册、磁盘上没有）**不在这里处理**：那不是可以悄悄修好的，
    /// 它得让读到它的那条路径报出"可诊断缺失"。
    pub fn gc(&self, known: &BTreeSet<String>) -> Result<ContentGc, StorageError> {
        let mut report = ContentGc::default();

        for (class_dir, _class) in self.class_directories()? {
            for shard in read_directories(&class_dir)? {
                for entry in fs::read_dir(&shard)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };

                    if name.starts_with(".tmp-") {
                        let size = entry.metadata().map(|m| m.len()).unwrap_or_default();
                        if fs::remove_file(entry.path()).is_ok() {
                            report.stray_temporaries += 1;
                            report.bytes_freed = report.bytes_freed.saturating_add(size);
                        }
                        continue;
                    }

                    // 先把引用算出来再判断，所以"哪些文件该留"这一条判据只有一处。
                    let Ok(reference) = self.reference_for(&class_dir, name) else {
                        continue;
                    };
                    if known.contains(reference.as_str()) {
                        continue;
                    }
                    let size = entry.metadata().map(|m| m.len()).unwrap_or_default();
                    if fs::remove_file(entry.path()).is_ok() {
                        report.orphans += 1;
                        report.bytes_freed = report.bytes_freed.saturating_add(size);
                    }
                }
            }
        }
        Ok(report)
    }

    /// 由分片目录与文件名拼出引用。
    fn reference_for(&self, class_dir: &Path, name: &str) -> Result<BlobRef, StorageError> {
        let class = class_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(StorageError::FailClosed("内容仓类别目录名无法解析"))?;
        BlobRef::new(format!("blob:{class}:{name}")).map_err(StorageError::from)
    }

    fn file_bytes(&self, blob_ref: &BlobRef) -> Result<u64, StorageError> {
        match fs::metadata(self.path_of(blob_ref)?) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    fn class_directories(&self) -> Result<Vec<(PathBuf, DataClass)>, StorageError> {
        let mut found = Vec::new();
        for class in DataClass::ALL {
            let directory = self.root.join(class.as_str());
            if directory.is_dir() {
                found.push((directory, class));
            }
        }
        Ok(found)
    }

    /// 把引用解析成路径。
    ///
    /// **这是本模块唯一的安全边界，所以它做的是白名单而不是清理**：类别名必须是四个已知值
    /// 之一，摘要必须是 64 位小写十六进制。用"把 `..` 替换掉"之类的办法处理越权路径，永远
    /// 会有想不到的写法——而白名单只有一种结果：不在名单里的，拒绝。
    fn path_of(&self, blob_ref: &BlobRef) -> Result<PathBuf, StorageError> {
        let rest = blob_ref
            .as_str()
            .strip_prefix("blob:")
            .ok_or(StorageError::MalformedBlobRef {
                blob_ref: blob_ref.to_string(),
            })?;
        let (class, digest) = rest.split_once(':').ok_or(StorageError::MalformedBlobRef {
            blob_ref: blob_ref.to_string(),
        })?;
        DataClass::parse(class).ok_or(StorageError::MalformedBlobRef {
            blob_ref: blob_ref.to_string(),
        })?;
        if digest.len() != Sha256Hex::LEN
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StorageError::MalformedBlobRef {
                blob_ref: blob_ref.to_string(),
            });
        }

        Ok(self.root.join(class).join(&digest[..2]).join(digest))
    }
}

/// 取出引用里声明的摘要。
fn declared_digest(blob_ref: &BlobRef) -> Result<&str, StorageError> {
    blob_ref
        .as_str()
        .rsplit_once(':')
        .map(|(_, digest)| digest)
        .ok_or(StorageError::MalformedBlobRef {
            blob_ref: blob_ref.to_string(),
        })
}

/// 列出目录下的子目录，按名字排序，跳过点开头的。
fn read_directories(root: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        found.push(entry.path());
    }
    found.sort();
    Ok(found)
}
