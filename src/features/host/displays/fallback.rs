//! Machine-owned headless display. It is deliberately absent from session
//! journals/preferences: closing a connection must not unplug the only screen.
use super::*;
#[cfg(windows)]
use crate::platform::display::{
    topology::VirtualProvider,
    virtual_driver::{FALLBACK_ID, Output},
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    width: u32,
    height: u32,
    hz: u32,
}

fn path() -> Result<PathBuf> {
    // Display recovery is independent from account login/authorization. SYSTEM
    // keeps using the machine directory even after the account vault is disabled.
    let root = if crate::platform::host_service::vault::sid(std::process::id())? == "S-1-5-18" {
        crate::platform::host_service::vault::root()?.join("displays")
    } else {
        store::root()?
    };
    Ok(root.join("fallback.json"))
}
fn clear_record() -> Result<()> {
    match std::fs::remove_file(path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
#[cfg(windows)]
pub(super) fn target(output: Output, targets: &[Target]) -> Option<&Target> {
    targets.iter().find(|t| {
        t.adapter == output.adapter
            && t.target == output.target
            && t.adapter_path
                .to_ascii_lowercase()
                .contains("#openuuycdisplay#")
    })
}
#[cfg(windows)]
fn replacement(t: &Target, own: Option<Output>) -> bool {
    t.active
        && t.available
        && t.virtual_provider != Some(VirtualProvider::Uu)
        && own.is_none_or(|o| o.adapter != t.adapter || o.target != t.target)
}
#[cfg(windows)]
pub(crate) fn pin() -> Result<Option<Driver>> {
    if Driver::device_instance().is_err() {
        return Ok(None);
    }
    let driver = Driver::open()?;
    if !driver.persistent_supported() {
        return Ok(None);
    }
    driver.pin_fallback()?;
    Ok(Some(driver))
}
#[cfg(windows)]
pub(super) fn screen(driver: &Driver) -> Result<Option<capture::Screen>> {
    let Some(output) = driver.fallback()? else {
        return Ok(None);
    };
    let targets = Topology::query(true)?.metadata()?;
    let Some(target) = target(output, &targets) else {
        return Ok(None);
    };
    let Some(mut screen) = capture::screens()?
        .into_iter()
        .find(|s| s.identity.as_deref() == Some(target.identity.as_str()))
    else {
        return Ok(None);
    };
    screen.render_adapter = capture::encoding_adapters()?.first().map(|a| a.luid);
    Ok(Some(screen))
}

/// Caller holds DisplayMutation. `ignore` contains only its verified session
/// targets about to be removed; no other driver's target is excluded this way.
#[cfg(windows)]
pub(super) fn ensure(
    driver: &Driver,
    width: u32,
    height: u32,
    ignore: &[String],
    verify_frame: bool,
) -> Result<Option<capture::Screen>> {
    let existing = driver.fallback()?;
    let targets = Topology::query(true)?.metadata()?;
    if targets
        .iter()
        .any(|t| replacement(t, existing) && !ignore.contains(&t.identity))
    {
        return screen(driver);
    }
    ensure!(
        driver.persistent_supported(),
        "无可用普通显示器，请更新虚拟显示驱动以启用常驻兜底屏"
    );
    let saved = store::read::<Record>(&path()?)?;
    let record = saved.unwrap_or(Record {
        width: width.clamp(2, 16384),
        height: height.clamp(2, 16384),
        hz: 144,
    });
    validate_modes(&[(record.width, record.height)])?;
    ensure!((1..=144).contains(&record.hz), "无效兜底屏刷新率");
    store::write(&path()?, &record)?; // Intent survives an owner/driver restart.
    if let Some(screen) = screen(driver)? {
        return Ok(Some(screen));
    }
    // Don't change an adapter underneath an existing manual display.
    if existing.is_none()
        && !targets
            .iter()
            .any(|t| t.virtual_provider == Some(VirtualProvider::OpenUuyc))
    {
        let gpu = capture::encoding_adapters()?
            .first()
            .cloned()
            .context("未找到虚拟显示渲染适配器")?;
        driver.render_adapter(gpu.luid)?;
    }
    let output = driver.add_fallback(record.width, record.height, record.hz)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let identity = loop {
        let targets = Topology::query(false)?.metadata()?;
        if let Some(target) = target(output, &targets) {
            break target.identity.clone();
        }
        ensure!(Instant::now() < deadline, "常驻虚拟屏创建后未出现");
        std::thread::sleep(Duration::from_millis(25));
    };
    // Re-read after arrival: keep newly connected physical monitors and do not
    // re-activate an official screen that its owner already reclaimed.
    let mut layout = Topology::query(true)?.snapshot()?;
    if !layout.targets.iter().any(|t| t.identity == identity) {
        let left = layout
            .targets
            .iter()
            .map(|t| i64::from(t.left) + i64::from(t.width))
            .max()
            .unwrap_or(0);
        layout.targets.push(SavedTarget {
            identity: identity.clone(),
            source_group: format!("fallback/{FALLBACK_ID}"),
            left: i32::try_from(left)?,
            top: 0,
            width: record.width,
            height: record.height,
            rotation: 1,
            refresh_numerator: record.hz,
            refresh_denominator: 1,
            scan_line_ordering: 1,
        });
        layout.apply()?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let selected = loop {
        if let Some(screen) = screen(driver)? {
            break screen;
        }
        ensure!(Instant::now() < deadline, "常驻虚拟屏尚未成为可用采集目标");
        std::thread::sleep(Duration::from_millis(25));
    };
    if !verify_frame {
        return Ok(Some(selected));
    }
    // Prove an actual desktop frame; official cleanup is its own autonomous
    // action, so a failure never tears down the new sole target as a rollback.
    let _runtime = crate::features::host::encoder::Runtime::new()?;
    let mut desktop = capture::Desktop::open_selected(&selected)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if desktop.next(20, 2, false, false, (1280, 720))?.is_some() && desktop.available {
            tracing::info!(screen=%identity,backend=desktop.backend_name(), "persistent headless display ready");
            return Ok(Some(selected));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("常驻虚拟屏已建立，但尚未得到桌面画面；保留屏幕等待恢复")
}

#[cfg(windows)]
pub(super) fn retire(driver: &Driver, replacement_identity: Option<&str>) -> Result<bool> {
    let (output, users) = driver.fallback_state()?;
    if users > u32::from(driver.owns_fallback_pin()) {
        return Ok(false);
    }
    let Some(output) = output else {
        clear_record()?;
        return Ok(true);
    };
    let targets = Topology::query(true)?.metadata()?;
    if !targets.iter().any(|t| {
        replacement(t, Some(output)) && replacement_identity.is_none_or(|id| id == t.identity)
    }) {
        return Ok(false);
    }
    // The driver checks live per-handle pins atomically with removal. A second
    // connection appearing after this snapshot cannot lose its fallback.
    if let Err(error) = driver.remove(FALLBACK_ID) {
        if crate::platform::display::virtual_driver::busy(&error) {
            return Ok(false);
        }
        return Err(error);
    }
    clear_record()?;
    tracing::info!("persistent headless display retired after replacement became ready");
    Ok(true)
}

#[cfg(windows)]
#[derive(Default)]
pub(crate) struct Maintainer {
    returning: Option<(String, Instant)>,
    protocol_retry: Option<Instant>,
}
#[cfg(windows)]
impl Maintainer {
    pub(crate) fn tick(&mut self) -> Result<()> {
        if self.protocol_retry.is_some_and(|t| Instant::now() < t) {
            return Ok(());
        }
        if Driver::device_instance().is_err() {
            self.returning = None;
            return Ok(());
        }
        let _serial = display::recovery::Serial::acquire()?;
        let driver = Driver::open()?;
        if !driver.persistent_supported() {
            // The old driver's version query itself renews its global watchdog.
            // Do not poll it frequently and accidentally keep orphaned screens alive.
            self.protocol_retry = Some(Instant::now() + Duration::from_secs(30));
            return Ok(());
        }
        self.protocol_retry = None;
        let (own, users) = driver.fallback_state()?;
        if own.is_none() && !path()?.is_file() {
            self.returning = None;
            return Ok(());
        }
        // An active connection owns the capture handoff, including super-screen
        // restoration. File cleanup releases pins even after a process crash.
        if users != 0 {
            self.returning = None;
            return Ok(());
        }
        let targets = Topology::query(true)?.metadata()?;
        let other = targets.iter().find(|t| replacement(t, own));
        if let Some(other) = other {
            let now = Instant::now();
            let (id, since) = self
                .returning
                .get_or_insert_with(|| (other.identity.clone(), now));
            if id != &other.identity {
                *id = other.identity.clone();
                *since = now;
            }
            if now.duration_since(*since) >= Duration::from_secs(2) {
                retire(&driver, Some(&other.identity))?;
            }
        } else {
            self.returning = None;
            if own.is_some_and(|output| {
                target(output, &targets).is_some_and(|t| t.active && t.available)
            }) {
                // An idle, already active fallback needs neither a capture
                // inventory nor another durable write of the same intent.
                return Ok(());
            }
            if own.is_some() || store::read::<Record>(&path()?)?.is_some() {
                ensure(&driver, 1920, 1080, &[], false)?;
            }
        }
        Ok(())
    }
}
#[cfg(windows)]
pub(crate) fn start_background() {
    // Installed mode has one account-independent service agent. A portable GUI
    // needs a local maintainer only while that service is unavailable.
    if crate::platform::host_service::install::running().unwrap_or(false) {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("display-fallback".into())
            .spawn(|| {
                let mut owner = Maintainer::default();
                let mut last_error = String::new();
                loop {
                    if crate::platform::host_service::install::running().unwrap_or(false) {
                        owner.returning = None;
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    match owner.tick() {
                        Ok(()) => last_error.clear(),
                        Err(e) => {
                            let e = format!("{e:#}");
                            if e != last_error {
                                tracing::warn!(error=%e,"headless display maintenance deferred");
                                last_error = e;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
    });
}

// ---------------------------------------------------------------------------
// Linux：兜底屏是一路 VKMS 连接器加合成器接管，不是驱动里的常驻显示器
// ---------------------------------------------------------------------------

/// Linux 上「机器所有」只能做到进程级：辅助程序把连接器租给这条连接，进程一退它
/// 就收回去（见 `platform/linux/virtual_display.rs`）。断线不退还——连接结束时只是
/// 放弃使用权，屏本身留到进程退出。
#[cfg(not(windows))]
struct Machine {
    session: crate::platform::virtual_display::Session,
    connector: String,
    identity: String,
    owners: std::collections::HashSet<String>,
}

#[cfg(not(windows))]
static MACHINE: std::sync::LazyLock<std::sync::Mutex<Option<Machine>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(not(windows))]
impl Machine {
    fn present(&self) -> Result<Option<capture::Screen>> {
        let screens = match capture::screens() {
            Ok(screens) => screens,
            // 零输出的机器上枚举本身就是失败，这里只是暂时没看见。
            Err(_) => return Ok(None),
        };
        Ok(screens
            .into_iter()
            .find(|s| s.identity.as_deref() == Some(self.identity.as_str())))
    }

    /// 等合成器与采集都把新输出报出来。零输出时枚举会失败，等它就行。
    fn wait(&mut self) -> Result<capture::Screen> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut last = None;
            match capture::screens() {
                Ok(screens) => {
                    if let Some(screen) = screens
                        .into_iter()
                        .find(|s| s.identity.as_deref() == Some(self.identity.as_str()))
                    {
                        return Ok(screen);
                    }
                }
                Err(error) => last = Some(format!("{error:#}")),
            }
            ensure!(
                Instant::now() < deadline,
                "{}",
                last.map_or_else(
                    || "兜底虚拟屏创建后没有出现在桌面里".to_owned(),
                    |error| format!("兜底虚拟屏创建后没有出现在桌面里：{error}")
                )
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(not(windows))]
fn verify_frame(screen: &capture::Screen) -> Result<()> {
    let _runtime = crate::features::host::encoder::Runtime::new()?;
    let mut desktop = capture::Desktop::open_selected(screen)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if desktop.next(20, 2, false, false, (1280, 720))?.is_some() && desktop.available {
            tracing::info!(
                screen = ?screen.identity,
                backend = desktop.backend_name(),
                "persistent headless display ready"
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("兜底虚拟屏已建立，但尚未得到桌面画面；保留屏幕等待恢复")
}

/// 给兜底屏换分辨率。VKMS 的模式表里没有这些尺寸，只能让合成器现造一个模式；
/// 换过的尺寸记下来，下一次按它重建。
#[cfg(not(windows))]
pub(super) fn resize(identity: &str, width: u32, height: u32) -> Result<capture::Screen> {
    validate_modes(&[(width, height)])?;
    {
        let machine = lock(&MACHINE);
        let current = machine.as_ref().context("兜底屏已不在")?;
        ensure!(current.identity == identity, "不是兜底屏");
        crate::platform::display::compositor::set_mode(
            crate::platform::display::topology::output_name(identity),
            width,
            height,
            Some(144),
        )?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(screens) = capture::screens()
            && let Some(screen) = screens
                .into_iter()
                .find(|s| s.identity.as_deref() == Some(identity))
            && (screen.width, screen.height) == (width, height)
        {
            let mut record = store::read::<Record>(&path()?)?.unwrap_or(Record {
                width,
                height,
                hz: 144,
            });
            record.width = width;
            record.height = height;
            store::write(&path()?, &record)?;
            return Ok(screen);
        }
        ensure!(Instant::now() < deadline, "兜底屏没有切到请求的分辨率");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
pub(super) fn ensure(
    token: &str,
    width: u32,
    height: u32,
    verify: bool,
) -> Result<Option<capture::Screen>> {
    let mut machine = lock(&MACHINE);
    let mut owners = std::collections::HashSet::new();
    if let Some(current) = machine.as_mut() {
        if let Some(screen) = current.present()? {
            current.owners.insert(token.to_owned());
            if verify {
                verify_frame(&screen)?;
            }
            return Ok(Some(screen));
        }
        // 辅助程序重启、合成器丢了这块输出都会走到这里：旧屏已经不在桌面里，
        // 重新建一块，正在用它的其他连接跟着搬过去。先显式还一次，免得辅助程序
        // 还没处理完连接断开就多租了一块出去。
        let _ = current.session.remove(&current.connector);
        owners = std::mem::take(&mut current.owners);
        *machine = None;
    }
    // 合成器接不住就别去惊动内核（见 `compositor::ensure_available`）。
    crate::platform::display::compositor::ensure_available()?;
    ensure!(
        openuuyc_vkms::installed(),
        "没有安装虚拟显示器辅助程序（openuuyc-vkms），无法为无屏的被控端添加兜底屏"
    );
    let saved = store::read::<Record>(&path()?)?;
    let record = saved.unwrap_or(Record {
        width: width.clamp(2, 16384),
        height: height.clamp(2, 16384),
        hz: 144,
    });
    validate_modes(&[(record.width, record.height)])?;
    ensure!((1..=144).contains(&record.hz), "无效兜底屏刷新率");
    // 先记尺寸：辅助程序或进程重启之后按同一块尺寸再建。
    store::write(&path()?, &record)?;
    let mut session = crate::platform::virtual_display::Session::open()?;
    let connector = session.create(record.width, record.height, record.hz)?;
    let identity = crate::platform::display::topology::identity(&connector);
    let mut created = Machine {
        session,
        connector,
        identity,
        owners,
    };
    created.owners.insert(token.to_owned());
    let screen = match created.wait() {
        Ok(screen) => screen,
        Err(error) => {
            if let Err(rollback) = created.session.remove(&created.connector) {
                tracing::error!(%rollback, "兜底虚拟屏回滚失败");
            }
            return Err(error);
        }
    };
    // 和 Windows 一样：屏已经建出来就留着等恢复，别把唯一一块屏回滚掉。
    let verified = if verify {
        verify_frame(&screen)
    } else {
        Ok(())
    };
    *machine = Some(created);
    verified.map(|()| Some(screen))
}

#[cfg(not(windows))]
pub(super) fn screen() -> Result<Option<capture::Screen>> {
    match lock(&MACHINE).as_ref() {
        Some(current) => current.present(),
        None => Ok(None),
    }
}

/// 兜底屏在桌面里的 identity，`annotate` 靠它把这块屏报成虚拟屏。
#[cfg(not(windows))]
pub(super) fn identity() -> Option<String> {
    lock(&MACHINE).as_ref().map(|m| m.identity.clone())
}

#[cfg(not(windows))]
pub(super) fn allows_retirement(token: &str) -> bool {
    lock(&MACHINE)
        .as_ref()
        .is_some_and(|m| m.owners.len() <= 1 && m.owners.contains(token))
}

#[cfg(not(windows))]
pub(super) fn retire(token: &str, replacement_identity: &str) -> Result<bool> {
    let mut machine = lock(&MACHINE);
    let Some(current) = machine.as_mut() else {
        return Ok(false);
    };
    if current.owners.len() > 1 || !current.owners.contains(token) {
        return Ok(false);
    }
    // 屏其实已经不在了（辅助程序重启过、连接断过）：没有东西要还，按已回收算，
    // 否则这条记录会一直挡着后续的回收。
    if current.present()?.is_none() {
        let identity = current.identity.clone();
        *machine = None;
        clear_record()?;
        tracing::info!(%identity, "persistent headless display was already gone");
        return Ok(true);
    }
    // 只有替代屏真的还在桌面里才回收；调用方已经确认它出过帧。
    let Ok(screens) = capture::screens() else {
        return Ok(false);
    };
    if !screens
        .iter()
        .any(|s| s.identity.as_deref() == Some(replacement_identity))
    {
        return Ok(false);
    }
    if let Err(error) = current.session.remove(&current.connector) {
        tracing::warn!(%error, "兜底虚拟屏回收失败");
        return Ok(false);
    }
    *machine = None;
    clear_record()?;
    tracing::info!(replacement = %replacement_identity, "persistent headless display retired after replacement became ready");
    Ok(true)
}

/// 一条连接开始使用已经存在的兜底屏时登记使用权。
#[cfg(not(windows))]
pub(super) fn claim(token: &str) {
    if let Some(current) = lock(&MACHINE).as_mut() {
        current.owners.insert(token.to_owned());
    }
}

/// 一条连接结束了：只是放弃它的使用权，屏本身要留到进程退出。
#[cfg(not(windows))]
pub(super) fn release(token: &str) {
    if let Some(current) = lock(&MACHINE).as_mut() {
        current.owners.remove(token);
    }
}

/// Linux 的兜底屏跟着进程活，没有需要后台维护的机器级状态。
#[cfg(not(windows))]
pub(crate) fn start_background() {}
