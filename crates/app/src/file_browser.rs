//! The Files sidebar. It owns navigation and transfers, but never executes a
//! remote command or retargets work when the selected pane changes.

#[path = "file_browser_model.rs"]
mod model;
#[path = "file_browser_style.rs"]
mod style;

use style::{color, icon, FileIcon, Hint};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_channel::{Receiver, Sender};
use flex_engine::{
    files::{FileTarget, TransferProgress},
    CoreError, Engine,
};
use gpui::prelude::*;
use gpui::{
    div, px, App, Context, ElementInputHandler, EntityInputHandler, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, Render,
    SharedString, UTF16Selection, Window,
};
use serde_json::json;

use crate::text_field::{self, TextField};
use crate::theme;

const LIST_LIMIT: u64 = 400;
const LAST_DIRECTORY: &str = "rexshell.file_browser.last_local_directory";

#[derive(Clone, Debug)]
pub enum FileBrowserEvent {
    Notice(String),
}

impl EventEmitter<FileBrowserEvent> for FileBrowser {}

type TransferJob = model::Job<FileTarget, TransferKind, TransferProgress>;

#[derive(Clone)]
enum TransferKind {
    Upload(Vec<PathBuf>),
    Download {
        remote: String,
        local: PathBuf,
        overwrite: bool,
    },
}

struct PendingUpload {
    target: FileTarget,
    label: String,
    paths: Vec<PathBuf>,
}

struct PendingDownloadOverwrite {
    target: FileTarget,
    label: String,
    remote: String,
    local: PathBuf,
}

enum Pending {
    Upload(PendingUpload),
    DownloadOverwrite(PendingDownloadOverwrite),
    Retry(usize),
}

enum TransferMessage {
    Progress {
        index: usize,
        progress: TransferProgress,
    },
    Finished {
        index: usize,
        result: Result<(), String>,
    },
}

/// A file browser bound to one immutable terminal pane at a time.
pub struct FileBrowser {
    engine: Arc<Engine>,
    target: Option<FileTarget>,
    target_label: String,
    paths: HashMap<String, String>,
    path: String,
    listing: Option<model::Listing>,
    list_error: Option<String>,
    loaded_prefix: Option<String>,
    filter: TextField,
    filter_options: model::FilterOptions,
    hidden: bool,
    generation: u64,
    focus: FocusHandle,
    filter_focus: FocusHandle,
    expanded_job: Option<usize>,
    pending: Option<Pending>,
    jobs: Vec<TransferJob>,
    progress_rx: Receiver<TransferMessage>,
    progress_tx: Sender<TransferMessage>,
    next_job: usize,
    dialog_open: bool,
    dialog_generation: u64,
    validation_error: Option<String>,
    capabilities: Option<Result<flex_engine::files::FileCapabilities, String>>,
    list_scroll: gpui::ScrollHandle,
    scroll_drag: Option<(f32, f32)>,
}

impl FileBrowser {
    pub fn new(engine: Arc<Engine>, cx: &mut Context<Self>) -> Self {
        let (progress_tx, progress_rx) = async_channel::unbounded();
        let mut this = Self {
            engine,
            target: None,
            target_label: String::new(),
            paths: HashMap::new(),
            path: String::new(),
            listing: None,
            list_error: None,
            loaded_prefix: None,
            filter: TextField::default(),
            filter_options: model::FilterOptions::default(),
            hidden: false,
            generation: 0,
            focus: cx.focus_handle(),
            filter_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            expanded_job: None,
            pending: None,
            jobs: Vec::new(),
            progress_rx,
            progress_tx,
            next_job: 0,
            dialog_open: false,
            dialog_generation: 0,
            validation_error: None,
            capabilities: None,
            list_scroll: gpui::ScrollHandle::new(),
            scroll_drag: None,
        };
        this.listen_for_progress(cx);
        this
    }

    pub fn handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn has_pending(&self) -> bool {
        self.jobs.iter().any(|job| {
            matches!(
                job.state,
                model::JobState::Queued | model::JobState::Running
            )
        }) || self.pending.is_some()
            || self.dialog_open
    }

    pub fn cancel_all(&mut self, cx: &mut Context<Self>) {
        for job in &mut self.jobs {
            job.cancel();
        }
        self.pending = None;
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        self.dialog_open = false;
        self.notice("已取消传输，正在清理当前任务。", cx);
        cx.notify();
    }

