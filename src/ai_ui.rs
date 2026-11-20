//! Settings › AI page. Hosts the built-in local MCP server settings and leaves
//! room for future AI capabilities as additional groups.

use gpui::*;
use crate::app::Workspace;

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    use crate::settings::{desc, group, row, tr};

    #[derive(Default)]
    pub struct AiUiState;

    impl AiUiState {
        pub fn new(_: &mut Window, _: &mut Context<Workspace>) -> Self { Self }
    }

    impl Workspace {
        pub(crate) fn ai_ui_enter(&mut self, _: &mut Context<Self>) {}

        pub(crate) fn ai_page(&self, _: &mut Context<Self>) -> AnyElement {
            div().w_full().flex().flex_col().gap(px(24.))
                .child(group(
                    Some(tr("MCP 本地服务", "Local MCP Server")),
                    vec![row(
                        tr("桌面端功能", "Desktop only"),
                        desc(tr("本地 MCP 服务仅在 Quiet.md 桌面版中可用。", "The local MCP server is available in the Quiet.md desktop app.")),
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
    use crate::local_api::{self, ServerStatus};
    use crate::settings::{button, desc, group, link_button, row, segmented, switch, tr};
    use crate::theme::{self, Theme};
    use crate::ui::icon;
    use gpui::prelude::FluentBuilder;

    pub struct AiUiState {
        port_field: Entity<TextField>,
        show_token: bool,
        notice: Option<(bool, String)>,
        _subs: Vec<Subscription>,
    }

    fn port_input_box(field: &Entity<TextField>) -> Div {
        div()
            .w(px(96.))
            .h(px(28.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(7.))
            .bg(Theme::bg_body())
            .border_1()
            .border_color(Theme::line_border())
            .child(field.clone())
    }

    fn mask_token(token: &str) -> String {
        if token.is_empty() {
            return "—".into();
        }
        if token.len() <= 12 {
            return "•".repeat(token.len());
        }
        format!("{}••••••••••••••••{}", &token[..6], &token[token.len() - 4..])
    }

    impl AiUiState {
        pub fn new(_: &mut Window, cx: &mut Context<Workspace>) -> Self {
            let cfg = local_api::config(cx);
            let port_field = cx.new(|cx| {
                let mut f = TextField::new(local_api::config::DEFAULT_PORT.to_string(), cx);
                f.placeholder_color = Theme::text_disabled();
                f.set_text(&cfg.port.to_string(), cx);
                f
            });
            let sub = cx.subscribe(&port_field, |w, _, ev: &TextFieldEvent, cx| {
                if matches!(ev, TextFieldEvent::Submit) {
                    w.ai_apply_port(cx);
                }
            });
            Self { port_field, show_token: false, notice: None, _subs: vec![sub] }
        }
    }

    impl Workspace {
        pub(crate) fn ai_ui_enter(&mut self, cx: &mut Context<Self>) {
            let cfg = local_api::config(cx);
            self.ai_ui.port_field.update(cx, |f, cx| f.set_text(&cfg.port.to_string(), cx));
            self.ai_ui.notice = None;
        }

        fn ai_apply_port(&mut self, cx: &mut Context<Self>) {
            let raw = self.ai_ui.port_field.read(cx).text.trim().to_string();
            let Ok(port) = raw.parse::<u16>() else {
                self.ai_ui.notice = Some((true, tr("请输入有效的端口号（1024–65535）", "Enter a valid port number (1024–65535)").into()));
                cx.notify();
                return;
            };
            if port < 1024 {
                self.ai_ui.notice = Some((true, tr("端口号需在 1024 到 65535 之间", "Port must be between 1024 and 65535").into()));
                cx.notify();
                return;
            }
            let mut cfg = local_api::config(cx);
            cfg.port = port;
            match local_api::apply_config(cfg, cx) {
                Ok(()) => self.ai_ui.notice = None,
                Err(e) => self.ai_ui.notice = Some((true, e.to_string())),
            }
            cx.notify();
        }

        fn ai_toggle_enabled(&mut self, cx: &mut Context<Self>) {
            let mut cfg = local_api::config(cx);
            cfg.enabled = !cfg.enabled;
            match local_api::apply_config(cfg, cx) {
                Ok(()) => self.ai_ui.notice = None,
                Err(e) => self.ai_ui.notice = Some((true, e.to_string())),
            }
            cx.notify();
        }

        fn ai_set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
            let mut cfg = local_api::config(cx);
            if cfg.read_only == read_only { return; }
            cfg.read_only = read_only;
            match local_api::apply_config(cfg, cx) {
                Ok(()) => self.ai_ui.notice = None,
                Err(e) => self.ai_ui.notice = Some((true, e.to_string())),
            }
            cx.notify();
        }

        fn ai_toggle_snapshot(&mut self, cx: &mut Context<Self>) {
            let mut cfg = local_api::config(cx);
            cfg.snapshot_before_edit = !cfg.snapshot_before_edit;
            match local_api::apply_config(cfg, cx) {
                Ok(()) => self.ai_ui.notice = None,
                Err(e) => self.ai_ui.notice = Some((true, e.to_string())),
            }
            cx.notify();
        }

        fn ai_regenerate_token(&mut self, cx: &mut Context<Self>) {
            match local_api::regenerate_token(cx) {
                Ok(_) => self.ai_ui.notice = Some((false, tr("已重新生成访问令牌，旧令牌立即失效", "Access token regenerated; the previous token is now invalid").into())),
                Err(e) => self.ai_ui.notice = Some((true, e.to_string())),
            }
            cx.notify();
        }

        fn ai_copy(&mut self, text: String, label: &'static str, cx: &mut Context<Self>) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.ai_ui.notice = Some((false, label.into()));
            cx.notify();
        }

        pub(crate) fn ai_page(&self, cx: &mut Context<Self>) -> AnyElement {
            let cfg = local_api::config(cx);
            let status = local_api::status(cx);
            let endpoint = cfg.endpoint();
            let current_port_input = self.ai_ui.port_field.read(cx).text.trim().to_string();
            let port_dirty = current_port_input != cfg.port.to_string();

            let (dot_color, status_text): (Hsla, SharedString) = match &status {
                ServerStatus::Running { port } => (
                    theme::c(theme::craft::old::G500),
                    format!("{} · http://127.0.0.1:{port}/mcp", tr("运行中", "Running")).into(),
                ),
                ServerStatus::Stopped => (
                    Theme::text_disabled(),
                    tr("未启动 · 仅监听本机 127.0.0.1，不暴露到局域网", "Stopped · Binds to 127.0.0.1 only, never exposed to the network").into(),
                ),
                ServerStatus::Error(err) => (
                    Theme::danger(),
                    format!("{}: {err}", tr("启动失败", "Failed to start")).into(),
                ),
            };

            let status_desc = div().flex().items_center().gap(px(6.))
                .child(div().size(px(7.)).rounded_full().flex_none().bg(dot_color))
                .child(div().text_size(px(theme::ui_font_small())).line_height(px(theme::ui_font_small() + 5.)).text_color(if matches!(status, ServerStatus::Error(_)) { Theme::danger() } else { Theme::text_caption() }).child(status_text));

            let enable_row = div().w_full().min_h(px(48.)).px(px(16.)).py(px(10.)).flex().items_center().justify_between().gap(px(24.))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(3.))
                    .child(div().text_size(px(theme::ui_font())).line_height(px(theme::ui_font() + 7.)).text_color(Theme::text()).child(tr("启用本地 MCP 服务", "Enable local MCP server")))
                    .child(status_desc))
                .child(switch("ai-mcp-enable", cfg.enabled).on_click(cx.listener(|w, _, _, cx| w.ai_toggle_enabled(cx))));

            let perm_row = row(
                tr("访问权限", "Access mode"),
                desc(tr("默认允许 AI Agent 读取并直接编辑文档，也可以随时切换为仅读取。", "Allows AI agents to read and edit notes by default, or switch to read-only anytime.")),
                segmented(
                    "ai-mcp-perm",
                    &[tr("读写", "Read & Write"), tr("仅读取", "Read-only")],
                    usize::from(cfg.read_only),
                    |w, ix, cx| w.ai_set_read_only(ix == 1, cx),
                    cx,
                ),
            );

            let port_ctrl = div().flex().items_center().gap(px(8.))
                .child(port_input_box(&self.ai_ui.port_field))
                .when(port_dirty, |d| d.child(
                    button("ai-mcp-port-apply", tr("应用", "Apply"))
                        .on_click(cx.listener(|w, _, _, cx| w.ai_apply_port(cx)))
                ));
            let port_row = row(
                tr("监听端口", "Port"),
                desc(tr("仅绑定本机回环地址 127.0.0.1（默认 27123）。", "Binds to loopback 127.0.0.1 only (default 27123).")),
                port_ctrl,
            );

            let snapshot_row = row(
                tr("AI 编辑前自动保留版本快照", "Save version snapshot before AI edits"),
                desc(tr("每次 AI 修改文档前先在版本历史中留存当前内容，随时可对比差异或一键恢复。", "Saves a snapshot to Version History before AI edits a note so you can diff or restore anytime.")),
                switch("ai-mcp-snapshot", cfg.snapshot_before_edit)
                    .on_click(cx.listener(|w, _, _, cx| w.ai_toggle_snapshot(cx))),
            );

            let token_display = if self.ai_ui.show_token {
                if cfg.token.is_empty() { "—".to_string() } else { cfg.token.clone() }
            } else {
                mask_token(&cfg.token)
            };
            let token_for_copy = cfg.token.clone();
            let token_row = div().w_full().px(px(16.)).py(px(12.)).flex().flex_col().gap(px(8.))
                .child(div().flex().items_center().justify_between().gap(px(12.))
                    .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.))
                        .child(div().text_size(px(theme::ui_font())).line_height(px(theme::ui_font() + 7.)).text_color(Theme::text()).child(tr("访问令牌（Bearer Token）", "Access token (Bearer)")))
                        .child(div().text_size(px(theme::ui_font_small())).line_height(px(theme::ui_font_small() + 5.)).text_color(Theme::text_caption()).child(tr("仅保存在本机应用数据目录，不会随文档库同步。", "Stored only in local app data; never synced with your library."))))
                    .child(div().flex_none().flex().items_center().gap(px(6.))
                        .child(div().id("ai-mcp-token-eye").size(px(28.)).rounded(px(7.)).flex().items_center().justify_center().cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
                            .child(icon(if self.ai_ui.show_token { "eye-open" } else { "eye" }, 15., Theme::icon()))
                            .on_click(cx.listener(|w, _, _, cx| { w.ai_ui.show_token = !w.ai_ui.show_token; cx.notify(); })))
                        .when(!token_for_copy.is_empty(), |d| {
                            let tok = token_for_copy.clone();
                            d.child(button("ai-mcp-token-copy", tr("复制", "Copy")).on_click(cx.listener(move |w, _, _, cx| {
                                w.ai_copy(tok.clone(), tr("已复制访问令牌", "Access token copied"), cx);
                            })))
                        })
                        .child(link_button("ai-mcp-token-regen", tr("重新生成", "Regenerate")).on_click(cx.listener(|w, _, _, cx| w.ai_regenerate_token(cx))))))
                .child(div().w_full().px(px(10.)).py(px(7.)).rounded(px(7.)).bg(Theme::bg_body()).border_1().border_color(Theme::line_border())
                    .font_family(theme::MONO_FONT).text_size(px(theme::ui_font_small())).text_color(Theme::text_code())
                    .child(token_display));

            let client_json_preview = if self.ai_ui.show_token {
                cfg.client_config_json()
            } else {
                cfg.client_config_json_masked()
            };
            let client_json_copy = cfg.client_config_json();
            let endpoint_copy = endpoint.clone();
            let connect_row = div().w_full().px(px(16.)).py(px(12.)).flex().flex_col().gap(px(10.))
                .child(div().flex().items_center().justify_between().gap(px(12.))
                    .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.))
                        .child(div().text_size(px(theme::ui_font())).line_height(px(theme::ui_font() + 7.)).text_color(Theme::text()).child(tr("客户端配置（Streamable HTTP）", "Client configuration (Streamable HTTP)")))
                        .child(div().text_size(px(theme::ui_font_small())).line_height(px(theme::ui_font_small() + 5.)).text_color(Theme::text_caption()).child(tr("粘贴到 Cursor、Claude Code 或其他支持 Streamable HTTP 的 MCP 客户端即可接入。", "Paste into Cursor, Claude Code, or any MCP client supporting Streamable HTTP."))))
                    .child(div().flex_none().flex().items_center().gap(px(8.))
                        .child(link_button("ai-mcp-copy-url", tr("复制地址", "Copy URL")).on_click(cx.listener(move |w, _, _, cx| {
                            w.ai_copy(endpoint_copy.clone(), tr("已复制 MCP 地址", "MCP URL copied"), cx);
                        })))
                        .child(button("ai-mcp-copy-json", tr("复制配置", "Copy config")).on_click(cx.listener(move |w, _, _, cx| {
                            w.ai_copy(client_json_copy.clone(), tr("已复制 MCP 配置 JSON", "MCP config JSON copied"), cx);
                        })))))
                .child(div().w_full().px(px(12.)).py(px(10.)).rounded(px(8.)).bg(Theme::bg_body()).border_1().border_color(Theme::line_border())
                    .font_family(theme::MONO_FONT).text_size(px(theme::ui_font_small())).line_height(px(theme::ui_font_small() + 6.)).text_color(Theme::text_code())
                    .child(client_json_preview));

            let mut rows: Vec<AnyElement> = vec![
                enable_row.into_any_element(),
                perm_row.into_any_element(),
                port_row.into_any_element(),
                snapshot_row.into_any_element(),
            ];
            if cfg.enabled || !cfg.token.is_empty() {
                rows.push(token_row.into_any_element());
                rows.push(connect_row.into_any_element());
            }

            let mut col = div().w_full().flex().flex_col().gap(px(20.));
            if let Some((is_err, ref msg)) = self.ai_ui.notice {
                col = col.child(
                    div().w_full().px(px(14.)).py(px(10.)).rounded(px(10.)).border_1()
                        .border_color(if is_err { Theme::danger() } else { Theme::line_border() })
                        .bg(crate::theme::raised(Theme::bg_body()))
                        .text_size(px(theme::ui_font_small()))
                        .text_color(if is_err { Theme::danger() } else { Theme::text_caption() })
                        .child(msg.clone()),
                );
            }
            col = col.child(group(Some(tr("MCP 本地服务", "Local MCP Server")), rows));
            col.into_any_element()
        }
    }
}

pub use imp::AiUiState;
