use gpui::*;
use crate::app::Workspace;

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    use crate::settings::{group, row, desc, tr};

    #[derive(Default)]
    pub struct SyncUiState;

    impl SyncUiState {
        pub fn new(_: &mut Window, _: &mut Context<Workspace>) -> Self { Self }
    }

    impl Workspace {
        pub(crate) fn sync_ui_enter(&mut self, _: &mut Context<Self>) {}

        pub(crate) fn sync_page(&self, _: &mut Context<Self>) -> AnyElement {
            div().w_full().flex().flex_col().gap(px(24.))
                .child(group(
                    Some(tr("云同步", "Cloud Sync")),
                    vec![row(
                        tr("桌面端功能", "Desktop only"),
                        desc(tr("多端实时云同步仅在 Quiet.md 桌面版中可用。", "Multi-device cloud sync is available in the Quiet.md desktop app.")),
                        div(),
                    ).into_any_element()],
                ))
                .into_any_element()
        }
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;
    use crate::input::{TextField, TextFieldEvent};
    use crate::settings::{button, desc, group, link_button, row, segmented, switch, tr};
    use crate::theme::{self, Theme};
    use crate::ui::{self, icon};
    use gpui::prelude::FluentBuilder;
    use quiet_sync_client::{SyncHandle, SyncScope, SyncStatus, VaultUnlock};
    use quiet_sync_proto::api::{DeviceInfo, StorageUsageResp, VaultSummary};
    use std::future::Future;
    use std::pin::Pin;

    const SYNC_INTERVAL_PRESETS: [u64; 5] = [0, 10, 30, 60, 300];

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SyncField {
        ServerUrl,
        Email,
        Password,
        DisplayName,
        InviteCode,
        DeviceName,
        NewVaultName,
        E2eePass,
        E2eeConfirm,
    }

    pub struct SyncUiState {
        server_url: Entity<TextField>,
        email: Entity<TextField>,
        password: Entity<TextField>,
        display_name: Entity<TextField>,
        invite_code: Entity<TextField>,
        device_name: Entity<TextField>,
        new_vault_name: Entity<TextField>,
        e2ee_pass: Entity<TextField>,
        e2ee_confirm: Entity<TextField>,
        unlock_secret: Entity<TextField>,
        rotate_new_pass: Entity<TextField>,
        register_mode: bool,
        new_vault_e2ee: bool,
        unlock_target: Option<VaultSummary>,
        unlock_with_recovery: bool,
        busy: bool,
        notice: Option<(bool, String)>,
        vaults: Vec<VaultSummary>,
        devices: Vec<DeviceInfo>,
        storage_usage: Option<StorageUsageResp>,
        generated_recovery_key: Option<String>,
        show_recovery_key: bool,
        show_rotate_form: bool,
        confirm_unlink: bool,
        confirm_logout: bool,
        _subs: Vec<Subscription>,
    }

    fn default_device_name() -> String {
        if let Ok(h) = std::env::var("HOSTNAME")
            && !h.trim().is_empty()
        {
            return h.trim().to_string();
        }
        match std::env::consts::OS {
            "macos" => "Mac".into(),
            "windows" => "Windows PC".into(),
            "linux" => "Linux".into(),
            other => other.into(),
        }
    }

    fn make_field(cx: &mut Context<Workspace>, ph: &str, masked: bool, initial: Option<&str>) -> Entity<TextField> {
        cx.new(|cx| {
            let mut f = TextField::new(ph.to_string(), cx);
            f.placeholder_color = Theme::text_disabled();
            f.masked = masked;
            f.tab_events = true;
            if let Some(v) = initial {
                f.set_text(v, cx);
            }
            f
        })
    }

    impl SyncUiState {
        pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
            let existing_creds = crate::sync::handle(cx).and_then(|h| h.credentials());
            let initial_url = existing_creds.as_ref().map(|c| c.server_url.as_str());
            let initial_email = existing_creds.as_ref().map(|c| c.email.as_str());
            let initial_dev = existing_creds.as_ref().map(|c| c.device_name.clone()).unwrap_or_else(default_device_name);

            let server_url = make_field(cx, "https://sync.example.com", false, initial_url);
            let email = make_field(cx, "you@example.com", false, initial_email);
            let password = make_field(cx, tr("至少 8 位密码", "At least 8 characters"), true, None);
            let display_name = make_field(cx, tr("昵称", "Display name"), false, None);
            let invite_code = make_field(cx, tr("可选（如服务器启用注册邀请）", "Optional"), false, None);
            let device_name = make_field(cx, tr("设备名称", "Device name"), false, Some(&initial_dev));
            let new_vault_name = make_field(cx, tr("例如：我的笔记库", "e.g. Personal Notes"), false, None);
            let e2ee_pass = make_field(cx, tr("设置端到端加密口令（至少 8 位）", "Passphrase (8+ chars)"), true, None);
            let e2ee_confirm = make_field(cx, tr("再次输入口令", "Confirm passphrase"), true, None);
            let unlock_secret = make_field(cx, tr("输入口令或恢复密钥", "Passphrase or QMD1-... recovery key"), true, None);
            let rotate_new_pass = make_field(cx, tr("新的端到端加密口令（至少 8 位）", "New passphrase (8+ chars)"), true, None);

            let subs = [
                (&server_url, SyncField::ServerUrl),
                (&email, SyncField::Email),
                (&password, SyncField::Password),
                (&display_name, SyncField::DisplayName),
                (&invite_code, SyncField::InviteCode),
                (&device_name, SyncField::DeviceName),
                (&new_vault_name, SyncField::NewVaultName),
                (&e2ee_pass, SyncField::E2eePass),
                (&e2ee_confirm, SyncField::E2eeConfirm),
            ]
            .into_iter()
            .map(|(f, kind)| {
                cx.subscribe_in(f, window, move |w, _, ev: &TextFieldEvent, window, cx| {
                    w.on_sync_field_event(kind, ev, window, cx);
                })
            })
            .collect();

            Self {
                server_url,
                email,
                password,
                display_name,
                invite_code,
                device_name,
                new_vault_name,
                e2ee_pass,
                e2ee_confirm,
                unlock_secret,
                rotate_new_pass,
                register_mode: false,
                new_vault_e2ee: true,
                unlock_target: None,
                unlock_with_recovery: false,
                busy: false,
                notice: None,
                vaults: Vec::new(),
                devices: Vec::new(),
                storage_usage: None,
                generated_recovery_key: None,
                show_recovery_key: false,
                show_rotate_form: false,
                confirm_unlink: false,
                confirm_logout: false,
                _subs: subs,
            }
        }

        fn field(&self, kind: SyncField) -> &Entity<TextField> {
            match kind {
                SyncField::ServerUrl => &self.server_url,
                SyncField::Email => &self.email,
                SyncField::Password => &self.password,
                SyncField::DisplayName => &self.display_name,
                SyncField::InviteCode => &self.invite_code,
                SyncField::DeviceName => &self.device_name,
                SyncField::NewVaultName => &self.new_vault_name,
                SyncField::E2eePass => &self.e2ee_pass,
                SyncField::E2eeConfirm => &self.e2ee_confirm,
            }
        }
    }

    fn input_box(field: &Entity<TextField>, w_px: f32) -> Div {
        div()
            .w(px(w_px))
            .h(px(30.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(7.))
            .bg(Theme::bg_body())
            .border_1()
            .border_color(Theme::line_border())
            .child(field.clone())
    }

    fn full_input_box(field: &Entity<TextField>) -> Div {
        div()
            .w_full()
            .h(px(32.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(7.))
            .bg(Theme::bg_body())
            .border_1()
            .border_color(Theme::line_border())
            .child(field.clone())
    }

    fn primary_btn(id: impl Into<ElementId>, label: &'static str, disabled: bool) -> Stateful<Div> {
        div()
            .id(id)
            .flex_none()
            .h(px(28.))
            .px(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.))
            .when(!disabled, |d| d.cursor_pointer().bg(Theme::primary()).text_color(gpui::white()).hover(|s| s.opacity(0.92)))
            .when(disabled, |d| d.bg(Theme::fill_hover()).text_color(Theme::text_disabled()))
            .text_size(px(theme::ui_font()))
            .line_height(px(theme::ui_font() + 5.))
            .child(label)
    }

    fn run_sync_op<T: Send + 'static>(
        w: &mut Workspace,
        cx: &mut Context<Workspace>,
        fut: impl FnOnce(SyncHandle) -> Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>> + Send + 'static,
        on_done: impl FnOnce(&mut Workspace, anyhow::Result<T>, &mut Context<Workspace>) + 'static,
    ) {
        if w.sync_ui.busy {
            return;
        }
        let Some(handle) = crate::sync::ensure_handle(cx) else {
            w.sync_ui.notice = Some((true, tr("同步服务尚未初始化", "Sync service is unavailable").into()));
            cx.notify();
            return;
        };
        w.sync_ui.busy = true;
        w.sync_ui.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { handle.block_on_bg(fut) }).await;
            let _ = this.update(cx, |w, cx| {
                w.sync_ui.busy = false;
                on_done(w, res, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn hhmm(secs: u64) -> String {
        let (_, _, _, h, mi) = crate::time::civil(secs);
        format!("{h:02}:{mi:02}")
    }

    fn format_bytes(bytes: u64) -> String {
        const KB: f64 = 1024.;
        const MB: f64 = KB * 1024.;
        const GB: f64 = MB * 1024.;
        let b = bytes as f64;
        if b >= GB {
            format!("{:.2} GB", b / GB)
        } else if b >= MB {
            format!("{:.1} MB", b / MB)
        } else if b >= KB {
            format!("{:.1} KB", b / KB)
        } else {
            format!("{bytes} B")
        }
    }

    impl Workspace {
        fn on_sync_field_event(&mut self, field: SyncField, ev: &TextFieldEvent, window: &mut Window, cx: &mut Context<Self>) {
            match ev {
                TextFieldEvent::Tab | TextFieldEvent::ShiftTab => {
                    let order: &[SyncField] = match field {
                        SyncField::ServerUrl
                        | SyncField::Email
                        | SyncField::Password
                        | SyncField::DisplayName
                        | SyncField::InviteCode
                        | SyncField::DeviceName => {
                            if self.sync_ui.register_mode {
                                &[
                                    SyncField::ServerUrl,
                                    SyncField::Email,
                                    SyncField::Password,
                                    SyncField::DisplayName,
                                    SyncField::InviteCode,
                                    SyncField::DeviceName,
                                ]
                            } else {
                                &[
                                    SyncField::ServerUrl,
                                    SyncField::Email,
                                    SyncField::Password,
                                    SyncField::DeviceName,
                                ]
                            }
                        }
                        SyncField::NewVaultName | SyncField::E2eePass | SyncField::E2eeConfirm => {
                            if self.sync_ui.new_vault_e2ee {
                                &[SyncField::NewVaultName, SyncField::E2eePass, SyncField::E2eeConfirm]
                            } else {
                                &[SyncField::NewVaultName]
                            }
                        }
                    };
                    if order.len() <= 1 {
                        return;
                    }
                    let Some(pos) = order.iter().position(|f| *f == field) else { return };
                    let next_pos = if matches!(ev, TextFieldEvent::Tab) {
                        (pos + 1) % order.len()
                    } else {
                        (pos + order.len() - 1) % order.len()
                    };
                    let target = self.sync_ui.field(order[next_pos]).clone();
                    target.update(cx, |f, cx| f.select_all_text(cx));
                    let fh = target.read(cx).focus_handle(cx);
                    window.focus(&fh, cx);
                    cx.notify();
                }
                TextFieldEvent::Submit => match field {
                    SyncField::Email
                    | SyncField::Password
                    | SyncField::DisplayName
                    | SyncField::InviteCode
                    | SyncField::DeviceName
                        if !self.sync_ui.busy =>
                    {
                        self.sync_submit_auth(cx);
                    }
                    SyncField::NewVaultName | SyncField::E2eePass | SyncField::E2eeConfirm if !self.sync_ui.busy => {
                        self.sync_create_vault(cx);
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        pub(crate) fn sync_ui_enter(&mut self, cx: &mut Context<Self>) {
            let Some(handle) = crate::sync::handle(cx) else { return };
            let Some(creds) = handle.credentials() else { return };
            self.sync_ui.server_url.update(cx, |f, cx| {
                if f.text.is_empty() { f.set_text(&creds.server_url, cx); }
            });
            self.sync_ui.email.update(cx, |f, cx| {
                if f.text.is_empty() { f.set_text(&creds.email, cx); }
            });
            self.sync_ui.device_name.update(cx, |f, cx| {
                if f.text.is_empty() { f.set_text(&creds.device_name, cx); }
            });
            if creds.vault_id.is_some() {
                self.fetch_sync_devices(cx);
            } else {
                self.fetch_sync_vaults(cx);
            }
        }

        fn fetch_sync_vaults(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| Box::pin(async move { h.list_vaults().await }),
                |w, res, _| match res {
                    Ok(v) => w.sync_ui.vaults = v,
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("获取文档库列表失败", "Could not load vaults")))),
                },
            );
        }

        fn fetch_sync_devices(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| {
                    Box::pin(async move {
                        let devices = h.list_devices().await?;
                        let storage = h.storage_usage().await.ok();
                        Ok((devices, storage))
                    })
                },
                |w, res, _| match res {
                    Ok((d, storage)) => {
                        w.sync_ui.devices = d;
                        if storage.is_some() {
                            w.sync_ui.storage_usage = storage;
                        }
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("获取设备列表失败", "Could not load devices")))),
                },
            );
        }

        fn fetch_sync_storage(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| Box::pin(async move { h.storage_usage().await }),
                |w, res, _| match res {
                    Ok(u) => w.sync_ui.storage_usage = Some(u),
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("查询云端空间失败", "Could not load cloud storage")))),
                },
            );
        }

        fn sync_test_server(&mut self, cx: &mut Context<Self>) {
            let url = self.sync_ui.server_url.read(cx).text.trim().to_string();
            if url.is_empty() {
                self.sync_ui.notice = Some((true, tr("请输入服务器地址", "Enter server URL").into()));
                cx.notify();
                return;
            }
            run_sync_op(
                self,
                cx,
                move |h| Box::pin(async move { h.check_server(&url).await }),
                |w, res, _| match res {
                    Ok(()) => w.sync_ui.notice = Some((false, tr("服务器连接正常", "Server is reachable").into())),
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("连接失败", "Connection failed")))),
                },
            );
        }

        fn sync_submit_auth(&mut self, cx: &mut Context<Self>) {
            let url = self.sync_ui.server_url.read(cx).text.trim().to_string();
            let email = self.sync_ui.email.read(cx).text.trim().to_string();
            let password = self.sync_ui.password.read(cx).text.clone();
            let display_name = self.sync_ui.display_name.read(cx).text.trim().to_string();
            let invite = self.sync_ui.invite_code.read(cx).text.trim().to_string();
            let device_name = self.sync_ui.device_name.read(cx).text.trim().to_string();
            let dev = if device_name.is_empty() { default_device_name() } else { device_name };

            if url.is_empty() || email.is_empty() || password.is_empty() {
                self.sync_ui.notice = Some((true, tr("请填写服务器地址、邮箱和密码", "Enter server URL, email and password").into()));
                cx.notify();
                return;
            }
            if self.sync_ui.register_mode && password.len() < 8 {
                self.sync_ui.notice = Some((true, tr("密码至少需要 8 位字符", "Password must be at least 8 characters").into()));
                cx.notify();
                return;
            }
            let is_reg = self.sync_ui.register_mode;
            run_sync_op(
                self,
                cx,
                move |h| {
                    Box::pin(async move {
                        if is_reg {
                            let name = if display_name.is_empty() {
                                email.split('@').next().unwrap_or("User").to_string()
                            } else {
                                display_name
                            };
                            let inv = (!invite.is_empty()).then_some(invite);
                            h.register(&url, &email, &password, &name, &dev, inv).await?;
                        } else {
                            h.login(&url, &email, &password, &dev).await?;
                        }
                        h.list_vaults().await
                    })
                },
                |w, res, cx| match res {
                    Ok(vaults) => {
                        w.sync_ui.vaults = vaults;
                        w.sync_ui.password.update(cx, |f, cx| f.set_text("", cx));
                        w.sync_ui.notice = Some((false, tr("登录成功，请选择或创建云端文档库", "Signed in. Select or create a vault.").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("认证失败", "Authentication failed")))),
                },
            );
        }

        fn sync_create_vault(&mut self, cx: &mut Context<Self>) {
            let name = self.sync_ui.new_vault_name.read(cx).text.trim().to_string();
            if name.is_empty() {
                self.sync_ui.notice = Some((true, tr("请输入文档库名称", "Enter a vault name").into()));
                cx.notify();
                return;
            }
            let pass = if self.sync_ui.new_vault_e2ee {
                let p = self.sync_ui.e2ee_pass.read(cx).text.clone();
                let c = self.sync_ui.e2ee_confirm.read(cx).text.clone();
                if p.chars().count() < 8 {
                    self.sync_ui.notice = Some((true, tr("端到端加密口令至少需要 8 个字符", "Passphrase must be at least 8 characters").into()));
                    cx.notify();
                    return;
                }
                if p != c {
                    self.sync_ui.notice = Some((true, tr("两次输入的加密口令不一致", "Passphrases do not match").into()));
                    cx.notify();
                    return;
                }
                Some(p)
            } else {
                None
            };

            run_sync_op(
                self,
                cx,
                move |h| {
                    Box::pin(async move {
                        let (vault, recovery) = h.create_vault(&name, pass).await?;
                        let devices = h.list_devices().await.unwrap_or_default();
                        Ok((vault, recovery, devices))
                    })
                },
                |w, res, cx| match res {
                    Ok((_, recovery, devices)) => {
                        w.sync_ui.devices = devices;
                        w.sync_ui.generated_recovery_key = recovery;
                        w.sync_ui.e2ee_pass.update(cx, |f, cx| f.set_text("", cx));
                        w.sync_ui.e2ee_confirm.update(cx, |f, cx| f.set_text("", cx));
                        w.sync_ui.notice = Some((false, tr("已创建并关联云端文档库，正在初次同步…", "Vault created and linked. Initial sync started.").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("创建失败", "Could not create vault")))),
                },
            );
        }

        fn sync_connect_vault(&mut self, vault: VaultSummary, unlock: Option<VaultUnlock>, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                move |h| {
                    Box::pin(async move {
                        h.link_vault(vault, unlock).await?;
                        h.list_devices().await
                    })
                },
                |w, res, cx| match res {
                    Ok(devices) => {
                        w.sync_ui.devices = devices;
                        w.sync_ui.unlock_target = None;
                        w.sync_ui.unlock_secret.update(cx, |f, cx| f.set_text("", cx));
                        w.sync_ui.notice = Some((false, tr("已连接到云端文档库，正在同步…", "Linked to vault. Syncing…").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("连接文档库失败", "Could not link vault")))),
                },
            );
        }

        fn sync_rotate_e2ee(&mut self, cx: &mut Context<Self>) {
            let new = self.sync_ui.rotate_new_pass.read(cx).text.clone();
            if new.chars().count() < 8 {
                self.sync_ui.notice = Some((true, tr("新口令至少需要 8 个字符", "New passphrase must be at least 8 characters").into()));
                cx.notify();
                return;
            }
            run_sync_op(
                self,
                cx,
                move |h| Box::pin(async move {
                    h.rotate_e2ee_passphrase(&new).await?;
                    Ok(h.recovery_key())
                }),
                |w, res, cx| match res {
                    Ok(rec_key) => {
                        w.sync_ui.generated_recovery_key = rec_key;
                        w.sync_ui.show_rotate_form = false;
                        w.sync_ui.rotate_new_pass.update(cx, |f, cx| f.set_text("", cx));
                        w.sync_ui.notice = Some((false, tr("加密口令已更新", "E2EE passphrase updated").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("修改失败", "Update failed")))),
                },
            );
        }

        fn sync_unlink(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| {
                    Box::pin(async move {
                        h.unlink_vault().await?;
                        h.list_vaults().await
                    })
                },
                |w, res, _| match res {
                    Ok(vaults) => {
                        w.sync_ui.vaults = vaults;
                        w.sync_ui.devices.clear();
                        w.sync_ui.confirm_unlink = false;
                        w.sync_ui.generated_recovery_key = None;
                        w.sync_ui.notice = Some((false, tr("已断开此文档库的云同步关联（本地文件保持不变）", "Unlinked vault. Local files remain intact.").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("断开失败", "Unlink failed")))),
                },
            );
        }

        fn sync_logout(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| Box::pin(async move { h.logout().await }),
                |w, res, _| match res {
                    Ok(()) => {
                        w.sync_ui.vaults.clear();
                        w.sync_ui.devices.clear();
                        w.sync_ui.storage_usage = None;
                        w.sync_ui.confirm_logout = false;
                        w.sync_ui.unlock_target = None;
                        w.sync_ui.generated_recovery_key = None;
                        w.sync_ui.notice = Some((false, tr("已退出登录", "Signed out").into()));
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("退出失败", "Sign out failed")))),
                },
            );
        }

        fn sync_revoke_device(&mut self, device_id: String, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                move |h| {
                    Box::pin(async move {
                        h.revoke_device(&device_id).await?;
                        h.list_devices().await
                    })
                },
                |w, res, cx| match res {
                    Ok(devices) => {
                        w.sync_ui.devices = devices;
                        w.show_toast(tr("已移除设备授权", "Device revoked"), cx);
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("移除失败", "Revoke failed")))),
                },
            );
        }

        fn sync_now_clicked(&mut self, cx: &mut Context<Self>) {
            run_sync_op(
                self,
                cx,
                |h| {
                    Box::pin(async move {
                        h.sync_now().await?;
                        Ok(h.storage_usage().await.ok())
                    })
                },
                |w, res, cx| match res {
                    Ok(storage) => {
                        if storage.is_some() {
                            w.sync_ui.storage_usage = storage;
                        }
                        w.show_toast(tr("同步完成", "Sync completed"), cx);
                    }
                    Err(e) => w.sync_ui.notice = Some((true, format!("{}: {e}", tr("同步失败", "Sync failed")))),
                },
            );
        }

        pub(crate) fn sync_page(&self, cx: &mut Context<Self>) -> AnyElement {
            let handle = crate::sync::handle(cx);
            let creds = handle.as_ref().and_then(|h| h.credentials());
            let status = crate::sync::status(cx);
            let ui = &self.sync_ui;

            let notice_el = ui.notice.as_ref().map(|(is_err, msg)| {
                let bg = if *is_err { theme::ca(theme::craft::R50, 0.7) } else { theme::ca(theme::craft::B50, 0.7) };
                let fg = if *is_err { theme::c(theme::craft::R600) } else { theme::c(theme::craft::B700) };
                div()
                    .w_full()
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(bg)
                    .border_1()
                    .border_color(if *is_err { theme::ca(theme::craft::R400, 0.35) } else { theme::ca(theme::craft::B500, 0.35) })
                    .text_size(px(theme::ui_font_small()))
                    .text_color(fg)
                    .child(msg.clone())
            });

            let rec_banner = ui.generated_recovery_key.as_ref().map(|rk| {
                let rk_copy = rk.clone();
                div()
                    .w_full()
                    .p(px(14.))
                    .rounded(px(10.))
                    .bg(theme::ca(theme::craft::O50, 0.75))
                    .border_1()
                    .border_color(theme::ca(theme::craft::O400, 0.4))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_size(px(theme::ui_font())).font_weight(FontWeight::SEMIBOLD).text_color(Theme::text()).child(tr("请妥善保存恢复密钥", "Save your recovery key")))
                    .child(div().text_size(px(theme::ui_font_small())).text_color(Theme::text_caption()).child(tr("忘记端到端加密口令时，只有恢复密钥可以解密您的云端笔记；服务器不保存任何密钥。", "If you forget your passphrase, this recovery key is the only way to decrypt your cloud notes. The server cannot reset it.")))
                    .child(div().px(px(10.)).py(px(8.)).rounded(px(6.)).bg(Theme::bg_body()).border_1().border_color(Theme::line_border()).font_family(theme::MONO_FONT).text_size(px(12.)).text_color(Theme::text()).child(rk.clone()))
                    .child(
                        div().flex().gap(px(8.))
                            .child(button("sync-copy-rk", tr("复制恢复密钥", "Copy recovery key")).on_click(cx.listener(move |w, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(rk_copy.clone()));
                                w.show_toast(tr("已复制到剪贴板", "Copied to clipboard"), cx);
                            })))
                            .child(link_button("sync-dismiss-rk", tr("我已保存", "I have saved it")).on_click(cx.listener(|w, _, _, cx| {
                                w.sync_ui.generated_recovery_key = None;
                                cx.notify();
                            }))),
                    )
            });

            let mut col = div().w_full().flex().flex_col().gap(px(24.)).children(notice_el).children(rec_banner);

            let is_logged_out = creds.is_none() || matches!(status, SyncStatus::LoggedOut | SyncStatus::AuthExpired);
            if is_logged_out {
                let auth_expired = matches!(status, SyncStatus::AuthExpired);
                let mode_idx = usize::from(ui.register_mode);
                let mut auth_rows = vec![
                    row(
                        tr("登录方式", "Account mode"),
                        None,
                        segmented(
                            "sync-auth-mode",
                            &[tr("登录", "Sign in"), tr("注册新账号", "Create account")],
                            mode_idx,
                            |w, ix, cx| {
                                w.sync_ui.register_mode = ix == 1;
                                cx.notify();
                            },
                            cx,
                        ),
                    ).into_any_element(),
                    row(
                        tr("邮箱", "Email"),
                        None,
                        input_box(&ui.email, 260.),
                    ).into_any_element(),
                    row(
                        tr("密码", "Password"),
                        None,
                        input_box(&ui.password, 260.),
                    ).into_any_element(),
                ];
                if ui.register_mode {
                    auth_rows.push(row(tr("显示名称", "Name"), None, input_box(&ui.display_name, 260.)).into_any_element());
                    auth_rows.push(row(tr("邀请码", "Invite code"), desc(tr("如服务端配置了注册邀请码则填写", "Required if server restricts registration")), input_box(&ui.invite_code, 260.)).into_any_element());
                }
                auth_rows.push(row(tr("此设备名称", "Device name"), desc(tr("显示在多端设备列表和协同提示中", "Shown in your devices list")), input_box(&ui.device_name, 260.)).into_any_element());
                auth_rows.push(
                    div().w_full().px(px(16.)).py(px(12.)).flex().items_center().justify_between()
                        .child(div().text_size(px(theme::ui_font_small())).text_color(if auth_expired { Theme::danger() } else { Theme::text_caption() })
                            .child(if auth_expired { tr("登录凭据已失效，请重新登录以恢复同步。", "Session expired. Please sign in again.") } else { tr("所有内容先写本地磁盘，再通过加密通道实时同步。", "Local-first: notes stay on disk and sync in real time.") }))
                        .child(primary_btn("sync-auth-submit", if ui.register_mode { tr("注册并继续", "Register") } else { tr("登录", "Sign in") }, ui.busy)
                            .on_click(cx.listener(|w, _, _, cx| if !w.sync_ui.busy { w.sync_submit_auth(cx); })))
                        .into_any_element(),
                );

                col = col
                    .child(group(
                        Some(tr("同步服务器", "Sync server")),
                        vec![row(
                            tr("服务器地址", "Server URL"),
                            desc(tr("远程服务仅支持 HTTPS；本机调试允许 http://127.0.0.1 或 localhost", "HTTPS required for remote servers (http:// allowed on localhost/127.0.0.1)")),
                            div().flex().items_center().gap(px(8.))
                                .child(input_box(&ui.server_url, 220.))
                                .child(button("sync-test-srv", tr("测试连接", "Test")).on_click(cx.listener(|w, _, _, cx| w.sync_test_server(cx)))),
                        ).into_any_element()],
                    ))
                    .child(group(Some(tr("账号认证", "Account")), auth_rows));

                return col.into_any_element();
            }

            let creds = creds.unwrap();
            if creds.vault_id.is_none() {
                // State 2: Authenticated & Unlinked
                let mut existing_rows: Vec<AnyElement> = ui.vaults.iter().map(|v| {
                    let v_clone = v.clone();
                    let title = if v.e2ee.is_some() {
                        format!("{}  (E2EE)", v.name)
                    } else {
                        v.name.clone()
                    };
                    let sub: SharedString = format!("seq {} · {}", v.head_seq, v.id).into();
                    row(
                        title,
                        Some(sub),
                        div().flex().items_center().gap(px(8.))
                            .when(v.e2ee.is_some(), |d| d.child(icon("lock", 14., Theme::icon())))
                            .child(button(SharedString::from(format!("link-vault-{}", v.id)), tr("连接到此文档库", "Connect"))
                                .on_click(cx.listener(move |w, _, _, cx| {
                                    if v_clone.e2ee.is_some() {
                                        w.sync_ui.unlock_target = Some(v_clone.clone());
                                        w.sync_ui.notice = None;
                                        cx.notify();
                                    } else {
                                        w.sync_connect_vault(v_clone.clone(), None, cx);
                                    }
                                }))),
                    ).into_any_element()
                }).collect();

                if existing_rows.is_empty() {
                    existing_rows.push(
                        row(
                            tr("暂无云端文档库", "No vaults yet"),
                            desc(tr("在下方创建一个新文档库，或点击右侧刷新列表", "Create one below or refresh")),
                            button("sync-refresh-vaults", tr("刷新", "Refresh")).on_click(cx.listener(|w, _, _, cx| w.fetch_sync_vaults(cx))),
                        ).into_any_element(),
                    );
                }

                col = col
                    .child(group(
                        Some(tr("已登录账号", "Signed in account")),
                        vec![row(
                            format!("{} ({})", creds.email, creds.server_url),
                            desc(tr("选择一个云端文档库，或新建并上传当前本地文档库", "Link an existing cloud vault or create a new one")),
                            div().flex().gap(px(8.))
                                .child(button("sync-reload-vaults", tr("刷新列表", "Refresh")).on_click(cx.listener(|w, _, _, cx| w.fetch_sync_vaults(cx))))
                                .child(button("sync-logout-unlinked", tr("退出登录", "Sign out")).on_click(cx.listener(|w, _, _, cx| w.sync_logout(cx)))),
                        ).into_any_element()],
                    ))
                    .child(group(Some(tr("已有云端文档库", "Existing vaults")), existing_rows));

                if let Some(target) = ui.unlock_target.clone() {
                    let target_for_btn = target.clone();
                    col = col.child(group(
                        Some(tr("解锁端到端加密文档库", "Unlock encrypted vault")),
                        vec![
                            row(
                                format!("{} · {}", target.name, tr("验证方式", "Method")),
                                None,
                                segmented(
                                    "sync-unlock-kind",
                                    &[tr("加密口令", "Passphrase"), tr("恢复密钥", "Recovery key")],
                                    usize::from(ui.unlock_with_recovery),
                                    |w, ix, cx| { w.sync_ui.unlock_with_recovery = ix == 1; cx.notify(); },
                                    cx,
                                ),
                            ).into_any_element(),
                            div().w_full().px(px(16.)).py(px(12.)).flex().flex_col().gap(px(10.))
                                .child(full_input_box(&ui.unlock_secret))
                                .child(
                                    div().flex().justify_end().gap(px(8.))
                                        .child(link_button("sync-unlock-cancel", tr("取消", "Cancel")).on_click(cx.listener(|w, _, _, cx| {
                                            w.sync_ui.unlock_target = None;
                                            cx.notify();
                                        })))
                                        .child(primary_btn("sync-unlock-confirm", tr("解锁并开始同步", "Unlock & Sync"), ui.busy)
                                            .on_click(cx.listener(move |w, _, _, cx| {
                                                let raw_secret = w.sync_ui.unlock_secret.read(cx).text.clone();
                                                let secret = if w.sync_ui.unlock_with_recovery {
                                                    raw_secret.trim().to_string()
                                                } else {
                                                    raw_secret
                                                };
                                                if secret.is_empty() {
                                                    w.sync_ui.notice = Some((true, tr("请输入口令或恢复密钥", "Enter passphrase or recovery key").into()));
                                                    cx.notify();
                                                    return;
                                                }
                                                let unlock = if w.sync_ui.unlock_with_recovery {
                                                    VaultUnlock::RecoveryKey(secret)
                                                } else {
                                                    VaultUnlock::Passphrase(secret)
                                                };
                                                w.sync_connect_vault(target_for_btn.clone(), Some(unlock), cx);
                                            }))),
                                ).into_any_element(),
                        ],
                    ));
                }

                let mut create_rows = vec![
                    row(tr("新文档库名称", "Vault name"), None, input_box(&ui.new_vault_name, 260.)).into_any_element(),
                    row(
                        tr("端到端加密 (E2EE)", "End-to-end encryption"),
                        desc(tr("正文、标题、目录名与附件名/内容均在本机使用 XChaCha20-Poly1305 加密后上传；服务端仍可见账号、文档库与设备信息、UUID/不透明附件 ID、密文大小、时间戳和同步序号", "Notes, titles, folder names, and attachment names/bytes are encrypted before upload; account, vault/device info, IDs, sizes, timestamps, and sequence numbers remain visible to the server")),
                        switch("sync-new-e2ee", ui.new_vault_e2ee).on_click(cx.listener(|w, _, _, cx| {
                            w.sync_ui.new_vault_e2ee = !w.sync_ui.new_vault_e2ee;
                            cx.notify();
                        })),
                    ).into_any_element(),
                ];
                if ui.new_vault_e2ee {
                    create_rows.push(row(tr("加密口令", "Passphrase"), None, input_box(&ui.e2ee_pass, 260.)).into_any_element());
                    create_rows.push(row(tr("确认口令", "Confirm"), None, input_box(&ui.e2ee_confirm, 260.)).into_any_element());
                }
                create_rows.push(
                    div().w_full().px(px(16.)).py(px(12.)).flex().justify_end()
                        .child(primary_btn("sync-create-vault", tr("创建云端文档库并上传当前库", "Create & Upload Current Library"), ui.busy)
                            .on_click(cx.listener(|w, _, _, cx| if !w.sync_ui.busy { w.sync_create_vault(cx); })))
                        .into_any_element(),
                );

                return col.child(group(Some(tr("创建新云端文档库", "Create new vault")), create_rows)).into_any_element();
            }

            // State 3: Linked & Running
            let h = handle.unwrap();
            let vault_name = creds.vault_name.clone().unwrap_or_else(|| creds.vault_id.clone().unwrap_or_default());
            let e2ee_on = h.is_e2ee_enabled();
            let paused = h.paused();
            let scope: SyncScope = h.scope();
            let interval_secs = h.sync_interval_secs();
            let interval_idx = SYNC_INTERVAL_PRESETS
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.abs_diff(interval_secs))
                .map_or(2, |(i, _)| i);

            let status_desc: SharedString = match &status {
                SyncStatus::Synced { last_sync_ms } => {
                    let t = hhmm(*last_sync_ms / 1000);
                    let prefix = if interval_secs == 0 {
                        tr("实时同步中 · 上次同步", "Real-time sync active · Last synced")
                    } else {
                        tr("云同步已开启 · 上次同步", "Cloud sync active · Last synced")
                    };
                    format!("{prefix} ({t})").into()
                }
                SyncStatus::Pending { pending } => {
                    crate::localized_format!("{} 项待同步 · 停止输入后自动上传", "{} pending · Will upload when idle", pending).into()
                }
                SyncStatus::Connecting => tr("正在连接实时推送通道…", "Connecting to WebSocket…").into(),
                SyncStatus::Syncing { pending } => {
                    if *pending > 0 { format!("{} ({pending})", tr("正在上传待同步项", "Syncing pending items")).into() }
                    else { tr("正在同步…", "Syncing…").into() }
                }
                SyncStatus::Offline { pending } => format!("{} ({pending})", tr("当前离线 · 本地改动将在恢复联网后自动续传", "Offline · Queued items")).into(),
                SyncStatus::Paused => tr("同步已暂停 · 恢复后将自动合并离线改动", "Sync paused").into(),
                SyncStatus::Error(err) => format!("{}: {err}", tr("错误", "Error")).into(),
                _ => tr("已就绪", "Ready").into(),
            };

            let status_title = if e2ee_on {
                format!("{vault_name}  ·  {}", tr("已启用 E2EE", "E2EE Enabled"))
            } else {
                vault_name
            };

            let overview_rows = vec![
                row(
                    status_title,
                    Some(status_desc),
                    div().flex().items_center().gap(px(8.))
                        .when(e2ee_on, |d| d.child(icon("lock", 14., Theme::icon())))
                        .child(button("sync-trigger-now", tr("立即同步", "Sync now")).on_click(cx.listener(|w, _, _, cx| w.sync_now_clicked(cx))))
                        .child(button("sync-toggle-pause", if paused { tr("恢复同步", "Resume") } else { tr("暂停", "Pause") }).on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(h) = crate::sync::handle(cx) {
                                h.set_paused(!paused);
                            }
                            w.sync_ui.notice = None;
                            cx.notify();
                        }))),
                ).into_any_element(),
                row(
                    creds.email.clone(),
                    Some(format!("{} · {}", creds.server_url, creds.device_name).into()),
                    div().flex().items_center().gap(px(8.))
                        .child(button("sync-refresh-devs", tr("刷新设备", "Refresh")).on_click(cx.listener(|w, _, _, cx| w.fetch_sync_devices(cx)))),
                ).into_any_element(),
                {
                    let (storage_desc, storage_right): (Option<SharedString>, AnyElement) = match &ui.storage_usage {
                        Some(u) => {
                            let detail: SharedString = format!(
                                "{} {} · {} {} · {} {}",
                                tr("正文", "Notes"),
                                format_bytes(u.entity_bytes),
                                tr("附件", "Assets"),
                                format_bytes(u.blob_bytes),
                                tr("单文件上限", "Max file"),
                                format_bytes(u.max_blob_bytes),
                            ).into();
                            let summary = match u.quota_bytes {
                                Some(q) => format!("{} / {}", format_bytes(u.used_bytes), format_bytes(q)),
                                None => format!("{} ({})", format_bytes(u.used_bytes), tr("不限空间", "Unlimited")),
                            };
                            let pct = u.quota_bytes.filter(|&q| q > 0).map(|q| (u.used_bytes as f32 / q as f32).clamp(0., 1.));
                            let right = div().flex().items_center().gap(px(10.))
                                .child(
                                    div().flex().flex_col().items_end().gap(px(4.))
                                        .child(div().text_size(px(theme::ui_font_small())).text_color(Theme::text()).child(summary))
                                        .when_some(pct, |d, p| {
                                            let bar_color = if p >= 0.9 { Theme::danger() } else { Theme::primary() };
                                            d.child(
                                                div().w(px(110.)).h(px(4.)).rounded_full().bg(Theme::fill_hover_strong()).overflow_hidden()
                                                    .child(div().h_full().w(px(110. * p)).rounded_full().bg(bar_color)),
                                            )
                                        }),
                                )
                                .child(button("sync-refresh-storage", tr("刷新", "Refresh")).on_click(cx.listener(|w, _, _, cx| w.fetch_sync_storage(cx))))
                                .into_any_element();
                            (Some(detail), right)
                        }
                        None => (
                            desc(tr("查询当前账号在服务端的已用空间与配额限制", "Check cloud storage usage and server limits")),
                            button("sync-query-storage", tr("查询空间", "Check")).on_click(cx.listener(|w, _, _, cx| w.fetch_sync_storage(cx))).into_any_element(),
                        ),
                    };
                    row(tr("云端空间", "Cloud storage"), storage_desc, storage_right).into_any_element()
                },
                row(
                    tr("同步间隔", "Sync interval"),
                    desc(tr("停止输入后等待指定时间再上传改动，可减轻服务器压力", "Wait after typing stops before uploading changes to reduce server load")),
                    segmented(
                        "sync-interval",
                        &[tr("实时", "Instant"), tr("10 秒", "10s"), tr("30 秒", "30s"), tr("1 分钟", "1m"), tr("5 分钟", "5m")],
                        interval_idx,
                        |w, ix, cx| {
                            if let Some(&secs) = SYNC_INTERVAL_PRESETS.get(ix)
                                && let Some(h) = crate::sync::handle(cx)
                            {
                                h.set_sync_interval_secs(secs);
                            }
                            w.sync_ui.notice = None;
                            cx.notify();
                        },
                        cx,
                    ),
                ).into_any_element(),
            ];
            col = col.child(group(Some(tr("云同步状态", "Sync status")), overview_rows));

            // E2EE Management card
            if e2ee_on {
                let rk_opt = h.recovery_key();
                let mut e2ee_rows = vec![
                    row(
                        tr("端到端加密", "End-to-end encryption"),
                        desc(tr("零知识模式：服务端仅存储密文信封与加密块", "Zero-knowledge ciphertext storage")),
                        div().flex().items_center().gap(px(8.))
                            .when(rk_opt.is_some(), |d| d.child(button("sync-toggle-rk", if ui.show_recovery_key { tr("隐藏恢复密钥", "Hide key") } else { tr("查看恢复密钥", "Show recovery key") }).on_click(cx.listener(|w, _, _, cx| {
                                w.sync_ui.show_recovery_key = !w.sync_ui.show_recovery_key;
                                cx.notify();
                            }))))
                            .child(button("sync-toggle-rotate", tr("修改加密口令", "Change passphrase")).on_click(cx.listener(|w, _, _, cx| {
                                w.sync_ui.show_rotate_form = !w.sync_ui.show_rotate_form;
                                cx.notify();
                            }))),
                    ).into_any_element(),
                ];
                if ui.show_recovery_key
                    && let Some(rk) = rk_opt
                {
                    let rk_copy = rk.clone();
                    e2ee_rows.push(
                        div().w_full().px(px(16.)).py(px(10.)).flex().items_center().justify_between().gap(px(12.))
                            .child(div().flex_1().min_w_0().font_family(theme::MONO_FONT).text_size(px(12.)).text_color(Theme::text()).child(rk))
                            .child(link_button("sync-copy-existing-rk", tr("复制", "Copy")).on_click(cx.listener(move |w, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(rk_copy.clone()));
                                w.show_toast(tr("已复制到剪贴板", "Copied to clipboard"), cx);
                            })))
                            .into_any_element(),
                    );
                }
                if ui.show_rotate_form {
                    e2ee_rows.push(
                        div().w_full().px(px(16.)).py(px(12.)).flex().flex_col().gap(px(8.))
                            .child(full_input_box(&ui.rotate_new_pass))
                            .child(div().flex().justify_end().gap(px(8.))
                                .child(link_button("sync-cancel-rotate", tr("取消", "Cancel")).on_click(cx.listener(|w, _, _, cx| {
                                    w.sync_ui.show_rotate_form = false;
                                    cx.notify();
                                })))
                                .child(primary_btn("sync-save-rotate", tr("更新加密口令", "Update passphrase"), ui.busy)
                                    .on_click(cx.listener(|w, _, _, cx| if !w.sync_ui.busy { w.sync_rotate_e2ee(cx); }))))
                            .into_any_element(),
                    );
                }
                col = col.child(group(Some(tr("端到端安全", "Security")), e2ee_rows));
            }

            // Sync Scope toggles
            col = col.child(group(
                Some(tr("同步范围", "Sync scope")),
                vec![
                    row(
                        tr("文档、画板、文件夹与附件", "Documents, boards, folders & assets"),
                        desc(tr("核心内容始终保持多端实时同步", "Always enabled")),
                        div().text_size(px(theme::ui_font_small())).text_color(Theme::text_caption()).child(tr("始终同步", "Always on")),
                    ).into_any_element(),
                    row(
                        tr("同步回收站", "Sync Trash"),
                        desc(tr("跨设备同步已删除但尚未彻底清理的文档", "Keep deleted items consistent across devices")),
                        switch("sync-scope-trash", scope.trash).on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(h) = crate::sync::handle(cx) {
                                let mut s = h.scope();
                                s.trash = !s.trash;
                                h.set_scope(s);
                            }
                            w.sync_ui.notice = None;
                            cx.notify();
                        })),
                    ).into_any_element(),
                    row(
                        tr("同步版本历史", "Sync Version History"),
                        desc(tr("在多台设备间同步每次保存和手动标记的历史快照", "Sync snapshots and milestone labels across devices")),
                        switch("sync-scope-history", scope.history).on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(h) = crate::sync::handle(cx) {
                                let mut s = h.scope();
                                s.history = !s.history;
                                h.set_scope(s);
                            }
                            w.sync_ui.notice = None;
                            cx.notify();
                        })),
                    ).into_any_element(),
                    row(
                        tr("同步偏好设置、快捷键与主题 CSS", "Sync Preferences, Shortcuts & Themes"),
                        desc(tr("本机窗口大小/位置/滚动进度等硬件相关项保留在本地", "Window geometry and scroll state stay local")),
                        switch("sync-scope-settings", scope.settings).on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(h) = crate::sync::handle(cx) {
                                let mut s = h.scope();
                                s.settings = !s.settings;
                                h.set_scope(s);
                            }
                            w.sync_ui.notice = None;
                            cx.notify();
                        })),
                    ).into_any_element(),
                ],
            ));

            // Connected devices
            let online = crate::sync::online_devices(cx);
            let mut dev_rows: Vec<AnyElement> = ui.devices.iter().map(|d| {
                let is_self = d.id == creds.device_id;
                let is_online = online.iter().any(|o| o.device_id == d.id) || is_self;
                let did = d.id.clone();
                let title = if is_self {
                    format!("{} ({})", d.name, tr("本机", "This device"))
                } else {
                    d.name.clone()
                };
                let subtitle: SharedString = format!("{} · {}", d.platform, hhmm(d.last_seen_at)).into();
                let right = div().flex().items_center().gap(px(8.))
                    .child(div().size(px(7.)).rounded_full().bg(if is_online { theme::c(theme::craft::old::G500) } else { Theme::text_disabled() }))
                    .when(!is_self, |x| x.child(link_button(SharedString::from(format!("revoke-dev-{}", d.id)), tr("移除", "Revoke")).on_click(cx.listener(move |w, _, _, cx| w.sync_revoke_device(did.clone(), cx)))));
                row(title, Some(subtitle), right).into_any_element()
            }).collect();
            if dev_rows.is_empty() {
                dev_rows.push(
                    row(tr("设备列表", "Devices"), desc(tr("点击上方刷新查看所有已授权设备", "Refresh to load authorized devices")), div()).into_any_element(),
                );
            }
            col = col.child(group(Some(tr("已连接设备", "Connected devices")), dev_rows));

            // Unlink & Sign out
            let unlink_ctrl = if ui.confirm_unlink {
                div().flex().items_center().gap(px(6.))
                    .child(link_button("sync-unlink-cancel", tr("取消", "Cancel")).on_click(cx.listener(|w, _, _, cx| { w.sync_ui.confirm_unlink = false; cx.notify(); })))
                    .child(button("sync-unlink-go", tr("确认断开", "Confirm unlink")).text_color(Theme::danger()).on_click(cx.listener(|w, _, _, cx| w.sync_unlink(cx))))
                    .into_any_element()
            } else {
                button("sync-unlink-btn", tr("断开关联", "Unlink")).on_click(cx.listener(|w, _, _, cx| { w.sync_ui.confirm_unlink = true; cx.notify(); })).into_any_element()
            };
            let logout_ctrl = if ui.confirm_logout {
                div().flex().items_center().gap(px(6.))
                    .child(link_button("sync-logout-cancel", tr("取消", "Cancel")).on_click(cx.listener(|w, _, _, cx| { w.sync_ui.confirm_logout = false; cx.notify(); })))
                    .child(button("sync-logout-go", tr("确认退出", "Confirm sign out")).text_color(Theme::danger()).on_click(cx.listener(|w, _, _, cx| w.sync_logout(cx))))
                    .into_any_element()
            } else {
                button("sync-logout-btn", tr("退出登录", "Sign out")).text_color(Theme::danger()).on_click(cx.listener(|w, _, _, cx| { w.sync_ui.confirm_logout = true; cx.notify(); })).into_any_element()
            };
            col = col.child(group(
                Some(tr("账号与关联", "Connection")),
                vec![
                    row(tr("断开云端文档库关联", "Unlink cloud vault"), desc(tr("仅移除云端同步关联，本地文档和文件夹完整保留", "Stops syncing without deleting local notes")), unlink_ctrl).into_any_element(),
                    row(tr("退出同步账号", "Sign out"), desc(tr("清除本机登录令牌并停止后台同步", "Clears local credentials and stops background sync")), logout_ctrl).into_any_element(),
                ],
            ));

            let _ = ui::list_line();
            col.into_any_element()
        }
    }
}

pub use imp::SyncUiState;
