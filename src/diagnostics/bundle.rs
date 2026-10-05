//! Explicit, local-only support bundle. Collection and compression never run on UI/media threads.
mod redact;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const CHUNK_BYTES: u64 = 4 * 1024 * 1024; // Working memory, not an inclusion limit.
const ARCHIVE_LIMIT: u64 = 32 * 1024 * 1024;
pub(crate) struct Input {
    pub summary: Value,
    pub private_values: Vec<String>,
}
pub(crate) struct Exported {
    pub path: PathBuf,
    pub logs: usize,
    pub warnings: usize,
}
pub(crate) struct Task {
    pub receiver: std::sync::mpsc::Receiver<std::result::Result<Exported, String>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl Task {
    pub fn start(input: Input, ctx: egui::Context) -> Result<Self> {
        let snapshot = super::logging::snapshot().context("日志系统尚未初始化")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (tx, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("diagnostic-export".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    export(input, snapshot, &worker_cancel)
                }))
                .map_err(|_| "诊断导出任务异常结束".to_owned())
                .and_then(|r| r.map_err(|e| format!("{e:#}")));
                let _ = tx.send(result);
                ctx.request_repaint();
            })?;
        Ok(Self { receiver, cancel })
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "已取消导出");
    Ok(())
}
/// Not a link: a reparse point on Windows; `symlink_metadata` already
/// reports a symbolic link as such elsewhere.
fn plain(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !m.file_type().is_symlink()
    }
}
fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && plain(&m))
}
fn compressed_entry(name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file(
        name,
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Zstd)
            .compression_level(Some(3)),
    )?;
    zip.write_all(data)?;
    Ok(zip.finish()?.into_inner())
}
struct Archive {
    zip: ZipWriter<File>,
    charged: u64,
    limit: u64,
}
impl Archive {
    fn append(&mut self, encoded: Vec<u8>, reserve: u64) -> Result<bool> {
        // Sum of self-contained entries includes their central directories and end records;
        // merged archive removes the redundant records, so this is a conservative exact-byte budget.
        if self.charged + encoded.len() as u64 + reserve > self.limit {
            return Ok(false);
        }
        self.charged += encoded.len() as u64;
        self.zip
            .merge_archive(zip::ZipArchive::new(std::io::Cursor::new(encoded))?)?;
        Ok(true)
    }
    fn required(&mut self, name: &str, data: &[u8]) -> Result<()> {
        ensure!(
            self.append(compressed_entry(name, data)?, 0)?,
            "诊断元数据超过压缩包容量"
        );
        Ok(())
    }
}
fn export(
    mut input: Input,
    snapshot: super::logging::Snapshot,
    cancel: &AtomicBool,
) -> Result<Exported> {
    cancelled(cancel)?;
    super::logging::sync_written();
    let mut roots = vec![snapshot.directory.clone()];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local).join("OpenUUYC/logs");
        if path != snapshot.directory {
            roots.push(path);
        }
    }
    if crate::platform::host_service::vault::applies().unwrap_or(false) {
        if let Ok(root) = crate::platform::host_service::vault::root() {
            roots.push(root.join("logs"));
        }
    }
    roots.sort();
    roots.dedup();
    let mut warnings = Vec::new();
    let mut candidates = Vec::new();
    for root in roots {
        if !fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir() && plain(&m)) {
            continue;
        }
        let entries = match fs::read_dir(root) {
            Ok(v) => v,
            Err(_) => {
                warnings.push("一个日志目录无法读取".to_owned());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("openuuyc-") && name.ends_with(".log") || path == snapshot.file)
                || !regular(&path)
            {
                continue;
            }
            if let Ok(m) = entry.metadata() {
                let stamp = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                candidates.push((stamp, path, m.len()));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.dedup_by(|a, b| a.1 == b.1);
    // Hardware collection is read-only and does not enumerate windows or capture content.
    if let Ok(h) = crate::platform::device_profile::Hardware::read() {
        input.private_values.push(h.name);
        input.summary["hardware"] =
            json!({"os":h.os,"cpu":h.cpu,"base_board":h.base_board,"memory_mib":h.memory});
    } else {
        warnings.push("硬件摘要读取失败".into());
    }
    cancelled(cancel)?;
    #[cfg(windows)]
    {
        use crate::platform::windows::components::{self, Kind};
        let mut components = serde_json::Map::new();
        for (label, kind) in [
            ("service", Kind::HostService),
            ("input", Kind::InputDriver),
            ("display", Kind::DisplayDriver),
            ("audio", Kind::AudioDriver),
        ] {
            cancelled(cancel)?;
            components.insert(
                label.into(),
                match components::status(kind) {
                    Ok(s) => json!({"installed":s.installed,"ready":s.ready,"status":s.label}),
                    Err(_) => json!({"status":"unavailable"}),
                },
            );
        }
        input.summary["components"] = components.into();
    }
    input.summary["application"] = json!({"version":env!("CARGO_PKG_VERSION"),"architecture":std::env::consts::ARCH,"logging":snapshot.settings,"dropped_logs":snapshot.dropped});
    for key in ["USERNAME", "USERPROFILE", "USER", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            input.private_values.push(value);
        }
    }
    let redactor = redact::Redactor::new(input.private_values)?;
    redactor.json(&mut input.summary);
    let output = snapshot
        .directory
        .parent()
        .context("诊断输出目录无效")?
        .join("diagnostics");
    fs::create_dir_all(&output)?;
    let name = format!(
        "OpenUUYC-diagnostics-{}-{}.zip",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let path = output.join(name);
    let temporary = path.with_extension("zip.partial");
    let result = (|| -> Result<Exported> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        struct Partial(PathBuf);
        impl Drop for Partial {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let _partial = Partial(temporary.clone());
        let mut archive = Archive {
            zip: ZipWriter::new(file),
            charged: 0,
            limit: ARCHIVE_LIMIT,
        };
        archive.required("summary.json", &serde_json::to_vec_pretty(&input.summary)?)?;
        archive.required("README.txt","本诊断包仅在本机生成，ZIP条目使用Zstandard（zstd）压缩，可用支持Zstandard ZIP的解压工具打开。\n日志按源文件编号与原始字节偏移分段，按段名从小到大拼接恢复顺序。最新日志优先，最终包不超过32MiB；没有压缩前总量、文件数或时间范围限制。\n不含配置文件、凭据、剪贴板、截图、壁纸或转储；脱敏与容量省略见manifest.json。分享前可自行查看。\n".as_bytes())?;
        let mut logs = 0;
        let mut segments = Vec::new();
        let mut coverage = Vec::new();
        let mut full = false;
        let source_count = candidates.len();
        for (index, (stamp, source, size)) in candidates.into_iter().enumerate() {
            cancelled(cancel)?;
            if !regular(&source) {
                warnings.push(format!("日志{}读取前被移除或替换", index + 1));
                continue;
            }
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, 0x00200000);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
            let mut file = match options.open(&source) {
                Ok(f) => f,
                Err(_) => {
                    warnings.push(format!("日志{}无法读取", index + 1));
                    continue;
                }
            };
            if !file.metadata().is_ok_and(|m| m.is_file() && plain(&m)) {
                warnings.push("日志是重解析目标，已跳过".into());
                continue;
            }
            let mut end = size;
            let mut chunk = CHUNK_BYTES;
            let mut included = false;
            while end > 0 {
                cancelled(cancel)?;
                let start = end.saturating_sub(chunk);
                file.seek(SeekFrom::Start(start))?;
                let mut bytes = Vec::with_capacity((end - start) as usize);
                (&mut file).take(end - start).read_to_end(&mut bytes)?;
                if bytes.is_empty() {
                    warnings.push(format!("日志{}导出期间缩短", index + 1));
                    break;
                }
                let skip = if start > 0 {
                    bytes.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1)
                } else {
                    0
                };
                let actual_start = start + skip as u64;
                // An all-newline final piece must still make backward progress.
                if actual_start >= end {
                    end = start;
                    continue;
                }
                let mut sanitized = String::new();
                for line in String::from_utf8_lossy(&bytes[skip..]).lines() {
                    cancelled(cancel)?;
                    if line.len() > 32768 {
                        sanitized.push_str("[已移除超长日志行]\n");
                    } else {
                        sanitized.push_str(&redactor.line(line));
                    }
                }
                let entry = format!("logs/{:04}/{actual_start:020}.log", index + 1);
                let encoded = compressed_entry(&entry, sanitized.as_bytes())?;
                let reserve = 64 * 1024 + (coverage.len() + segments.len() + 1) as u64 * 512;
                if !archive.append(encoded, reserve)? {
                    // Keep the smallest window above two maximum retained log lines.
                    if chunk > 64 * 1024 {
                        chunk /= 2;
                        continue;
                    }
                    full = true;
                    break;
                }
                included = true;
                segments.push(json!({"entry":entry,"begin":actual_start,"end":end}));
                end = actual_start;
                chunk = CHUNK_BYTES;
            }
            if included {
                logs += 1;
            }
            coverage.push(json!({"source":index+1,"modified_utc":chrono::DateTime::<chrono::Utc>::from(stamp).to_rfc3339(),"source_bytes":size,"included_begin":end,"included_end":size,"complete":end==0}));
            if full {
                warnings.push(format!(
                    "达到压缩后32MiB预算：当前日志保留末尾，后续{}个较早日志未包含",
                    source_count - index - 1
                ));
                break;
            }
        }
        if logs == 0 {
            warnings.push("未找到可读取的非空日志".into());
        }
        cancelled(cancel)?;
        archive.required("manifest.json",&serde_json::to_vec_pretty(&json!({"created_utc":chrono::Utc::now().to_rfc3339(),"sources":coverage,"segments":segments,"warnings":warnings,"compression":"zstd","compressed_limit":ARCHIVE_LIMIT,"snapshot":"导出开始时的文件长度与界面状态；其他进程仍可能继续写入。","redaction":"脱敏别名仅本包关联；原始协议/认证与非结构化或超长内容不包含。"}))?)?;
        let file = archive.zip.finish()?;
        ensure!(
            file.metadata()?.len() <= ARCHIVE_LIMIT,
            "诊断包超过压缩后容量限制"
        );

        file.sync_all()?;
        drop(file);
        cancelled(cancel)?;
        fs::rename(&temporary, &path)?;
        Ok(Exported {
            path: path.clone(),
            logs,
            warnings: warnings.len(),
        })
    })();
    result
}
pub(crate) fn open_folder(path: &Path) -> Result<()> {
    let parent = path.parent().context("诊断包路径无效")?;
    super::logging::open_folder(parent)
}
