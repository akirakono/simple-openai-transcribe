use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib::{self, ControlFlow};
use tokio::runtime::Runtime;

use crate::config::{AppConfig, ConfigStore, parse_terms};
use crate::env;
use crate::ipc;
use crate::openai::{self, SessionCommand, SessionOptions, UiEvent};

pub fn run_daemon() -> Result<()> {
    let store = ConfigStore::new()?;
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

    // `clap` already consumed our subcommands. Passing the original argv into
    // GApplication makes `daemon` look like a file path to open.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

struct Ui {
    _state: Rc<RefCell<UiState>>,
}

struct UiState {
    app: adw::Application,
    window: adw::ApplicationWindow,
    transcript_buffer: gtk::TextBuffer,
    preview_buffer: gtk::TextBuffer,
    terms_buffer: gtk::TextBuffer,
    translation_buffer: gtk::TextBuffer,
    status_label: gtk::Label,
    clipboard: Option<arboard::Clipboard>,
    runtime: Arc<Runtime>,
    config_store: ConfigStore,
    config: AppConfig,
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
    api_key: String,
    active_session: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    correction_generation: u64,
    correction_in_flight: bool,
    pending_correction_text: String,
    pending_correction_snapshot: String,
}

impl Ui {
    #[allow(clippy::too_many_arguments)]
    fn build(
        app: &adw::Application,
        config_store: ConfigStore,
        config: AppConfig,
        api_key: String,
        runtime: Arc<Runtime>,
        ipc_rx: Receiver<ipc::Command>,
        ui_rx: Receiver<UiEvent>,
        ui_tx: std::sync::mpsc::Sender<UiEvent>,
    ) -> Result<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Simple OpenAI Transcribe")
            .default_width(config.window_width)
            .default_height(config.window_height)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let start_button = gtk::Button::with_label("開始");
        let finish_button = gtk::Button::with_label("文字起こし完了");
        let translate_button = gtk::Button::with_label("英訳");
        let close_button = gtk::Button::with_label("閉じる");
        toolbar.append(&start_button);
        toolbar.append(&finish_button);
        toolbar.append(&translate_button);
        toolbar.append(&close_button);

        let status_label = gtk::Label::new(Some("待機中"));
        status_label.set_xalign(0.0);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_wide_handle(true);

        let left_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let transcript_label = gtk::Label::new(Some("文字起こし"));
        transcript_label.set_xalign(0.0);
        let transcript_buffer = gtk::TextBuffer::new(None);
        let transcript_view = gtk::TextView::with_buffer(&transcript_buffer);
        transcript_view.set_wrap_mode(gtk::WrapMode::WordChar);
        transcript_view.set_vexpand(true);
        let transcript_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&transcript_view)
            .build();

        let preview_label = gtk::Label::new(Some("進行中"));
        preview_label.set_xalign(0.0);
        let preview_buffer = gtk::TextBuffer::new(None);
        let preview_view = gtk::TextView::with_buffer(&preview_buffer);
        preview_view.set_wrap_mode(gtk::WrapMode::WordChar);
        preview_view.set_editable(false);
        preview_view.set_cursor_visible(false);
        preview_view.set_can_focus(false);
        preview_view.add_css_class("preview-transcript");
        let preview_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .min_content_height(120)
            .child(&preview_view)
            .build();

        left_box.append(&transcript_label);
        left_box.append(&transcript_scroller);
        left_box.append(&preview_label);
        left_box.append(&preview_scroller);

        let right_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let terms_label = gtk::Label::new(Some("固有名詞 / 用語"));
        terms_label.set_xalign(0.0);
        let terms_buffer = gtk::TextBuffer::new(None);
        terms_buffer.set_text(&config.terms.join("\n"));
        let terms_view = gtk::TextView::with_buffer(&terms_buffer);
        terms_view.set_wrap_mode(gtk::WrapMode::WordChar);
        terms_view.set_vexpand(true);
        let terms_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .min_content_width(280)
            .child(&terms_view)
            .build();

        let translation_label = gtk::Label::new(Some("英訳"));
        translation_label.set_xalign(0.0);
        let translation_buffer = gtk::TextBuffer::new(None);
        let translation_view = gtk::TextView::with_buffer(&translation_buffer);
        translation_view.set_wrap_mode(gtk::WrapMode::WordChar);
        translation_view.set_editable(false);
        translation_view.set_vexpand(true);
        let translation_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&translation_view)
            .build();

        right_box.append(&terms_label);
        right_box.append(&terms_scroller);
        right_box.append(&translation_label);
        right_box.append(&translation_scroller);

        paned.set_start_child(Some(&left_box));
        paned.set_end_child(Some(&right_box));

        content.append(&toolbar);
        content.append(&status_label);
        content.append(&paned);
        window.set_content(Some(&content));
        install_css();

        let state = Rc::new(RefCell::new(UiState {
            app: app.clone(),
            window: window.clone(),
            transcript_buffer: transcript_buffer.clone(),
            preview_buffer: preview_buffer.clone(),
            terms_buffer: terms_buffer.clone(),
            translation_buffer: translation_buffer.clone(),
            status_label: status_label.clone(),
            clipboard: arboard::Clipboard::new().ok(),
            runtime,
            config_store,
            config,
            ui_tx,
            api_key,
            active_session: None,
            correction_generation: 0,
            correction_in_flight: false,
            pending_correction_text: String::new(),
            pending_correction_snapshot: String::new(),
        }));

        {
            let state = Rc::clone(&state);
            start_button.connect_clicked(move |_| {
                Ui::start_transcription(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            finish_button.connect_clicked(move |_| {
                Ui::finish_transcription(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            translate_button.connect_clicked(move |_| {
                Ui::translate(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            close_button.connect_clicked(move |_| {
                Ui::quit(&state);
            });
        }

        {
            let state = Rc::clone(&state);
            terms_buffer.connect_changed(move |buffer| {
                let terms = buffer_text(buffer);
                let parsed = parse_terms(&terms);
                let mut state = state.borrow_mut();
                state.config.terms = parsed;
                if let Err(error) = state.config_store.save(&state.config) {
                    state.set_status(format!("設定保存に失敗: {error}"));
                }
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
            glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
                while let Ok(event) = ui_rx.try_recv() {
                    match event {
                        UiEvent::Status(message) => {
                            state.borrow().status_label.set_text(&message);
                        }
                        UiEvent::Preview(text) => {
                            if !text.is_empty() {
                                state.borrow_mut().correction_generation += 1;
                            }
                            state.borrow().preview_buffer.set_text(&text);
                        }
                        UiEvent::AppendCommitted(text) => {
                            let buffer = state.borrow().transcript_buffer.clone();
                            append_to_buffer(&buffer, &text);
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
                        UiEvent::TranslationReady(text) => {
                            let state_ref = state.borrow();
                            state_ref.translation_buffer.set_text(&text);
                            state_ref.status_label.set_text("英訳が完了しました");
                        }
                        UiEvent::TranslationStarted => {
                            state.borrow().status_label.set_text("英訳中");
                        }
                        UiEvent::RecordingStarted(session_tx) => {
                            state.borrow_mut().active_session = Some(session_tx);
                        }
                        UiEvent::RecordingFinished => {
                            let mut state = state.borrow_mut();
                            state.active_session = None;
                            if state.config.copy_on_finish {
                                let text = buffer_text(&state.transcript_buffer);
                                if !text.trim().is_empty() {
                                    if let Err(error) =
                                        copy_to_clipboard(&mut state.clipboard, &text)
                                    {
                                        state.set_status(format!(
                                            "clipboard へのコピーに失敗: {error}"
                                        ));
                                    } else {
                                        state.set_status("clipboard にコピーしました");
                                    }
                                }
                            }
                        }
                        UiEvent::Error(error) => {
                            tracing::error!("ui error: {error}");
                            state.borrow().status_label.set_text(&error);
                        }
                    }
                }
                ControlFlow::Continue
            });
        }

        window.hide();

        Ok(Self { _state: state })
    }

    fn show_window(state: &Rc<RefCell<UiState>>) {
        state.borrow().window.present();
    }

    fn start_transcription(state: &Rc<RefCell<UiState>>) {
        let mut state_mut = state.borrow_mut();
        if state_mut.active_session.is_some() {
            state_mut.set_status("すでに録音中です");
            return;
        }

        let terms = parse_terms(&buffer_text(&state_mut.terms_buffer));
        state_mut.config.terms = terms.clone();
        if let Err(error) = state_mut.config_store.save(&state_mut.config) {
            state_mut.set_status(format!("設定保存に失敗: {error}"));
        }

        let options = SessionOptions {
            api_key: state_mut.api_key.clone(),
            terms,
            vad_threshold: state_mut.config.vad_threshold,
            vad_silence_ms: state_mut.config.vad_silence_ms,
        };

        let ui_tx = state_mut.ui_tx.clone();
        let runtime = Arc::clone(&state_mut.runtime);
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
        let session = state.borrow().active_session.clone();
        if let Some(session) = session {
            let _ = session.send(SessionCommand::Finish);
        } else {
            let transcript = buffer_text(&state.borrow().transcript_buffer);
            if !transcript.trim().is_empty() {
                let mut state = state.borrow_mut();
                if let Err(error) = copy_to_clipboard(&mut state.clipboard, &transcript) {
                    state
                        .status_label
                        .set_text(&format!("clipboard へのコピーに失敗: {error}"));
                } else {
                    state.status_label.set_text("clipboard にコピーしました");
                }
            }
        }
    }

    fn translate(state: &Rc<RefCell<UiState>>) {
        let transcript = buffer_text(&state.borrow().transcript_buffer);
        if transcript.trim().is_empty() {
            state
                .borrow()
                .status_label
                .set_text("翻訳するテキストがありません");
            return;
        }

        let api_key = state.borrow().api_key.clone();
        let ui_tx = state.borrow().ui_tx.clone();
        let runtime = Arc::clone(&state.borrow().runtime);
        let _ = ui_tx.send(UiEvent::TranslationStarted);

        runtime.spawn(async move {
            match openai::translate_text(&api_key, &transcript).await {
                Ok(result) => {
                    let _ = ui_tx.send(UiEvent::TranslationReady(result));
                }
                Err(error) => {
                    tracing::error!("translation request failed: {error:#}");
                    let _ = ui_tx.send(UiEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn quit(state: &Rc<RefCell<UiState>>) {
        {
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
                    let _ = ui_tx.send(UiEvent::CorrectionReady(openai::CorrectionResult {
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

    fn apply_correction(state: &Rc<RefCell<UiState>>, result: openai::CorrectionResult) {
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

        let updated = format!("{prefix}{corrected}");
        state.transcript_buffer.set_text(&updated);
        state.pending_correction_text.clear();
        state.pending_correction_snapshot.clear();
    }
}

impl UiState {
    fn set_status(&mut self, status: impl AsRef<str>) {
        self.status_label.set_text(status.as_ref());
    }
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
        textview.preview-transcript text {
            color: alpha(currentColor, 0.55);
            font-style: italic;
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