    /// Select a pane. Results from the previous pane are invalidated before the
    /// new request starts, so a slow server cannot paint into the new pane.
    pub fn set_target(
        &mut self,
        target: Option<FileTarget>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if self.target == target {
            self.target_label = label;
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.target = target;
        self.target_label = label;
        self.capabilities = None;
        self.listing = None;
        self.list_error = None;
        self.filter = TextField::default();
        self.path = self
            .target
            .as_ref()
            .and_then(|target| self.paths.get(&target_key(target)).cloned())
            .unwrap_or_default();
        self.refresh_capabilities(cx);
        self.load_directory(cx, None);
    }

    fn upload_allowed(&self) -> bool {
        matches!(&self.capabilities, Some(Ok(capabilities)) if capabilities.upload)
    }
    fn download_allowed(&self) -> bool {
        matches!(&self.capabilities, Some(Ok(capabilities)) if capabilities.download)
    }
    fn refresh_capabilities(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .file_capabilities(target.clone())
                .await
                .map_err(|e| e.to_string());
            this.update(cx, |me, cx| {
                if me.target.as_ref() == Some(&target) {
                    me.capabilities = Some(result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Follow the terminal's current directory again (empty path asks the
    /// server for cwd); it also forgets this pane's manual path.
    pub fn follow_terminal_directory(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        self.paths.remove(&target_key(target));
        self.path.clear();
        self.generation = self.generation.wrapping_add(1);
        self.load_directory(cx, None);
    }

    fn listen_for_progress(&mut self, cx: &mut Context<Self>) {
        let rx = self.progress_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                let keep_going = this.update(cx, |me, cx| match message {
                    TransferMessage::Progress { index, progress } => {
                        let notice =
                            me.jobs
                                .iter_mut()
                                .find(|job| job.id == index)
                                .and_then(|job| {
                                    if job.state != model::JobState::Running {
                                        return None;
                                    }
                                    let changed = job.progress.as_ref().is_none_or(|previous| {
                                        previous.file_name != progress.file_name
                                    });
                                    let notice = changed.then(|| {
                                        format!("正在传输 {} · {}", progress.file_name, job.label)
                                    });
                                    job.progress = Some(progress);
                                    notice
                                });
                        if let Some(notice) = notice {
                            me.notice(notice, cx);
                        }
                        cx.notify();
                    }
                    TransferMessage::Finished { index, result } => {
                        let message = me.jobs.iter_mut().find(|job| job.id == index).map(|job| {
                            let was_cancelled = job.cancel.load(Ordering::Relaxed);
                            match result {
                                Ok(()) => {
                                    job.state = model::JobState::Success;
                                    format!("已完成 {}", job.label)
                                }
                                Err(error) => {
                                    job.state = if was_cancelled {
                                        model::JobState::Cancelled
                                    } else {
                                        model::JobState::Failure
                                    };
                                    job.error = Some(error.clone());
                                    format!("{}: {}", job.label, error)
                                }
                            }
                        });
                        if let Some(message) = message {
                            me.notice(message, cx);
                        }
                        me.prune_history();
                        me.start_next(cx);
                        cx.notify();
                    }
                });
                if keep_going.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn load_directory(&mut self, cx: &mut Context<Self>, prefix: Option<String>) {
        let Some(target) = self.target.clone() else {
            cx.notify();
            return;
        };
        self.list_error = None;
        self.listing = None;
        self.list_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.scroll_drag = None;
        cx.notify();
        let stamp = model::RequestStamp {
            generation: self.generation,
            target: target_key(&target),
        };
        let path = self.path.clone();
        let requested_prefix = prefix.clone();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .call(
                    &target.address,
                    "com.superlogical.terminal.list_dir",
                    json!({
                        "session_id": target.session_id,
                        "block_id": target.block_id,
                        "args": {
                            "path": path,
                            "prefix": prefix.unwrap_or_default(),
                            "files": true,
                            "limit": LIST_LIMIT
                        }
                    }),
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| model::parse_listing(value).map_err(str::to_owned));
            this.update(cx, |me, cx| {
                let current = me.target.as_ref().map(target_key);
                let active = model::RequestStamp {
                    generation: me.generation,
                    target: current.unwrap_or_default(),
                };
                if !active.accepts(&stamp) {
                    return;
                }
                match result {
                    Ok(listing) => {
                        me.path = listing.path.clone();
                        me.loaded_prefix = requested_prefix;
                        me.listing = Some(listing);
                        me.list_error = None;
                    }
                    Err(error) => {
                        me.listing = None;
                        me.list_error = Some(format!(
                            "无法读取 {}：{error}",
                            if me.path.is_empty() {
                                "终端目录"
                            } else {
                                &me.path
                            }
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        self.filter = TextField::default();
        self.path = path;
        if let Some(target) = self.target.as_ref() {
            self.paths.insert(target_key(target), self.path.clone());
        }
        self.generation = self.generation.wrapping_add(1);
        self.load_directory(cx, None);
    }

    fn enter(&mut self, name: &str, cx: &mut Context<Self>) {
        self.navigate(model::join_path(&self.path, name), cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.refresh_capabilities(cx);
        self.load_directory(cx, None);
    }

    fn notice(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(FileBrowserEvent::Notice(message.into()));
    }

    fn can_stage(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pending.is_some() || self.dialog_open {
            self.validation_error = Some("请先确认或取消当前选择。".into());
            cx.notify();
            false
        } else {
            self.validation_error = None;
            true
        }
    }

    fn stage_upload(
        &mut self,
        target: FileTarget,
        label: String,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let invalid = paths
            .iter()
            .find(|path| match std::fs::symlink_metadata(path) {
                Ok(metadata) => !metadata.file_type().is_file(),
                Err(_) => true,
            });
        if let Some(path) = invalid {
            let message = format!(
                "上传已取消：'{}' 不是可读取的普通文件。不支持文件夹和符号链接，请仅选择文件。",
                path.display()
            );
            self.validation_error = Some(message.clone());
            self.notice(message, cx);
        } else {
            self.pending = Some(Pending::Upload(PendingUpload {
                target,
                label,
                paths,
            }));
        }
        cx.notify();
    }

    fn begin_upload_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.upload_allowed() {
            self.validation_error = Some("无法上传：请检查连接，上传需要 API 11+。".into());
            cx.notify();
            return;
        }
        if !self.can_stage(cx) {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.validation_error = Some("请先选择终端窗格。".into());
            cx.notify();
            return;
        };
        self.stage_upload(target, self.target_label.clone(), paths, cx);
    }

    fn choose_upload(&mut self, cx: &mut Context<Self>) {
        if !self.upload_allowed() {
            return;
        }
        if !self.can_stage(cx) {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.validation_error = Some("请先选择终端窗格。".into());
            cx.notify();
            return;
        };
        let label = self.target_label.clone();
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        let generation = self.dialog_generation;
        self.dialog_open = true;
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择要上传的文件".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |me, cx| {
                if me.dialog_generation != generation {
                    return;
                }
                me.dialog_open = false;
                match result {
                    Ok(Ok(Some(paths))) => me.stage_upload(target, label, paths, cx),
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        me.validation_error = Some(format!("无法打开文件选择器：{error}。请重试。"))
                    }
                    Err(error) => {
                        me.validation_error = Some(format!("文件选择器意外关闭：{error}。请重试。"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn choose_download(&mut self, entry: &model::Entry, cx: &mut Context<Self>) {
        if !self.download_allowed() {
            return;
        }
        if !self.can_stage(cx) {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let remote = model::join_path(&self.path, &entry.name);
        let initial = self
            .engine
            .shrew()
            .setting(LAST_DIRECTORY)
            .and_then(|value| value.as_str().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let suggested = safe_filename(&entry.name);
        let label = self.target_label.clone();
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        let generation = self.dialog_generation;
        self.dialog_open = true;
        let receiver = cx.prompt_for_new_path(&initial, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |me, cx| {
                if me.dialog_generation != generation {
                    return;
                }
                me.dialog_open = false;
                match result {
                    Ok(Ok(Some(local))) => {
                        me.engine.shrew().set_setting(
                            LAST_DIRECTORY,
                            json!(local
                                .parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string()),
                        );
                        let pending = PendingDownloadOverwrite {
                            target,
                            label,
                            remote,
                            local: local.clone(),
                        };
                        match std::fs::symlink_metadata(&local) {
                            Ok(_) => me.pending = Some(Pending::DownloadOverwrite(pending)),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                me.queue_download(pending, false, cx)
                            }
                            Err(error) => {
                                me.validation_error = Some(format!(
                                    "无法检查目标 {}：{error}。请选择其他本地路径。",
                                    local.display()
                                ))
                            }
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        me.validation_error = Some(format!("无法打开保存对话框：{error}。请重试。"))
                    }
                    Err(error) => {
                        me.validation_error = Some(format!("保存对话框意外关闭：{error}。请重试。"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn queue_download(
        &mut self,
        download: PendingDownloadOverwrite,
        overwrite: bool,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_job;
        self.next_job += 1;
        self.jobs.push(TransferJob {
            id,
            target: download.target,
            label: download.label,
            kind: TransferKind::Download {
                remote: download.remote,
                local: download.local,
                overwrite,
            },
            state: model::JobState::Queued,
            error: None,
            progress: None,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.pending = None;
        self.start_next(cx);
    }

    fn confirm_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Upload(upload) => {
                let id = self.next_job;
                self.next_job += 1;
                self.jobs.push(TransferJob {
                    id,
                    target: upload.target,
                    label: upload.label,
                    kind: TransferKind::Upload(upload.paths),
                    state: model::JobState::Queued,
                    error: None,
                    progress: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                });
                self.start_next(cx);
            }
            Pending::DownloadOverwrite(download) => self.queue_download(download, true, cx),
            Pending::Retry(index) => {
                if let Some(job) = self.jobs.iter_mut().find(|job| job.id == index) {
                    if job.state == model::JobState::Failure
                        || job.state == model::JobState::Cancelled
                    {
                        job.retry();
                        self.start_next(cx);
                    }
                }
            }
        }
        cx.notify();
    }

    fn retry(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.iter().find(|job| job.id == index) else {
            return;
        };
        if !matches!(
            job.state,
            model::JobState::Failure | model::JobState::Cancelled
        ) {
            return;
        }
        if matches!(job.kind, TransferKind::Upload(_)) {
            if !self.can_stage(cx) {
                return;
            }
            self.pending = Some(Pending::Retry(index));
            self.notice(
                "重试上传可能保留上次在服务器创建的文件，Rex 不会撤销这些文件。",
                cx,
            );
            cx.notify();
        } else if let Some(job) = self.jobs.iter_mut().find(|job| job.id == index) {
            job.retry();
            self.start_next(cx);
        }
    }

    fn start_next(&mut self, cx: &mut Context<Self>) {
        let Some(job) = model::next_job(&mut self.jobs) else {
            return;
        };
        let index = job.id;
        let target = job.target.clone();
        let kind = job.kind.clone();
        let cancel = job.cancel.clone();
        let engine = self.engine.clone();
        let tx = self.progress_tx.clone();
        cx.notify();
        cx.spawn(async move |_, _| {
            let result = async {
                // Backend validates capabilities on the exact retained control connection.
                let progress_tx = tx.clone();
                let callback: Arc<dyn Fn(TransferProgress) + Send + Sync> =
                    Arc::new(move |progress| {
                        let _ = progress_tx.try_send(TransferMessage::Progress { index, progress });
                    });
                match kind {
                    TransferKind::Upload(paths) => {
                        engine.upload_files(target, paths, cancel, callback).await
                    }
                    TransferKind::Download {
                        remote,
                        local,
                        overwrite,
                    } => {
                        engine
                            .download_file(target, remote, local, overwrite, cancel, callback)
                            .await
                    }
                }
            }
            .await
            .map_err(|error: CoreError| error.to_string());
            let _ = tx.send(TransferMessage::Finished { index, result }).await;
        })
        .detach();
    }

    fn prune_history(&mut self) {
        let keep_retry = match self.pending {
            Some(Pending::Retry(id)) => Some(id),
            _ => None,
        };
        let completed = self
            .jobs
            .iter()
            .filter(|job| {
                !matches!(
                    job.state,
                    model::JobState::Queued | model::JobState::Running
                )
            })
            .count();
        let mut discard = completed.saturating_sub(50);
        self.jobs.retain(|job| {
            if discard > 0
                && Some(job.id) != keep_retry
                && !matches!(
                    job.state,
                    model::JobState::Queued | model::JobState::Running
                )
            {
                discard -= 1;
                false
            } else {
                true
            }
        });
    }

    fn clear_completed(&mut self, cx: &mut Context<Self>) {
        let keep_retry = match self.pending {
            Some(Pending::Retry(id)) => Some(id),
            _ => None,
        };
        self.jobs.retain(|job| {
            Some(job.id) == keep_retry
                || matches!(
                    job.state,
                    model::JobState::Queued | model::JobState::Running
                )
        });
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let command = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        if self.filter.marked_utf16().is_some() {
            return;
        }
        if event.keystroke.key == "tab" {
            if event.keystroke.modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if event.keystroke.key == "escape" {
            self.pending = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.pending.is_some() && event.keystroke.key == "enter" {
            self.confirm_pending(cx);
            cx.stop_propagation();
            return;
        }
        if command && event.keystroke.key.eq_ignore_ascii_case("u") {
            self.choose_upload(cx);
            cx.stop_propagation();
            return;
        }
        if command && event.keystroke.key.eq_ignore_ascii_case("a") {
            self.filter.select_all();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if command && event.keystroke.key.eq_ignore_ascii_case("v") {
            if let Some(text) = crate::win32::clipboard_text() {
                self.filter.insert(&text);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if command || event.keystroke.modifiers.alt {
            return;
        }
        if text_field::edit(
            &mut self.filter,
            &event.keystroke.key,
            event.keystroke.modifiers.shift,
        )
        .is_some()
        {
            cx.stop_propagation();
            cx.notify();
        }
    }

    // One control seam keeps click and keyboard actions identical, including disabled guards.
    fn button(
        id: impl Into<gpui::ElementId>,
        content: impl IntoElement,
        active: bool,
        enabled: bool,
        hint: impl Into<String>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let hint = hint.into();
        div()
            .id(id)
            .tab_index(0)
            .tab_stop(enabled)
            .size(px(26.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .text_xs()
            .text_color(color(style::MUTED))
            .when(active, |el| {
                el.bg(color(style::SELECTED))
                    .text_color(color(style::LAVENDER))
            })
            .when(!enabled, |el| el.opacity(0.35))
            .hover(|el| el.bg(color(style::SELECTED)))
            .focus(|el| {
                el.bg(color(style::SELECTED))
                    .border_1()
                    .border_color(color(style::LAVENDER))
            })
            .tooltip(move |_, cx| cx.new(|_| Hint(hint.clone())).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(|event: &KeyDownEvent, _, cx| {
                // GPUI emits the keyboard click on key-up. Only shield the
                // parent input handler here; invoking the action would run twice.
                if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                    cx.stop_propagation();
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if enabled {
                    action(this, cx);
                }
                cx.stop_propagation();
            }))
            .child(content)
    }

    fn transfer_hint(&self, upload: bool) -> String {
        let operation = if upload { "上传" } else { "下载" };
        match &self.capabilities {
            _ if self.target.is_none() => "请先选择终端窗格".into(),
            None => "正在检查文件传输支持…".into(),
            Some(Err(error)) => format!("无法{operation}：{error}"),
            Some(Ok(caps)) if if upload { caps.upload } else { caps.download } => {
                if upload {
                    "上传文件（Ctrl/Cmd+U）".into()
                } else {
                    "下载文件，选择本地保存位置".into()
                }
            }
            Some(Ok(caps)) => format!(
                "无法{operation}：服务器 API {}，需要 API {}+",
                caps.revision,
                if upload { 11 } else { 13 }
            ),
        }
    }

    fn filter_box(
        &self,
        focused: bool,
        input_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let focus = input_focus;
        let view = cx.entity();
        div()
            .id("files-filter")
            .track_focus(&self.filter_focus)
            .tab_index(0)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.filter_focus);
                    cx.stop_propagation();
                }),
            )
            .relative()
            .flex_1()
            .min_w(px(0.))
            .h(px(20.))
            .line_height(px(20.))
            .overflow_hidden()
            .child(if focused {
                self.filter
                    .styled(color(style::LAVENDER), color(style::BACKGROUND))
                    .into_any_element()
            } else {
                div()
                    .text_color(color(style::MUTED))
                    .child(if self.filter.text().is_empty() {
                        "过滤文件…".to_owned()
                    } else {
                        self.filter.text().to_owned()
                    })
                    .into_any_element()
            })
            .when(focused && self.filter.text().is_empty(), |el| {
                el.child(
                    div()
                        .absolute()
                        .left(px(3.))
                        .top(px(0.))
                        .text_color(color(style::MUTED))
                        .child("过滤文件…"),
                )
            })
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, view.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full(),
            )
    }

    fn row(&self, entry: model::Entry, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let name = entry.name.clone();
        let folder = !entry.file;
        let mut row = div()
            .id(SharedString::from(format!("file:{}", entry.name)))
            .group("file-row")
            .flex()
            .items_center()
            .h(px(34.))
            .flex_shrink_0()
            .pl(px(10.))
            .pr(px(8.))
            .gap(px(6.))
            .min_w(px(0.))
            .hover(|el| el.bg(color(style::SEARCH)));
        if folder {
            let child = name.clone();
            row = row.child(
                Self::button(
                    SharedString::from(format!("enter:{name}")),
                    icon(FileIcon::Chevron, 12., color(style::MUTED)),
                    false,
                    true,
                    "进入文件夹（也可双击文件名）",
                    move |this, cx| this.enter(&child, cx),
                    cx,
                )
                .w(px(12.)),
            );
        } else {
            row = row.child(div().w(px(12.)).flex_shrink_0());
        }
        row = row
            .child(icon(
                if folder {
                    FileIcon::Folder
                } else {
                    FileIcon::File
                },
                16.,
                color(style::FOREGROUND),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .child(entry.name.clone()),
            );
        if entry.symlink {
            row = row.child(theme::hint(
                div()
                    .id(SharedString::from(format!("link:{name}")))
                    .child(icon(FileIcon::Link, 12., color(style::MUTED))),
                "符号链接",
            ));
        }
        if entry.file {
            row = row.child(
                Self::button(
                    SharedString::from(format!("download:{name}")),
                    icon(FileIcon::Download, 15., color(style::FOREGROUND)),
                    false,
                    self.download_allowed(),
                    self.transfer_hint(false),
                    move |this, cx| this.choose_download(&entry, cx),
                    cx,
                )
                .opacity(0.65)
                .group_hover("file-row", |el| el.opacity(1.))
                .in_focus(|el| el.opacity(1.)),
            );
        }
        if folder {
            row = row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        this.enter(&name, cx);
                    }
                }),
            );
        }
        row
    }

    fn list_scrollbar(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let scroll = self.list_scroll.clone();
        let drag = self.scroll_drag;
        let entity = cx.entity().downgrade();
        div()
            .id("files-scrollbar")
            .absolute()
            .top(px(0.0))
            .right(px(3.0))
            .w(px(7.0))
            .h_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    let viewport = f32::from(this.list_scroll.bounds().size.height);
                    let maximum = f32::from(this.list_scroll.max_offset().height);
                    if let Some((top, height)) = style::scroll_thumb(
                        viewport,
                        maximum,
                        f32::from(this.list_scroll.offset().y),
                    ) {
                        let position =
                            f32::from(event.position.y - this.list_scroll.bounds().origin.y);
                        if position < top || position > top + height {
                            let offset = -((position - height / 2.0)
                                / (viewport - height).max(1.0))
                            .clamp(0.0, 1.0)
                                * maximum;
                            this.list_scroll
                                .set_offset(gpui::point(px(0.0), px(offset)));
                        }
                        this.scroll_drag = Some((
                            f32::from(event.position.y),
                            f32::from(this.list_scroll.offset().y),
                        ));
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let viewport = f32::from(scroll.bounds().size.height);
                        let maximum = f32::from(scroll.max_offset().height);
                        if let Some((top, height)) =
                            style::scroll_thumb(viewport, maximum, f32::from(scroll.offset().y))
                        {
                            window.paint_quad(
                                gpui::fill(
                                    gpui::Bounds::new(
                                        gpui::point(
                                            bounds.origin.x + px(1.0),
                                            bounds.origin.y + px(top),
                                        ),
                                        gpui::size(px(5.0), px(height)),
                                    ),
                                    color(style::SELECTED),
                                )
                                .corner_radii(px(3.0)),
                            );
                            if let Some((start, offset)) = drag {
                                let moving = entity.clone();
                                window.on_mouse_event(
                                    move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                        if phase == gpui::DispatchPhase::Bubble {
                                            let _ = moving.update(cx, |me, cx| {
                                                let delta = (f32::from(event.position.y) - start)
                                                    * maximum
                                                    / (viewport - height).max(1.0);
                                                me.list_scroll.set_offset(gpui::point(
                                                    px(0.0),
                                                    px((offset - delta).clamp(-maximum, 0.0)),
                                                ));
                                                cx.notify();
                                            });
                                        }
                                    },
                                );
                            }
                        }
                        if drag.is_some() {
                            let released = entity.clone();
                            window.on_mouse_event(
                                move |event: &gpui::MouseUpEvent, phase, _, cx| {
                                    if event.button == MouseButton::Left
                                        && phase == gpui::DispatchPhase::Bubble
                                    {
                                        let _ = released.update(cx, |me, cx| {
                                            me.scroll_drag = None;
                                            cx.notify();
                                        });
                                    }
                                },
                            );
                        }
                    },
                )
                .size_full(),
            )
    }

    fn pending_view(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let (text, paths, disclaimer) = match self.pending.as_ref()? {
            Pending::Upload(upload) => (
                format!("上传 {} 个文件到 {}？\n{} · 窗格 {} / {}", upload.paths.len(), upload.label,
                    upload.target.address, upload.target.session_id, upload.target.block_id),
                Some(upload.paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n")),
                Some("Rex 使用原生上传存储，并非当前浏览目录；完成后会将路径自动粘贴到原始终端。"),
            ),
            Pending::DownloadOverwrite(download) => (
                format!("从 {} 下载并覆盖本地文件？\n{} · 窗格 {} / {}", download.label,
                    download.target.address, download.target.session_id, download.target.block_id),
                Some(download.local.display().to_string()), None,
            ),
            Pending::Retry(id) => (
                self.jobs.iter().find(|job| job.id == *id)
                    .map(|job| format!("重新上传到 {}（{} · 窗格 {} / {}）？", job.label,
                        job.target.address, job.target.session_id, job.target.block_id))
                    .unwrap_or_else(|| "上次任务已清除，请重新选择文件。".into()),
                None,
                Some("服务器可能保留上次上传的文件。重试会启动新的原生上传批次，并将路径自动粘贴到原始终端。"),
            ),
        };
        Some(
            div()
                .id("file-confirm")
                .flex_shrink_0()
                .p_2()
                .text_xs()
                .border_t_1()
                .border_color(color(style::LAVENDER))
                .bg(color(style::SEARCH))
                .child(text)
                .when_some(paths, |el, paths| {
                    el.child(
                        div()
                            .id("file-confirm-details")
                            .max_h(px(60.))
                            .overflow_y_scroll()
                            .text_color(color(style::MUTED))
                            .child(paths),
                    )
                })
                .when_some(disclaimer, |el, text| {
                    el.child(div().mt_1().text_color(color(style::MUTED)).child(text))
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .mt_1()
                        .child(
                            Self::button(
                                "file-confirm-no",
                                "取消",
                                false,
                                true,
                                "取消（Esc）",
                                |this, cx| {
                                    this.pending = None;
                                    cx.notify();
                                },
                                cx,
                            )
                            .w_auto()
                            .px_2(),
                        )
                        .child(
                            Self::button(
                                "file-confirm-yes",
                                "确认",
                                true,
                                true,
                                "确认（Enter）",
                                Self::confirm_pending,
                                cx,
                            )
                            .w_auto()
                            .px_2(),
                        ),
                ),
        )
    }
}

impl Focusable for FileBrowser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FileBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root_focused = self.focus.is_focused(window);
        let field_focused = root_focused || self.filter_focus.is_focused(window);
        let input_focus = if root_focused {
            self.focus.clone()
        } else {
            self.filter_focus.clone()
        };
        let needle = self.filter.text().to_owned();
        let upload_allowed = self.upload_allowed();
        let has_target = self.target.is_some();
        let tint = color(style::FOREGROUND);
        let mut root = div()
            .id("file-browser")
            .group("file-drop")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, window, cx| this.key(event, window, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus);
                    cx.stop_propagation();
                }),
            )
            .tab_group()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.))
            .overflow_hidden()
            .text_sm()
            .bg(color(style::BACKGROUND))
            .text_color(tint)
            .can_drop(move |value, _, _| upload_allowed && value.is::<ExternalPaths>())
            .drag_over::<ExternalPaths>(move |el, _, _, _| {
                if upload_allowed {
                    el.bg(color(style::SEARCH))
                } else {
                    el
                }
            })
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.begin_upload_paths(paths.paths().to_vec(), cx);
            }));

        let source = match (&self.target, &self.capabilities) {
            (Some(target), caps) => format!(
                "{}\n{}\n窗格 {} / {}\n{}",
                self.target_label,
                target.address,
                target.session_id,
                target.block_id,
                match caps {
                    Some(Ok(caps)) => format!(
                        "API {} · 上传{} · 下载{}",
                        caps.revision,
                        if caps.upload {
                            "可用"
                        } else {
                            "不可用（需 11+）"
                        },
                        if caps.download {
                            "可用"
                        } else {
                            "不可用（需 13+）"
                        }
                    ),
                    Some(Err(error)) => format!("文件传输不可用：{error}"),
                    None => "正在检查文件传输支持…".into(),
                }
            ),
            _ => "请先选择终端窗格".into(),
        };
        root = root.child(
            div()
                .flex()
                .items_center()
                .h(px(44.))
                .flex_shrink_0()
                .px(px(12.))
                .gap(px(3.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(30.))
                        .px(px(9.))
                        .gap(px(8.))
                        .rounded(px(7.))
                        .bg(color(style::SELECTED))
                        .child(icon(FileIcon::Folder, 17., tint))
                        .child("文件"),
                )
                .child(
                    div()
                        .id("files-source")
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(FileIcon::Info, 13., color(style::MUTED)))
                        .tooltip(move |_, cx| cx.new(|_| Hint(source.clone())).into()),
                )
                .child(div().flex_1())
                .child(Self::button(
                    "files-upload",
                    icon(FileIcon::Upload, 16., tint),
                    false,
                    upload_allowed,
                    self.transfer_hint(true),
                    Self::choose_upload,
                    cx,
                ))
                .child(Self::button(
                    "files-refresh",
                    icon(FileIcon::Refresh, 16., tint),
                    false,
                    has_target,
                    "刷新目录",
                    Self::refresh,
                    cx,
                ))
                .child(Self::button(
                    "files-hidden",
                    icon(FileIcon::Eye, 16., tint),
                    self.hidden,
                    has_target,
                    "显示 / 隐藏隐藏文件",
                    |this, cx| {
                        this.hidden = !this.hidden;
                        cx.notify();
                    },
                    cx,
                )),
        );

        root = root.child(
            div()
                .flex()
                .items_center()
                .h(px(36.))
                .flex_shrink_0()
                .px(px(12.))
                .gap(px(6.))
                .child(
                    div()
                        .id("files-breadcrumbs")
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_x_scroll()
                        .whitespace_nowrap()
                        .font_family("Consolas")
                        .text_xs()
                        .children(self.breadcrumbs(cx)),
                )
                .child(Self::button(
                    "files-follow",
                    icon(FileIcon::Terminal, 16., tint),
                    false,
                    has_target,
                    "跟随终端当前目录",
                    Self::follow_terminal_directory,
                    cx,
                )),
        );

        root = root.child(
            div()
                .flex()
                .items_center()
                .mx(px(8.))
                .mb(px(8.))
                .px(px(9.))
                .h(px(32.))
                .flex_shrink_0()
                .rounded(px(8.))
                .bg(color(style::SEARCH))
                .gap(px(6.))
                .child(icon(FileIcon::Search, 14., color(style::MUTED)))
                .child(self.filter_box(field_focused, input_focus, cx))
                .child(Self::button(
                    "files-case",
                    "Aa",
                    self.filter_options.case_sensitive,
                    true,
                    "区分大小写（仅过滤已加载文件）",
                    |this, cx| {
                        this.filter_options.case_sensitive = !this.filter_options.case_sensitive;
                        cx.notify();
                    },
                    cx,
                ))
                .child(Self::button(
                    "files-word",
                    "ab",
                    self.filter_options.whole_word,
                    true,
                    "全字匹配",
                    |this, cx| {
                        this.filter_options.whole_word = !this.filter_options.whole_word;
                        cx.notify();
                    },
                    cx,
                ))
                .child(Self::button(
                    "files-regex",
                    ".*",
                    self.filter_options.regex,
                    true,
                    "使用正则表达式",
                    |this, cx| {
                        this.filter_options.regex = !this.filter_options.regex;
                        cx.notify();
                    },
                    cx,
                )),
        );

        // Only unsupported capabilities and errors occupy space; ready/source details live in the tooltip.
        let capability_warning = match &self.capabilities {
            Some(Err(error)) => Some(format!("文件传输不可用：{error}")),
            Some(Ok(caps)) if !caps.upload || !caps.download => Some(format!(
                "{}{}",
                if caps.upload {
                    ""
                } else {
                    "上传需 API 11+。"
                },
                if caps.download {
                    ""
                } else {
                    "下载需 API 13+。"
                }
            )),
            _ => None,
        };
        for error in [
            capability_warning.as_ref(),
            self.validation_error.as_ref(),
            self.list_error.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            root = root.child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(color(style::WARNING))
                    .child(error.clone()),
            );
        }
        if let Some(prefix) = self
            .loaded_prefix
            .as_ref()
            .filter(|prefix| !prefix.is_empty())
        {
            root = root.child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(color(style::MUTED))
                    .child(format!("服务器前缀：{prefix} · 刷新可清除")),
            );
        }
        let mut contents = div()
            .id("files-directory-scroll")
            .size_full()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .pr(px(9.0))
            .pb_2();
        if let Some(listing) = &self.listing {
            if let Some(status) = &listing.status {
                let message: String = match status {
                    model::Status::Missing => "目录不存在".into(),
                    model::Status::NotDir => "此路径不是目录".into(),
                    model::Status::Denied => "无权访问此目录".into(),
                    model::Status::Other(detail) => format!("目录状态：{detail}"),
                };
                contents = contents.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(color(style::WARNING))
                        .child(message),
                );
            }
            if self.path != "/" && !self.path.is_empty() {
                contents = contents.child(
                    Self::button(
                        "files-parent",
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(icon(FileIcon::Folder, 16., tint))
                            .child(".."),
                        false,
                        true,
                        "返回上级目录",
                        |this, cx| this.navigate(model::up_path(&this.path), cx),
                        cx,
                    )
                    .w_full()
                    .h(px(34.))
                    .justify_start()
                    .pl(px(28.)),
                );
            }
            match model::filter_with_options(&listing.entries, &needle, self.filter_options) {
                Ok(mut entries) => {
                    if !self.hidden {
                        entries.retain(|entry| !entry.name.starts_with('.'));
                    }
                    // Listing order is already alphabetical; stable sorting only groups folders first.
                    entries.sort_by_key(|entry| entry.file);
                    if entries.is_empty() && listing.status.is_none() {
                        contents = contents.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_color(color(style::MUTED))
                                .child("没有匹配的文件"),
                        );
                    }
                    contents =
                        contents.children(entries.into_iter().map(|entry| self.row(entry, cx)));
                }
                Err(error) => {
                    root = root.child(
                        div()
                            .px_3()
                            .pb_1()
                            .text_xs()
                            .text_color(color(style::WARNING))
                            .child(error),
                    );
                }
            }
            if listing.truncated {
                let prefix = model::filter_prefix(&needle);
                contents = contents.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(color(style::MUTED))
                        .child("仅加载了部分目录，过滤仅作用于已加载文件。")
                        .child(
                            Self::button(
                                "load-prefix", "按前缀加载", false,
                                !self.filter_options.regex && !self.filter_options.whole_word,
                                if self.filter_options.regex || self.filter_options.whole_word {
                                    "正则 / 全字匹配仅过滤已加载文件；关闭后可按字面前缀加载"
                                } else {
                                    "从服务器按字面前缀加载（服务器不区分大小写，本地仍应用当前过滤选项）"
                                },
                                move |this, cx| {
                                    this.generation = this.generation.wrapping_add(1);
                                    this.load_directory(cx, Some(prefix.clone()));
                                }, cx,
                            ).w_auto().px_2()
                        )
                );
            }
        } else if !has_target {
            contents = contents.child(
                div()
                    .p_3()
                    .text_color(color(style::MUTED))
                    .child("选择终端窗格以浏览文件"),
            );
        } else if self.list_error.is_none() {
            contents = contents.child(
                div()
                    .p_3()
                    .text_color(color(style::MUTED))
                    .child("正在加载目录…"),
            );
        }
        root = root.child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(contents)
                .child(self.list_scrollbar(cx)),
        );
        // Confirmation stays pinned outside both independently scrolling lists.
        if let Some(pending) = self.pending_view(cx) {
            root = root.child(pending);
        }
        if !self.jobs.is_empty() {
            root = root.child(
                div()
                    .id("files-transfer-scroll")
                    .max_h(px(150.))
                    .min_h(px(48.))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(color(style::SELECTED))
                    .text_xs()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(format!("传输 · {}", self.jobs.len()))
                            .child(
                                Self::button(
                                    "clear-completed",
                                    "清除已完成",
                                    false,
                                    true,
                                    "清除已结束的传输记录",
                                    Self::clear_completed,
                                    cx,
                                )
                                .w_auto()
                                .px_2()
                                .ml_auto(),
                            ),
                    )
                    .children(self.jobs.iter().map(|job| self.job_view(job.id, job, cx))),
            );
        }
        if upload_allowed {
            root = root.child(
                div()
                    .absolute()
                    .top(px(48.))
                    .left(px(8.))
                    .right(px(8.))
                    .p_3()
                    .rounded(px(8.))
                    .bg(color(style::SELECTED))
                    .border_1()
                    .border_color(color(style::LAVENDER))
                    .invisible()
                    .group_drag_over::<ExternalPaths>("file-drop", |el| el.visible())
                    .child("松开以上传文件 · 上传前需确认目标窗格"),
            );
        }
        root
    }
}

impl FileBrowser {
    fn breadcrumbs(&self, cx: &mut Context<Self>) -> Vec<gpui::Stateful<gpui::Div>> {
        let mut out = Vec::new();
        let mut current = String::new();
        let components: Vec<_> = self
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if self.path.starts_with('/') {
            out.push(
                div()
                    .id("crumb-root")
                    .tab_index(0)
                    .on_key_down(|event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                            cx.stop_propagation();
                        }
                    })
                    .px_1()
                    .child("/")
                    .on_click(cx.listener(|this, _, _, cx| this.navigate("/".into(), cx))),
            );
        }
        for (index, component) in components.into_iter().enumerate() {
            current = model::join_path(&current, component);
            let path = current.clone();
            out.push(
                div()
                    .id(SharedString::from(format!("crumb:{path}")))
                    .tab_index(0)
                    .on_key_down(|event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                            cx.stop_propagation();
                        }
                    })
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .when(index > 0, |el| {
                        el.child(div().px_1().text_color(color(style::MUTED)).child("/"))
                    })
                    .child(component.to_owned())
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(path.clone(), cx))),
            );
        }
        if out.is_empty() {
            out.push(div().id("crumb-empty").px_1().child("终端目录"));
        }
        out
    }

