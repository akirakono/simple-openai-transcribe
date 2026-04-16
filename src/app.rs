use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::gio;
use gtk::glib::{self, ControlFlow};
use tokio::runtime::Runtime;

use crate::config::{AppConfig, ConfigStore, parse_terms};
use crate::env;
use crate::history::{HistoryEntry, HistoryKind, HistoryStore};
use crate::ipc;
use crate::openai::{
    self, CorrectionResult, SessionCommand, SessionOptions, TranslationTarget, UiEvent,
};

const AUTO_FINISH_AFTER_TRANSCRIPTION: Duration = Duration::from_secs(10);
const AUTO_FINISH_WAITING_FOR_TRANSCRIPTION: Duration = Duration::from_secs(60);
const LOCAL_SPEECH_ACTIVE_GRACE: Duration = Duration::from_secs(1);

pub fn run_daemon() -> Result<()> {
    let store = ConfigStore::new()?;
    let history_store = HistoryStore::new()?;
    let config = store.load().unwrap_or_default();
    let api_key = env::require_api_key()?;

    let runtime = Arc::new(Runtime::new().context("failed to create background Tokio runtime")?);
    let (ipc_tx, ipc_rx) = channel();
    let (ui_tx, ui_rx) = channel();
    let _ipc_handle = ipc::spawn_server(ipc_tx)?;
    let ipc_rx = Rc::new(RefCell::new(Some(ipc_rx)));
    let ui_rx = Rc::new(RefCell::new(Some(ui_rx)));

    let app = adw::Application::builder()
        .application_id("dev.akirakono.simple-openai-transcribe")
        .build();

    let runtime_for_activate = Arc::clone(&runtime);
    let store_for_activate = store.clone();
    let api_key_for_activate = api_key.clone();
    let config_for_activate = config.clone();
    let ipc_rx_for_activate = Rc::clone(&ipc_rx);
    let ui_rx_for_activate = Rc::clone(&ui_rx);
    let ui_tx_for_activate = ui_tx.clone();

    app.connect_activate(move |application| {
        let ipc_rx = ipc_rx_for_activate
            .borrow_mut()
            .take()
            .expect("application.activate should only be used once");
        let ui_rx = ui_rx_for_activate
            .borrow_mut()
            .take()
            .expect("application.activate should only be used once");

        let ui = Ui::build(
            application,
            store_for_activate.clone(),
            history_store.clone(),
            config_for_activate.clone(),
            api_key_for_activate.clone(),
            Arc::clone(&runtime_for_activate),
            ipc_rx,
            ui_rx,
            ui_tx_for_activate.clone(),
        );

        if let Err(error) = ui {
            tracing::error!("failed to build UI: {error:#}");
            application.quit();
        }
    });

    app.connect_shutdown(|_| {
        ipc::cleanup_socket();
    });

    app.run_with_args::<&str>(&[]);
    Ok(())
}

struct Ui {
    _state: Rc<RefCell<UiState>>,
}

struct UiState {
    app: adw::Application,
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    transcript_buffer: gtk::TextBuffer,
    preview_buffer: gtk::TextBuffer,
    english_buffer: gtk::TextBuffer,
    status_label: gtk::Label,
    spinner: gtk::Spinner,
    record_button_label: gtk::Label,
    record_button_icon: gtk::Image,
    clipboard: Option<arboard::Clipboard>,
    runtime: Arc<Runtime>,
    config_store: ConfigStore,
    history_store: HistoryStore,
    config: AppConfig,
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
    api_key: String,
    active_session: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    correction_generation: u64,
    correction_in_flight: bool,
    pending_correction_text: String,
    pending_correction_snapshot: String,
    active_recording_history: Option<RecordingHistoryContext>,
    latest_recording_history: Option<FinishedRecordingHistory>,
    finish_requested: bool,
    last_transcription_activity: Option<Instant>,
    last_local_speech_activity: Option<Instant>,
    waiting_for_transcription_since: Option<Instant>,
    awaiting_transcription_after_local_speech: bool,
}

#[derive(Debug, Clone)]
struct RecordingHistoryContext {
    transcript_before: String,
}

#[derive(Debug, Clone)]
struct FinishedRecordingHistory {
    entry_id: String,
    transcript_before: String,
}

