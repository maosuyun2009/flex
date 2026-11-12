//! Workspace placement and shutdown guard for the independent file browser.
use super::*;
use flex_engine::files::FileTarget;

impl Workspace {
    pub(super) fn sync_file_target(&mut self, cx: &mut Context<Self>) {
        if !self.settings.files_open {
            return;
        }
        let target = self.active().filter(|b| b.is_terminal()).and_then(|block| {
            let server = self.state.server(self.server_id.as_deref()?)?;
            Some((
                FileTarget {
                    server_id: server.id.clone(),
                    address: server.address.clone(),
                    session_id: self.session_id.clone()?,
                    block_id: block.id.clone(),
                },
                format!("{} · {}", server.name, block.display_name()),
            ))
        });
        let (target, label) = target
            .map(|(t, l)| (Some(t), l))
            .unwrap_or((None, "Select a terminal pane".into()));
        self.file_browser
            .update(cx, |files, cx| files.set_target(target, label, cx));
    }

    pub(super) fn files_have_focus(&self, window: &Window, cx: &App) -> bool {
        self.settings.files_open
            && !self.show_settings
            && self
                .file_browser
                .read(cx)
                .handle()
                .contains_focused(window, cx)
    }

    pub(super) fn toggle_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.files_open = !self.settings.files_open;
        self.settings.save(&self.engine);
        if !self.settings.files_open {
            self.pending_focus = self.active_block.clone();
            window.focus(&self.focus);
        }
        self.sync_file_target(cx);
        cx.notify();
    }

    pub(super) fn file_panel(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let width = crate::settings::file_panel_width(
            self.settings.files_width,
            f32::from(window.viewport_size().width),
            if self.settings.sidebar {
                f32::from(theme::SIDEBAR_W)
            } else {
                0.0
            },
        );
        let mut panel = div()
            .relative()
            .flex_none()
            .w(px(width))
            .min_w(px(0.0))
            .h_full()
            .border_l_1()
            .border_color(theme::color(0x44485a))
            .bg(theme::color(0x1e2030))
            .child(self.file_browser.clone())
            .child(
                div()
                    .id("file-panel-resize")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .w(px(5.0))
                    .h_full()
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.file_resize = Some((event.position.x, width));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );
        if let Some((start, width)) = self.file_resize {
            let entity = cx.entity().downgrade();
            panel = panel.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let moving = entity.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                let _ = moving.update(cx, |me, cx| {
                                    me.settings.files_width = (width
                                        + f32::from(start - event.position.x))
                                    .clamp(180.0, 520.0);
                                    cx.notify();
                                });
                            }
                        });
                        let released = entity.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if event.button == MouseButton::Left
                                && phase == gpui::DispatchPhase::Bubble
                            {
                                let _ = released.update(cx, |me, cx| {
                                    me.file_resize = None;
                                    me.settings.save(&me.engine);
                                    cx.notify();
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        panel.into_any_element()
    }

    pub(super) fn request_app_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_browser.read(cx).has_pending() {
            self.confirm_file_exit = true;
            window.focus(&self.focus);
            cx.notify();
        } else {
            window.remove_window();
        }
    }

    pub(super) fn file_exit_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.confirm_file_exit && !self.exit_after_files {
            return div().into_any_element();
        }
        div().absolute().size_full().top(px(0.0)).left(px(0.0)).occlude()
            .bg(hsla(0.0,0.0,0.0,0.55)).flex().items_center().justify_center()
            .on_any_mouse_down(|_,_,cx| cx.stop_propagation())
            .on_scroll_wheel(|_,_,cx| cx.stop_propagation())
            .on_mouse_move(|_,_,cx| cx.stop_propagation())
            .map(|mut el| { el.interactivity().on_any_mouse_up(|_,_,cx| cx.stop_propagation()); el })
            .child(div().w(px(440.0)).p_4().rounded_lg().bg(theme::color(theme::SURFACE))
                .border_1().border_color(theme::border()).flex().flex_col().gap_3()
                .child(if self.exit_after_files { "Stopping transfers…" } else { "File transfers are still pending" })
                .child(div().text_xs().text_color(theme::muted())
                    .child("Stopping cannot undo uploads or terminal path pastes already completed. Incomplete downloads will not replace existing files."))
                .when(!self.exit_after_files, |el| el.child(div().flex().gap_3()
                    .child(div().id("files-exit-wait").px_3().py_2().rounded_md().bg(theme::hover())
                        .child("Keep working").on_click(cx.listener(|me,_,window,cx| {
                            me.confirm_file_exit=false; me.pending_focus=me.active_block.clone(); window.focus(&me.focus);cx.notify();
                        })))
                    .child(div().id("files-exit-stop").px_3().py_2().rounded_md().text_color(theme::danger())
                        .child("Stop transfers and exit").on_click(cx.listener(|me,_,_,cx| {
                            me.file_browser.update(cx, |files,cx| files.cancel_all(cx));
                            me.confirm_file_exit=false;me.exit_after_files=true;cx.notify();
                        }))))))
            .into_any_element()
    }
}