    fn job_view(&self, index: usize, job: &TransferJob, cx: &mut Context<Self>) -> gpui::Div {
        let cancellable = matches!(
            job.state,
            model::JobState::Queued | model::JobState::Running
        );
        let retryable = matches!(
            job.state,
            model::JobState::Failure | model::JobState::Cancelled
        );
        let expanded = self.expanded_job == Some(index);
        let (name, description, kind) = match &job.kind {
            TransferKind::Upload(paths) => (
                format!("上传 {} 个文件", paths.len()),
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
                FileIcon::Upload,
            ),
            TransferKind::Download { remote, local, .. } => (
                format!("下载 {}", remote.rsplit('/').next().unwrap_or(remote)),
                format!("{remote} → {}", local.display()),
                FileIcon::Download,
            ),
        };
        let state = match job.state {
            model::JobState::Queued => "等待中",
            model::JobState::Running => "传输中",
            model::JobState::Success => "已完成",
            model::JobState::Failure => "失败",
            model::JobState::Cancelled => "已取消",
        };
        let source = format!(
            "{}\n{} · 窗格 {} / {}\n{}",
            job.label, job.target.address, job.target.session_id, job.target.block_id, description
        );
        let hint = match &job.error {
            Some(error) => format!("{source}\n{error}\n点击查看详情"),
            None => format!("{source}\n点击查看详情"),
        };
        let mut summary = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(icon(kind, 13., color(style::MUTED)))
            .child(div().flex_1().min_w(px(0.)).truncate().child(name))
            .child(
                div()
                    .text_color(color(if job.error.is_some() {
                        style::WARNING
                    } else {
                        style::MUTED
                    }))
                    .child(state),
            )
            .child(Self::button(
                SharedString::from(format!("job-details:{index}")),
                icon(FileIcon::Info, 13., color(style::MUTED)),
                expanded,
                true,
                hint,
                move |this, cx| {
                    this.expanded_job = if this.expanded_job == Some(index) {
                        None
                    } else {
                        Some(index)
                    };
                    cx.notify();
                },
                cx,
            ));
        if cancellable || retryable {
            summary = summary.child(
                Self::button(
                    SharedString::from(format!("job-action:{index}")),
                    if cancellable { "取消" } else { "重试" },
                    false,
                    true,
                    if cancellable {
                        "取消此传输"
                    } else {
                        "重试此传输"
                    },
                    move |this, cx| {
                        if cancellable {
                            if let Some(job) = this.jobs.iter_mut().find(|job| job.id == index) {
                                job.cancel();
                            }
                            cx.notify();
                        } else {
                            this.retry(index, cx);
                        }
                    },
                    cx,
                )
                .w_auto()
                .px_2(),
            );
        }
        let mut row = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1()
            .py_1()
            .border_b_1()
            .border_color(color(style::SELECTED))
            .child(summary);
        if let Some(error) = &job.error {
            let detail = error.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("job-error:{index}")))
                    .truncate()
                    .text_color(color(style::WARNING))
                    .child(error.clone())
                    .tooltip(move |_, cx| cx.new(|_| Hint(detail.clone())).into()),
            );
        }
        if expanded {
            row = row.child(div().text_color(color(style::MUTED)).child(source));
            if let Some(error) = &job.error {
                row = row.child(div().text_color(color(style::WARNING)).child(error.clone()));
            }
        }
        if let Some(progress) = &job.progress {
            row = row.child(div().truncate().text_color(color(style::MUTED)).child(
                match progress.total_bytes {
                    Some(total) => format!(
                        "{} · {} / {} 字节",
                        progress.file_name, progress.completed_bytes, total
                    ),
                    None => format!("{} · {} 字节", progress.file_name, progress.completed_bytes),
                },
            ));
        }
        row
    }
}

impl EntityInputHandler for FileBrowser {
    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let caret = self.filter.caret_utf16();
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.filter.marked_utf16()
    }
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.filter.unmark();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter.insert(text);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _selected: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter.mark(text);
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        element: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        let (_, before) = self.filter.caret_line();
        let run = window.text_style().to_run(before.len());
        let width = window
            .text_system()
            .shape_line(
                before.to_owned().into(),
                gpui::rems(0.875).to_pixels(window.rem_size()),
                &[run],
                None,
            )
            .width;
        Some(gpui::Bounds::new(
            gpui::point(element.origin.x + width, element.origin.y),
            gpui::size(px(1.), px(20.)),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn target_key(target: &FileTarget) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        target.server_id, target.address, target.session_id, target.block_id
    )
}

fn safe_filename(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = leaf.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        "download".into()
    } else {
        clean
    }
}
