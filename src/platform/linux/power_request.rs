//! Keeps the display on and the machine out of idle sleep while a screen is
//! captured or a viewing session is live, like the Windows power request. The desktop's idle
//! policy is reached through `org.freedesktop.ScreenSaver.Inhibit` on the
//! session bus (GNOME, KDE, Xfce, hypridle and others honour it); the
//! inhibition is also released if the bus connection closes.
use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};

pub(crate) struct Request {
    connection: Connection,
    cookie: u32,
}

impl Request {
    pub(crate) fn capture() -> Result<Self> {
        Self::new("OpenUUYC 正在采集远程桌面")
    }

    pub(crate) fn viewing() -> Result<Self> {
        Self::new("OpenUUYC 正在进行远程观看会话")
    }

    fn new(reason: &str) -> Result<Self> {
        let connection = Connection::session().context("连接会话总线失败")?;
        let cookie: u32 = screensaver(&connection)?
            .call("Inhibit", &("OpenUUYC", reason))
            .context("桌面未接受阻止息屏与自动休眠的请求")?;
        tracing::debug!(cookie, "session display/idle inhibition acquired");
        Ok(Self { connection, cookie })
    }
}

fn screensaver(connection: &Connection) -> Result<Proxy<'static>> {
    Proxy::new(
        connection,
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    )
    .context("桌面未提供 org.freedesktop.ScreenSaver 接口")
}

impl Drop for Request {
    fn drop(&mut self) {
        let released = screensaver(&self.connection).and_then(|proxy| {
            proxy
                .call::<_, _, ()>("UnInhibit", &(self.cookie,))
                .context("UnInhibit")
        });
        match released {
            Ok(()) => tracing::debug!("session display/idle inhibition released"),
            // Closing the connection with this value releases it anyway.
            Err(error) => tracing::warn!(error = %format!("{error:#}"),
                "session display/idle inhibition release failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Needs a desktop session bus. On GNOME, also checks the session
    /// manager lists this idle inhibition only while the request is held.
    #[test]
    #[ignore]
    fn viewing_holds_the_idle_inhibition_until_dropped() {
        use zbus::{blocking::Proxy, zvariant::OwnedObjectPath};
        let connection = zbus::blocking::Connection::session().expect("session bus");
        let ours = || -> Option<usize> {
            let manager = Proxy::new(
                &connection,
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                "org.gnome.SessionManager",
            )
            .ok()?;
            let inhibitors: Vec<OwnedObjectPath> = manager.call("GetInhibitors", &()).ok()?;
            Some(
                inhibitors
                    .into_iter()
                    .filter(|path| {
                        Proxy::new(
                            &connection,
                            "org.gnome.SessionManager",
                            path.as_ref(),
                            "org.gnome.SessionManager.Inhibitor",
                        )
                        .and_then(|inhibitor| {
                            let reason: String = inhibitor.call("GetReason", &())?;
                            let flags: u32 = inhibitor.call("GetFlags", &())?;
                            Ok(reason == "OpenUUYC 正在进行远程观看会话" && flags & 8 != 0)
                        })
                        .unwrap_or(false)
                    })
                    .count(),
            )
        };
        let before = ours();
        let request = super::Request::viewing().expect("inhibit");
        let held = ours();
        drop(request);
        let after = ours();
        println!(
            "GNOME idle inhibitors from us: before {before:?}, held {held:?}, after {after:?}"
        );
        if let (Some(before), Some(held), Some(after)) = (before, held, after) {
            assert_eq!(held, before + 1);
            assert_eq!(after, before);
        }
    }
}