impl Ui {
    #[allow(clippy::too_many_arguments)]
    fn build(
        app: &adw::Application,
        config_store: ConfigStore,
        history_store: HistoryStore,
        config: AppConfig,
        api_key: String,
        runtime: Arc<Runtime>,
        ipc_rx: Receiver<ipc::Command>,
        ui_rx: Receiver<UiEvent>,
        ui_tx: std::sync::mpsc::Sender<UiEvent>,
    ) -> Result<Self> {
        install_css();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Simple OpenAI Transcribe")
            .default_width(config.window_width)
            .default_height(config.window_height)
            .build();

        let toast_overlay = adw::ToastOverlay::new();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        toast_overlay.set_child(Some(&root));
        window.set_content(Some(&toast_overlay));

        let header = adw::HeaderBar::new();
        header.add_css_class("flat");

        let (record_button, record_button_icon, record_button_label) =
            labelled_icon_button("media-record-symbolic", "録音 (F1 / Ctrl+R)");
        let (settings_button, _, _) =
            labelled_icon_button("emblem-system-symbolic", "設定 (Ctrl+,)");
        let (history_button, _, _) =
            labelled_icon_button("document-open-recent-symbolic", "履歴 (Ctrl+H)");

        header.pack_start(&record_button);
        header.pack_start(&history_button);
        header.pack_end(&settings_button);

        let spinner = gtk::Spinner::new();
        spinner.set_spinning(false);
        let status_label = gtk::Label::new(Some("待機中"));
        status_label.add_css_class("heading");
        let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        status_box.append(&spinner);
        status_box.append(&status_label);
        header.set_title_widget(Some(&status_box));
        root.append(&header);

        let preview_section = gtk::Box::new(gtk::Orientation::Vertical, 4);
        preview_section.set_margin_top(8);
        preview_section.set_margin_bottom(8);
        preview_section.set_margin_start(12);
        preview_section.set_margin_end(12);
        let preview_title = gtk::Label::new(Some("Live Preview"));
        preview_title.set_xalign(0.0);
        preview_title.add_css_class("caption-heading");
        let preview_buffer = gtk::TextBuffer::new(None);
        let preview_view = gtk::TextView::with_buffer(&preview_buffer);
        preview_view.set_editable(false);
        preview_view.set_cursor_visible(false);
        preview_view.set_can_focus(false);
        preview_view.set_wrap_mode(gtk::WrapMode::WordChar);
        preview_view.add_css_class("live-preview");
        let preview_scroller = gtk::ScrolledWindow::builder()
            .child(&preview_view)
            .hexpand(true)
            .min_content_height(72)
            .build();
        preview_section.append(&preview_title);
        preview_section.append(&preview_scroller);
        root.append(&preview_section);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_bottom(12);
        root.append(&content);

        let transcript_buffer = gtk::TextBuffer::new(None);
        let english_buffer = gtk::TextBuffer::new(None);

        let left_pane = editor_pane("日本語 (TRANSCRIPTION)", &transcript_buffer, true);
        let right_pane = editor_pane("英語 (TRANSLATION)", &english_buffer, true);
        let action_pane = translation_actions_pane();

        let right_split = gtk::Paned::new(gtk::Orientation::Horizontal);
        right_split.set_wide_handle(true);
        right_split.set_resize_start_child(false);
        right_split.set_shrink_start_child(false);
        right_split.set_start_child(Some(&action_pane.container));
        right_split.set_end_child(Some(&right_pane));
        right_split.set_position(84);

        let root_split = gtk::Paned::new(gtk::Orientation::Horizontal);
        root_split.set_wide_handle(true);
        root_split.set_resize_start_child(true);
        root_split.set_start_child(Some(&left_pane));
        root_split.set_end_child(Some(&right_split));
        root_split.set_position(config.window_width / 2);
        content.append(&root_split);

        let state = Rc::new(RefCell::new(UiState {
            app: app.clone(),
            window: window.clone(),
            toast_overlay: toast_overlay.clone(),
            transcript_buffer: transcript_buffer.clone(),
            preview_buffer: preview_buffer.clone(),
            english_buffer: english_buffer.clone(),
            status_label: status_label.clone(),
            spinner: spinner.clone(),
            record_button_label: record_button_label.clone(),
            record_button_icon: record_button_icon.clone(),
            clipboard: arboard::Clipboard::new().ok(),
            runtime,
            config_store,
            history_store,
            config,
            ui_tx,
            api_key,
            active_session: None,
            correction_generation: 0,
            correction_in_flight: false,
            pending_correction_text: String::new(),
            pending_correction_snapshot: String::new(),
            active_recording_history: None,
            latest_recording_history: None,
            finish_requested: false,
            last_transcription_activity: None,
            last_local_speech_activity: None,
            waiting_for_transcription_since: None,
            awaiting_transcription_after_local_speech: false,
        }));

        Self::install_actions(app, &window, &state);

        {
            let state = Rc::clone(&state);
            record_button.connect_clicked(move |_| {
                Ui::toggle_recording(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            history_button.connect_clicked(move |_| {
                Ui::show_history(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            settings_button.connect_clicked(move |_| {
                Ui::ensure_recording_finished(&state);
                Ui::show_preferences(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            action_pane.to_english.connect_clicked(move |_| {
                Ui::translate_to_english(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            action_pane.to_japanese.connect_clicked(move |_| {
                Ui::translate_to_japanese(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            window.connect_close_request(move |_| {
                Ui::quit(&state);
                glib::Propagation::Stop
            });
        }

        {
            let state = Rc::clone(&state);
            window.connect_notify_local(Some("is-active"), move |window, _| {
                if !window.property::<bool>("is-active")
                    && state.borrow().active_session.is_some()
                    && !state.borrow().finish_requested
                {
                    Ui::finish_transcription(&state);
                }
            });
        }

        {
            let state = Rc::clone(&state);
            glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
                while let Ok(command) = ipc_rx.try_recv() {
                    match command {
                        ipc::Command::Show => Ui::show_window(&state),
                        ipc::Command::Start => {
                            Ui::show_window(&state);
                            Ui::start_transcription(&state);
                        }
                        ipc::Command::Finish => Ui::finish_transcription(&state),
                        ipc::Command::Quit => {
                            Ui::quit(&state);
                            return ControlFlow::Break;
                        }
                        ipc::Command::GetState => {}
                    }
                }
                ControlFlow::Continue
            });
        }

        {
            let state = Rc::clone(&state);
            glib::timeout_add_local(Duration::from_secs(1), move || {
                let should_finish = {
                    let state = state.borrow();
                    state.active_session.is_some()
                        && !state.finish_requested
                        && state.should_auto_finish()
                };
                if should_finish {
                    Ui::finish_transcription(&state);
                }
                ControlFlow::Continue
            });
        }

        {
            let state = Rc::clone(&state);
            glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
                while let Ok(event) = ui_rx.try_recv() {
                    match event {
                        UiEvent::Status(message) => {
                            let is_busy = matches!(
                                message.as_str(),
                                "録音中" | "最終結果を待っています" | "英訳中" | "和訳中"
                            );
                            state.borrow_mut().set_status(&message, is_busy);
                        }
                        UiEvent::Preview(text) => {
                            if !text.is_empty() {
                                let mut state = state.borrow_mut();
                                state.correction_generation += 1;
                                state.touch_transcription_activity();
                            }
                            state.borrow().preview_buffer.set_text(&text);
                        }
                        UiEvent::LocalSpeechDetected => {
                            state.borrow_mut().touch_local_speech_activity();
                        }
                        UiEvent::AppendCommitted(text) => {
                            let buffer = state.borrow().transcript_buffer.clone();
                            append_to_buffer(&buffer, &text);
                            state.borrow_mut().touch_transcription_activity();
                            Ui::schedule_correction(&state, &text);
                        }
                        UiEvent::CorrectionTimerElapsed(generation) => {
                            Ui::run_correction_if_due(&state, generation);
                        }
                        UiEvent::CorrectionReady(result) => {
                            Ui::apply_correction(&state, result);
                        }
                        UiEvent::CorrectionFailed(generation) => {
                            let mut state = state.borrow_mut();
                            if generation == state.correction_generation {
                                state.correction_in_flight = false;
                            }
                        }
                        UiEvent::TranslationStarted(target) => {
                            let label = match target {
                                TranslationTarget::English => "英訳中",
                                TranslationTarget::Japanese => "和訳中",
                            };
                            state.borrow_mut().set_status(label, true);
                        }
                        UiEvent::TranslationReady(target, text) => {
                            Ui::apply_translation(&state, target, &text);
                        }
                        UiEvent::RecordingStarted(session_tx) => {
                            let mut state = state.borrow_mut();
                            state.active_session = Some(session_tx);
                            state.finish_requested = false;
                            state.touch_transcription_activity();
                            state.set_recording_active(true);
                            state.set_status("録音中", true);
                        }
                        UiEvent::RecordingFinished => {
                            let mut state = state.borrow_mut();
                            state.active_session = None;
                            state.finish_requested = false;
                            state.last_transcription_activity = None;
                            state.last_local_speech_activity = None;
                            state.waiting_for_transcription_since = None;
                            state.awaiting_transcription_after_local_speech = false;
                            state.set_recording_active(false);
                            state.set_status("待機中", false);
                            state.persist_transcription_history();
                            if state.config.copy_on_finish {
                                let text = buffer_text(&state.transcript_buffer);
                                if !text.trim().is_empty() {
                                    if let Err(error) =
                                        copy_to_clipboard(&mut state.clipboard, &text)
                                    {
                                        state.set_status(
                                            &format!("clipboard へのコピーに失敗: {error}"),
                                            false,
                                        );
                                    } else {
                                        state.show_toast("文字起こしをコピーしました");
                                    }
                                }
                            }
                        }
                        UiEvent::Error(error) => {
                            tracing::error!("ui error: {error}");
                            state.borrow_mut().set_status(&error, false);
                        }
                    }
                }
                ControlFlow::Continue
            });
        }

        window.hide();
        Ok(Self { _state: state })
    }

    fn install_actions(
        app: &adw::Application,
        window: &adw::ApplicationWindow,
        state: &Rc<RefCell<UiState>>,
    ) {
        let toggle_record = gio::SimpleAction::new("toggle-record", None);
        {
            let state = Rc::clone(state);
            toggle_record.connect_activate(move |_, _| {
                Ui::toggle_recording(&state);
            });
        }
        window.add_action(&toggle_record);

        let to_english = gio::SimpleAction::new("translate-english", None);
        {
            let state = Rc::clone(state);
            to_english.connect_activate(move |_, _| {
                Ui::translate_to_english(&state);
            });
        }
        window.add_action(&to_english);

        let to_japanese = gio::SimpleAction::new("translate-japanese", None);
        {
            let state = Rc::clone(state);
            to_japanese.connect_activate(move |_, _| {
                Ui::translate_to_japanese(&state);
            });
        }
        window.add_action(&to_japanese);

        let preferences = gio::SimpleAction::new("preferences", None);
        {
            let state = Rc::clone(state);
            preferences.connect_activate(move |_, _| {
                Ui::show_preferences(&state);
            });
        }
        window.add_action(&preferences);

        let history = gio::SimpleAction::new("history", None);
        {
            let state = Rc::clone(state);
            history.connect_activate(move |_, _| {
                Ui::show_history(&state);
            });
        }
        window.add_action(&history);

        app.set_accels_for_action("win.toggle-record", &["F1", "<Primary>r"]);
        app.set_accels_for_action("win.translate-english", &["<Primary>Right"]);
        app.set_accels_for_action("win.translate-japanese", &["<Primary>Left"]);
        app.set_accels_for_action("win.preferences", &["<Primary>comma"]);
        app.set_accels_for_action("win.history", &["<Primary>h"]);
        app.set_accels_for_action("win.close-window", &["Escape"]);

        let close_window = gio::SimpleAction::new("close-window", None);
        {
            let state = Rc::clone(state);
            close_window.connect_activate(move |_, _| {
                Ui::quit(&state);
            });
        }
        window.add_action(&close_window);
    }

    fn show_window(state: &Rc<RefCell<UiState>>) {
        state.borrow().window.present();
    }

    fn toggle_recording(state: &Rc<RefCell<UiState>>) {
        if state.borrow().active_session.is_some() {
            Ui::finish_transcription(state);
        } else {
            Ui::start_transcription(state);
        }
    }

    fn start_transcription(state: &Rc<RefCell<UiState>>) {
        let mut state_mut = state.borrow_mut();
        if state_mut.active_session.is_some() {
            state_mut.set_status("すでに録音中です", false);
            return;
        }

        state_mut.preview_buffer.set_text("");
        state_mut.pending_correction_text.clear();
        state_mut.pending_correction_snapshot.clear();
        state_mut.latest_recording_history = None;
        state_mut.finish_requested = false;
        state_mut.last_transcription_activity = Some(Instant::now());
        state_mut.last_local_speech_activity = None;
        state_mut.waiting_for_transcription_since = None;
        state_mut.awaiting_transcription_after_local_speech = false;
        state_mut.active_recording_history = Some(RecordingHistoryContext {
            transcript_before: buffer_text(&state_mut.transcript_buffer),
        });

        let options = SessionOptions {
            api_key: state_mut.api_key.clone(),
            terms: state_mut.config.terms.clone(),
            transcription_prompt: state_mut.config.transcription_system_prompt.clone(),
            vad_threshold: state_mut.config.vad_threshold,
            vad_silence_ms: state_mut.config.vad_silence_ms,
        };

        let ui_tx = state_mut.ui_tx.clone();
        let runtime = Arc::clone(&state_mut.runtime);
        state_mut.set_status("録音中", true);
        drop(state_mut);

        runtime.spawn(async move {
            match openai::start_transcription(options, ui_tx.clone()).await {
                Ok(session_tx) => {
                    let _ = ui_tx.send(UiEvent::RecordingStarted(session_tx));
                }
                Err(error) => {
                    tracing::error!("failed to start transcription task: {error:#}");
                    let _ = ui_tx.send(UiEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn finish_transcription(state: &Rc<RefCell<UiState>>) {
        let session = {
            let mut state = state.borrow_mut();
            if state.finish_requested {
                return;
            }
            state.finish_requested = true;
            state.active_session.clone()
        };
        if let Some(session) = session {
            let _ = session.send(SessionCommand::Finish);
            state
                .borrow_mut()
                .set_status("最終結果を待っています", true);
        } else {
            state.borrow_mut().finish_requested = false;
            let transcript = buffer_text(&state.borrow().transcript_buffer);
            if transcript.trim().is_empty() {
                return;
            }
            let mut state = state.borrow_mut();
            if let Err(error) = copy_to_clipboard(&mut state.clipboard, &transcript) {
                state.set_status(&format!("clipboard へのコピーに失敗: {error}"), false);
            } else {
                state.show_toast("文字起こしをコピーしました");
            }
        }
    }

    fn translate_to_english(state: &Rc<RefCell<UiState>>) {
        Ui::ensure_recording_finished(state);
        let source = buffer_text(&state.borrow().transcript_buffer);
        if source.trim().is_empty() {
            state
                .borrow_mut()
                .set_status("日本語テキストがありません", false);
            return;
        }

        let (api_key, instructions, ui_tx, runtime) = {
            let state = state.borrow();
            (
                state.api_key.clone(),
                state.config.translate_to_english_prompt.clone(),
                state.ui_tx.clone(),
                Arc::clone(&state.runtime),
            )
        };

        let _ = ui_tx.send(UiEvent::TranslationStarted(TranslationTarget::English));
        runtime.spawn(async move {
            match openai::translate_text(&api_key, &source, &instructions).await {
                Ok(result) => {
                    let _ = ui_tx.send(UiEvent::TranslationReady(
                        TranslationTarget::English,
                        result,
                    ));
                }
                Err(error) => {
                    tracing::error!("english translation request failed: {error:#}");
                    let _ = ui_tx.send(UiEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn translate_to_japanese(state: &Rc<RefCell<UiState>>) {
        Ui::ensure_recording_finished(state);
        let source = buffer_text(&state.borrow().english_buffer);
        if source.trim().is_empty() {
            state
                .borrow_mut()
                .set_status("英語テキストがありません", false);
            return;
        }

        let (api_key, instructions, ui_tx, runtime) = {
            let state = state.borrow();
            (
                state.api_key.clone(),
                state.config.translate_to_japanese_prompt.clone(),
                state.ui_tx.clone(),
                Arc::clone(&state.runtime),
            )
        };

        let _ = ui_tx.send(UiEvent::TranslationStarted(TranslationTarget::Japanese));
        runtime.spawn(async move {
            match openai::translate_text(&api_key, &source, &instructions).await {
                Ok(result) => {
                    let _ = ui_tx.send(UiEvent::TranslationReady(
                        TranslationTarget::Japanese,
                        result,
                    ));
                }
                Err(error) => {
                    tracing::error!("japanese translation request failed: {error:#}");
                    let _ = ui_tx.send(UiEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn apply_translation(state: &Rc<RefCell<UiState>>, target: TranslationTarget, text: &str) {
        let mut state = state.borrow_mut();
        match target {
            TranslationTarget::English => {
                state.english_buffer.set_text(text);
                state.persist_translation_history(TranslationTarget::English, text);
                if let Err(error) = copy_to_clipboard(&mut state.clipboard, text) {
                    state.set_status(&format!("clipboard へのコピーに失敗: {error}"), false);
                } else {
                    state.show_toast("英訳をコピーしました");
                }
            }
            TranslationTarget::Japanese => {
                state.transcript_buffer.set_text(text);
                state.persist_translation_history(TranslationTarget::Japanese, text);
                if let Err(error) = copy_to_clipboard(&mut state.clipboard, text) {
                    state.set_status(&format!("clipboard へのコピーに失敗: {error}"), false);
                } else {
                    state.show_toast("和訳をコピーしました");
                }
            }
        }
        state.set_status("待機中", false);
    }

    fn quit(state: &Rc<RefCell<UiState>>) {
        let mut state = state.borrow_mut();
        let width = state.window.width();
        let height = state.window.height();
        if width > 0 {
            state.config.window_width = width;
        }
        if height > 0 {
            state.config.window_height = height;
        }
        if let Some(session) = state.active_session.take() {
            let _ = session.send(SessionCommand::Stop);
        }
        let _ = state.config_store.save(&state.config);
        state.app.quit();
    }

    fn schedule_correction(state: &Rc<RefCell<UiState>>, appended_text: &str) {
        let appended_text = appended_text.trim();
        if appended_text.is_empty() {
            return;
        }

        let (generation, ui_tx, runtime) = {
            let mut state = state.borrow_mut();
            if state.pending_correction_text.is_empty() {
                state.pending_correction_text = appended_text.to_string();
            } else {
                state.pending_correction_text.push('\n');
                state.pending_correction_text.push_str(appended_text);
            }
            state.pending_correction_snapshot = buffer_text(&state.transcript_buffer);
            state.correction_generation += 1;
            (
                state.correction_generation,
                state.ui_tx.clone(),
                Arc::clone(&state.runtime),
            )
        };

        runtime.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = ui_tx.send(UiEvent::CorrectionTimerElapsed(generation));
        });
    }

    fn run_correction_if_due(state: &Rc<RefCell<UiState>>, generation: u64) {
        let (api_key, terms, original, snapshot, ui_tx, runtime) = {
            let mut state = state.borrow_mut();
            if generation != state.correction_generation
                || state.correction_in_flight
                || state.pending_correction_text.trim().is_empty()
            {
                return;
            }

            state.correction_in_flight = true;
            (
                state.api_key.clone(),
                state.config.terms.clone(),
                state.pending_correction_text.clone(),
                state.pending_correction_snapshot.clone(),
                state.ui_tx.clone(),
                Arc::clone(&state.runtime),
            )
        };

        runtime.spawn(async move {
            match openai::correct_transcript_text(&api_key, &terms, &original).await {
                Ok(corrected) => {
                    let _ = ui_tx.send(UiEvent::CorrectionReady(CorrectionResult {
                        generation,
                        original,
                        snapshot,
                        corrected,
                    }));
                }
                Err(error) => {
                    tracing::error!("transcript correction failed: {error:#}");
                    let _ = ui_tx.send(UiEvent::CorrectionFailed(generation));
                }
            }
        });
    }

    fn apply_correction(state: &Rc<RefCell<UiState>>, result: CorrectionResult) {
        let mut state = state.borrow_mut();
        state.correction_in_flight = false;

        if result.generation != state.correction_generation {
            return;
        }

        let current = buffer_text(&state.transcript_buffer);
        if current != result.snapshot {
            tracing::info!("skipping auto-correction because transcript changed meanwhile");
            state.pending_correction_text.clear();
            state.pending_correction_snapshot.clear();
            return;
        }

        let original = result.original.trim();
        let corrected = result.corrected.trim();
        if original.is_empty() || corrected.is_empty() || original == corrected {
            state.pending_correction_text.clear();
            state.pending_correction_snapshot.clear();
            return;
        }

        let Some(prefix) = current.strip_suffix(original) else {
            tracing::info!("skipping auto-correction because transcript tail no longer matches");
            state.pending_correction_text.clear();
            state.pending_correction_snapshot.clear();
            return;
        };

        state
            .transcript_buffer
            .set_text(&format!("{prefix}{corrected}"));
        state.pending_correction_text.clear();
        state.pending_correction_snapshot.clear();
        state.update_latest_recording_history();
    }

    fn show_preferences(state: &Rc<RefCell<UiState>>) {
        Ui::ensure_recording_finished(state);
        let snapshot = state.borrow().config.clone();

        let prefs = adw::PreferencesWindow::builder()
            .transient_for(&state.borrow().window)
            .modal(true)
            .title("設定")
            .default_width(760)
            .default_height(680)
            .build();

        let page = adw::PreferencesPage::new();

        let prompt_group = adw::PreferencesGroup::builder()
            .title("System Prompt")
            .description("文字起こしと翻訳に使う system prompt を編集します。")
            .build();

        let (transcription_widget, transcription_buffer) = preferences_editor(
            "文字起こし用 Prompt",
            &snapshot.transcription_system_prompt,
            120,
        );
        let (en_widget, en_buffer) =
            preferences_editor("英訳用 Prompt", &snapshot.translate_to_english_prompt, 100);
        let (ja_widget, ja_buffer) =
            preferences_editor("和訳用 Prompt", &snapshot.translate_to_japanese_prompt, 100);

        prompt_group.add(&transcription_widget);
        prompt_group.add(&en_widget);
        prompt_group.add(&ja_widget);
        page.add(&prompt_group);

        let terms_group = adw::PreferencesGroup::builder()
            .title("固有名詞")
            .description("1行1単語で登録します。文字起こし補助と transcript 補正に使います。")
            .build();
        let (terms_widget, terms_buffer) =
            preferences_editor("認識辞書", &snapshot.terms.join("\n"), 180);
        terms_group.add(&terms_widget);
        page.add(&terms_group);

        prefs.add(&page);

        {
            let state = Rc::clone(state);
            transcription_buffer.connect_changed(move |buffer| {
                let text = buffer_text(buffer);
                let mut state = state.borrow_mut();
                state.config.transcription_system_prompt = text;
                state.persist_config();
            });
        }

        {
            let state = Rc::clone(state);
            en_buffer.connect_changed(move |buffer| {
                let text = buffer_text(buffer);
                let mut state = state.borrow_mut();
                state.config.translate_to_english_prompt = text;
                state.persist_config();
            });
        }

        {
            let state = Rc::clone(state);
            ja_buffer.connect_changed(move |buffer| {
                let text = buffer_text(buffer);
                let mut state = state.borrow_mut();
                state.config.translate_to_japanese_prompt = text;
                state.persist_config();
            });
        }

        {
            let state = Rc::clone(state);
            terms_buffer.connect_changed(move |buffer| {
                let text = buffer_text(buffer);
                let mut state = state.borrow_mut();
                state.config.terms = parse_terms(&text);
                state.persist_config();
            });
        }

        prefs.present();
    }

    fn show_history(state: &Rc<RefCell<UiState>>) {
        Ui::ensure_recording_finished(state);
        let (parent, history_store) = {
            let state = state.borrow();
            (state.window.clone(), state.history_store.clone())
        };

        let entries = match history_store.load_all() {
            Ok(entries) => entries,
            Err(error) => {
                state
                    .borrow_mut()
                    .set_status(&format!("履歴の読み込みに失敗: {error}"), false);
                return;
            }
        };

        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .title("履歴")
            .default_width(1080)
            .default_height(760)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        let title = gtk::Label::new(Some("履歴"));
        title.add_css_class("title-2");
        header.set_title_widget(Some(&title));
        root.append(&header);

        let key_controller = gtk::EventControllerKey::new();
        {
            let window = window.clone();
            key_controller.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    window.close();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        window.add_controller(key_controller);

        let split = gtk::Paned::new(gtk::Orientation::Horizontal);
        split.set_position(320);
        split.set_wide_handle(true);
        split.set_vexpand(true);
        split.set_margin_top(12);
        split.set_margin_bottom(12);
        split.set_margin_start(12);
        split.set_margin_end(12);
        root.append(&split);
        window.set_content(Some(&root));

        let entries = Rc::new(entries);
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Single);

        for entry in entries.iter() {
            let row = gtk::ListBoxRow::new();
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
            box_.set_margin_top(10);
            box_.set_margin_bottom(10);
            box_.set_margin_start(10);
            box_.set_margin_end(10);

            let title = gtk::Label::new(Some(&history_entry_title(entry)));
            title.set_xalign(0.0);
            title.add_css_class("heading");
            let subtitle = gtk::Label::new(Some(&history_entry_subtitle(entry)));
            subtitle.set_xalign(0.0);
            subtitle.add_css_class("dim-label");
            subtitle.set_wrap(true);

            box_.append(&title);
            box_.append(&subtitle);
            row.set_child(Some(&box_));
            list.append(&row);
        }

        let list_scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hexpand(true)
            .vexpand(true)
            .min_content_width(280)
            .build();
        split.set_start_child(Some(&list_scroller));

        let detail = gtk::Box::new(gtk::Orientation::Vertical, 12);
        detail.set_hexpand(true);
        detail.set_vexpand(true);

        let summary_label = gtk::Label::new(Some("履歴を選択してください"));
        summary_label.set_xalign(0.0);
        summary_label.add_css_class("title-3");
        detail.append(&summary_label);

        let source_buffer = gtk::TextBuffer::new(None);
        let japanese_buffer = gtk::TextBuffer::new(None);
        let english_buffer = gtk::TextBuffer::new(None);
        detail.append(&readonly_editor_pane("ソース", &source_buffer));
        detail.append(&readonly_editor_pane("日本語", &japanese_buffer));
        detail.append(&readonly_editor_pane("英語", &english_buffer));

        let detail_scroller = gtk::ScrolledWindow::builder()
            .child(&detail)
            .hexpand(true)
            .vexpand(true)
            .build();
        split.set_end_child(Some(&detail_scroller));

        {
            let entries = Rc::clone(&entries);
            list.connect_row_selected(move |_, row| {
                let Some(row) = row else {
                    return;
                };
                let Some(entry) = entries.get(row.index() as usize) else {
                    return;
                };

                summary_label.set_text(&format!(
                    "{}  {}",
                    history_kind_label(entry.kind),
                    format_timestamp(entry.created_at_ms)
                ));
                source_buffer.set_text(&entry.source_text);
                japanese_buffer.set_text(&entry.japanese_text);
                english_buffer.set_text(&entry.english_text);
            });
        }

        if !entries.is_empty() {
            if let Some(first_row) = list.row_at_index(0) {
                list.select_row(Some(&first_row));
            }
        }

        window.present();
    }

    fn ensure_recording_finished(state: &Rc<RefCell<UiState>>) {
        let should_finish = {
            let state = state.borrow();
            state.active_session.is_some() && !state.finish_requested
        };
        if should_finish {
            Ui::finish_transcription(state);
        }
    }
}

impl UiState {
    fn set_status(&mut self, status: &str, busy: bool) {
        self.status_label.set_text(status);
        self.spinner.set_spinning(busy);
    }

    fn set_recording_active(&mut self, recording: bool) {
        self.record_button_icon.set_icon_name(Some(if recording {
            "media-playback-stop-symbolic"
        } else {
            "media-record-symbolic"
        }));
        self.record_button_label.set_text(if recording {
            "停止 (F1 / Ctrl+R)"
        } else {
            "録音 (F1 / Ctrl+R)"
        });
    }

    fn show_toast(&self, message: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(message));
    }

    fn persist_config(&mut self) {
        if let Err(error) = self.config_store.save(&self.config) {
            self.set_status(&format!("設定保存に失敗: {error}"), false);
        }
    }

    fn touch_transcription_activity(&mut self) {
        self.last_transcription_activity = Some(Instant::now());
        self.waiting_for_transcription_since = None;
        self.awaiting_transcription_after_local_speech = false;
    }

    fn touch_local_speech_activity(&mut self) {
        let now = Instant::now();
        self.last_local_speech_activity = Some(now);
        if !self.awaiting_transcription_after_local_speech {
            self.waiting_for_transcription_since = Some(now);
        }
        self.awaiting_transcription_after_local_speech = true;
    }

    fn should_auto_finish(&self) -> bool {
        should_auto_finish(
            self.last_transcription_activity,
            self.awaiting_transcription_after_local_speech,
            self.last_local_speech_activity,
            self.waiting_for_transcription_since,
        )
    }

    fn persist_transcription_history(&mut self) {
        let Some(context) = self.active_recording_history.take() else {
            return;
        };

        let transcript_after = buffer_text(&self.transcript_buffer);
        let appended_text = transcript_delta(&context.transcript_before, &transcript_after);
        if appended_text.trim().is_empty() {
            return;
        }

        match self.history_store.create_entry(
            HistoryKind::Transcription,
            appended_text,
            transcript_after,
            buffer_text(&self.english_buffer),
        ) {
            Ok(entry) => {
                self.latest_recording_history = Some(FinishedRecordingHistory {
                    entry_id: entry.id,
                    transcript_before: context.transcript_before,
                });
            }
            Err(error) => {
                self.set_status(&format!("履歴保存に失敗: {error}"), false);
            }
        }
    }

    fn update_latest_recording_history(&mut self) {
        let Some(context) = self.latest_recording_history.clone() else {
            return;
        };

        let mut entries = match self.history_store.load_all() {
            Ok(entries) => entries,
            Err(error) => {
                self.set_status(&format!("履歴更新に失敗: {error}"), false);
                return;
            }
        };

        let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.id == context.entry_id)
        else {
            return;
        };

        entry.source_text = transcript_delta(
            &context.transcript_before,
            &buffer_text(&self.transcript_buffer),
        );
        entry.japanese_text = buffer_text(&self.transcript_buffer);
        entry.english_text = buffer_text(&self.english_buffer);

        if let Err(error) = self.history_store.update(entry) {
            self.set_status(&format!("履歴更新に失敗: {error}"), false);
        }
    }

    fn persist_translation_history(&mut self, target: TranslationTarget, translated_text: &str) {
        let (kind, source_text, japanese_text, english_text) = match target {
            TranslationTarget::English => (
                HistoryKind::TranslateToEnglish,
                buffer_text(&self.transcript_buffer),
                buffer_text(&self.transcript_buffer),
                translated_text.to_string(),
            ),
            TranslationTarget::Japanese => (
                HistoryKind::TranslateToJapanese,
                buffer_text(&self.english_buffer),
                translated_text.to_string(),
                buffer_text(&self.english_buffer),
            ),
        };

        if let Err(error) =
            self.history_store
                .create_entry(kind, source_text, japanese_text, english_text)
        {
            self.set_status(&format!("履歴保存に失敗: {error}"), false);
        }
    }
}

struct TranslationActionsPane {
    container: gtk::Box,
    to_english: gtk::Button,
    to_japanese: gtk::Button,
}

fn editor_pane(title: &str, buffer: &gtk::TextBuffer, editable: bool) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.add_css_class("caption-heading");
    let view = gtk::TextView::with_buffer(buffer);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_editable(editable);
    view.set_monospace(false);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .hexpand(true)
        .vexpand(true)
        .build();
    container.append(&label);
    container.append(&scroller);
    container
}

fn readonly_editor_pane(title: &str, buffer: &gtk::TextBuffer) -> gtk::Box {
    editor_pane(title, buffer, false)
}

fn translation_actions_pane() -> TranslationActionsPane {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    container.set_valign(gtk::Align::Center);
    container.set_halign(gtk::Align::Center);
    container.set_margin_start(8);
    container.set_margin_end(8);

    let to_english = gtk::Button::with_label("英訳 ➡  Ctrl+Right");
    to_english.set_tooltip_text(Some("日本語を英訳して右ペインへ出力 (Ctrl+Right)"));
    let to_japanese = gtk::Button::with_label("⬅ 和訳  Ctrl+Left");
    to_japanese.set_tooltip_text(Some("英語を和訳して左ペインへ出力 (Ctrl+Left)"));

    container.append(&to_english);
    container.append(&to_japanese);

    TranslationActionsPane {
        container,
        to_english,
        to_japanese,
    }
}

fn labelled_icon_button(icon_name: &str, label: &str) -> (gtk::Button, gtk::Image, gtk::Label) {
    let button = gtk::Button::new();
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let icon = gtk::Image::from_icon_name(icon_name);
    let text = gtk::Label::new(Some(label));
    box_.append(&icon);
    box_.append(&text);
    button.set_child(Some(&box_));
    (button, icon, text)
}

fn preferences_editor(
    title: &str,
    initial_text: &str,
    min_height: i32,
) -> (gtk::Box, gtk::TextBuffer) {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.add_css_class("caption-heading");
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(initial_text);
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .min_content_height(min_height)
        .build();
    container.append(&label);
    container.append(&scroller);
    (container, buffer)
}

fn append_to_buffer(buffer: &gtk::TextBuffer, text: &str) {
    if text.trim().is_empty() {
        return;
    }

    let mut end = buffer.end_iter();
    let current = buffer_text(buffer);
    if !current.trim().is_empty() {
        buffer.insert(&mut end, "\n");
    }
    buffer.insert(&mut end, text.trim());
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string()
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "
        textview.live-preview text {
            color: alpha(currentColor, 0.55);
            font-style: italic;
        }

        .caption-heading {
            opacity: 0.72;
            font-weight: 700;
            letter-spacing: 0.04em;
        }

        scrolledwindow {
            border-radius: 12px;
        }
        ",
    );

    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn copy_to_clipboard(clipboard: &mut Option<arboard::Clipboard>, text: &str) -> Result<()> {
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().context("failed to create clipboard handle")?);
    }

    let clipboard = clipboard
        .as_mut()
        .context("failed to initialize persistent clipboard handle")?;
    clipboard
        .set_text(text.to_string())
        .context("failed to set clipboard text")
}

fn transcript_delta(before: &str, after: &str) -> String {
    let before = before.trim();
    let after = after.trim();

    if before.is_empty() {
        return after.to_string();
    }

    if let Some(rest) = after.strip_prefix(before) {
        return rest.trim().to_string();
    }

    after.to_string()
}

fn should_auto_finish(
    last_transcription_activity: Option<Instant>,
    awaiting_transcription_after_local_speech: bool,
    last_local_speech_activity: Option<Instant>,
    waiting_for_transcription_since: Option<Instant>,
) -> bool {
    if awaiting_transcription_after_local_speech {
        let local_speech_active = last_local_speech_activity
            .is_some_and(|instant| instant.elapsed() < LOCAL_SPEECH_ACTIVE_GRACE);
        if local_speech_active {
            return waiting_for_transcription_since.is_some_and(|instant| {
                instant.elapsed() >= AUTO_FINISH_WAITING_FOR_TRANSCRIPTION
            });
        }
    }

    last_transcription_activity
        .is_some_and(|instant| instant.elapsed() >= AUTO_FINISH_AFTER_TRANSCRIPTION)
}

fn history_entry_title(entry: &HistoryEntry) -> String {
    let base = match entry.kind {
        HistoryKind::Transcription => entry.source_text.as_str(),
        HistoryKind::TranslateToEnglish => entry.english_text.as_str(),
        HistoryKind::TranslateToJapanese => entry.japanese_text.as_str(),
    };
    summarize_text(base, 48)
}

fn history_entry_subtitle(entry: &HistoryEntry) -> String {
    format!(
        "{}  {}",
        format_timestamp(entry.created_at_ms),
        history_kind_label(entry.kind)
    )
}

fn history_kind_label(kind: HistoryKind) -> &'static str {
    match kind {
        HistoryKind::Transcription => "文字起こし",
        HistoryKind::TranslateToEnglish => "英訳",
        HistoryKind::TranslateToJapanese => "和訳",
    }
}

fn summarize_text(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut summary = String::new();
    for ch in normalized.chars() {
        if summary.chars().count() >= max_chars {
            summary.push('…');
            break;
        }
        summary.push(ch);
    }

    if summary.is_empty() {
        "内容なし".to_string()
    } else {
        summary
    }
}

fn format_timestamp(timestamp_ms: i64) -> String {
    let timestamp = timestamp_ms / 1_000;
    gtk::glib::DateTime::from_unix_local(timestamp)
        .ok()
        .and_then(|date_time| date_time.format("%Y-%m-%d %H:%M").ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| timestamp_ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_delta_returns_suffix_when_appending() {
        assert_eq!(
            transcript_delta("hello", "hello\nworld"),
            "world".to_string()
        );
    }

    #[test]
    fn transcript_delta_falls_back_to_full_text_when_prefix_differs() {
        assert_eq!(transcript_delta("hello", "world"), "world".to_string());
    }

    #[test]
    fn auto_finish_waits_for_transcription_after_local_speech() {
        assert!(!should_auto_finish(
            Some(Instant::now() - AUTO_FINISH_AFTER_TRANSCRIPTION - Duration::from_secs(1)),
            true,
            Some(Instant::now()),
            Some(Instant::now() - Duration::from_secs(5)),
        ));
    }

    #[test]
    fn auto_finish_falls_back_after_long_wait_for_missing_transcription() {
        assert!(should_auto_finish(
            Some(Instant::now()),
            true,
            Some(Instant::now()),
            Some(Instant::now() - AUTO_FINISH_WAITING_FOR_TRANSCRIPTION - Duration::from_secs(1)),
        ));
    }

    #[test]
    fn auto_finish_falls_back_to_ten_second_timer_after_local_speech_stops() {
        assert!(should_auto_finish(
            Some(Instant::now() - AUTO_FINISH_AFTER_TRANSCRIPTION - Duration::from_secs(1)),
            true,
            Some(Instant::now() - LOCAL_SPEECH_ACTIVE_GRACE - Duration::from_millis(1)),
            Some(Instant::now() - Duration::from_secs(5)),
        ));
    }

    #[test]
    fn auto_finish_after_ten_seconds_without_pending_local_speech() {
        assert!(should_auto_finish(
            Some(Instant::now() - AUTO_FINISH_AFTER_TRANSCRIPTION - Duration::from_secs(1)),
            false,
            None,
            None,
        ));
    }
}
