# SPEC.md

  ## Summary

  - Rust + GTK4/libadwaita で Ubuntu Wayland 専用の常駐型文字起こしアプリを新規実装する。
  - OpenAI Realtime API を WebSocket で使い、日本語音声を streaming で文字起こしする。
  - 文字起こし結果は UI 上で編集可能にし、固有名詞リストを prompt に反映できるようにする。
  - 翻訳は gpt-4.1-nano を使って英訳し、原文とは別表示にする。
  - 起動制御は systemd --user + CLI + Ubuntu のシステムショートカットで行う。

  ## Runtime / Startup

  - バイナリは simple-openai-transcribe の単一 CLI とし、daemon show start finish quit の subcommand を持つ。
  - daemon は GTK アプリ本体を起動し、常駐して IPC を待ち受ける。
  - show はウィンドウ表示、start はウィンドウ表示 + 録音開始、finish は録音停止 + flush + clipboard copy、quit はアプリ終了を行う。
  - 常駐制御は $XDG_RUNTIME_DIR/simple-openai-transcribe.sock の Unix socket IPC で行う。
  - CLI 実行時にデーモン未起動なら systemctl --user start simple-openai-transcribe.service を試し、その後 IPC を再送する。
  - systemd --user unit を提供し、Ubuntu のシステムショートカットから各 CLI を呼ぶ運用を前提にする。

  ## Environment / Config

  - 起動時にプロジェクトルートの .env を読み込み、OPENAI_API_KEY を process environment に注入する。
  - .env 読み込みは dotenvy などの軽量ライブラリで、daemon と全 CLI entrypoint の最初で実行する。
  - .env 読み込み後も OPENAI_API_KEY が未設定なら、UI と CLI の両方で明確なエラーを出して処理を中断する。
  - API key 自体は設定ファイルへ保存しない。
  - 永続設定は ~/.config/simple-openai-transcribe/config.toml に保存し、対象は固有名詞リスト、VAD パラメータ、UI 状態のみとする。

  ## UI

  - 1 ウィンドウ構成にし、最低限以下を表示する。
  - 開始 ボタン
  - 文字起こし完了 ボタン
  - 英訳 ボタン
  - 閉じる ボタン
  - 編集可能な日本語文字起こし欄
  - 編集可能な固有名詞リスト欄
  - 英訳結果表示欄
  - 文字起こし本文は編集可能な TextView を source of truth にする。
  - Realtime の delta は本文へ直接混ぜず、進行中プレビューとして別管理する。
  - completed を受けた turn だけ本文末尾へ確定追記する。
  - 閉じる ボタンとウィンドウ close は同じ挙動にし、デーモン終了まで行う。

  ## Realtime Transcription

  - OpenAI Realtime transcription session を使う。
  - セッション設定は以下に固定する。
  - type=transcription
  - audio.input.format=audio/pcm
  - audio.input.transcription.model=gpt-4o-transcribe-latest
  - audio.input.transcription.language=ja
  - audio.input.noise_reduction={type:"near_field"}
  - audio.input.turn_detection.type=server_vad
  - マイク入力は 24kHz mono PCM に変換して input_audio_buffer.append で送る。
  - 進行中文字起こしは conversation.item.input_audio_transcription.delta を使って表示する。
  - 確定文字起こしは conversation.item.input_audio_transcription.completed を使って反映する。
  - turn の順序は input_audio_buffer.committed.item_id と previous_item_id を使って安定化する。
  - 固有名詞リストは毎セッション開始時に transcription prompt に埋め込み、「以下の語を優先して正確に文字起こしする」形式の短いテンプレートで送る。
  - prompt 生成時は空行除去と重複除去を行う。

  ## Finish / Clipboard / Translation

  - 文字起こし完了 は録音停止後に最終 Realtime event を短時間待ち、確定済み本文を clipboard にコピーする。
  - completed が未着の末尾 turn は、必要なら最新 delta を暫定追記してからコピーする。
  - 英訳 は現在の編集済み日本語本文を /v1/responses に送り、gpt-4.1-nano で自然な英語へ翻訳する。
  - 英訳結果は別欄に表示し、日本語本文は変更しない。
  - 翻訳 prompt は簡潔固定とし、「意味を変えず自然な英語に翻訳する」を基本方針にする。

  ## Public Interfaces

  - CLI:
  - simple-openai-transcribe daemon
  - simple-openai-transcribe show
  - simple-openai-transcribe start
  - simple-openai-transcribe finish
  - simple-openai-transcribe quit
  - IPC message:
  - Show
  - Start
  - Finish
  - Quit
  - GetState
  - Config fields:
  - terms: string[]
  - vad_threshold: f32
  - vad_silence_ms: u32
  - copy_on_finish: bool
  - auto_show_on_start: bool

  ## Dependencies

  - Rust crates は少なくとも以下を使う。
  - gtk4
  - libadwaita
  - tokio
  - tokio-tungstenite
  - reqwest
  - serde
  - clap
  - cpal
  - rubato
  - arboard
  - directories
  - tracing
  - .env 読み込み用に dotenvy

  ## Test Plan

  - .env 読み込み後に OPENAI_API_KEY が参照できる。
  - .env が存在しても key が空なら fail fast する。
  - prompt builder が空行除去・重複除去・日本語テンプレート生成を正しく行う。
  - turn assembler が previous_item_id ベースで completed 順不同を整列できる。
  - delta は本文に直接混ざらず、completed のみが確定追記される。
  - finish で録音停止、最終 flush、clipboard copy、state 遷移が正しく行われる。
  - 英訳 が編集済み本文を送信し、原文を変更しない。
  - デーモン未起動時の start finish quit が auto-start を含めて期待通り動く。
  - Ubuntu システムショートカットから CLI を叩いて開始・完了・終了できる。

  ## Assumptions

  - .env は開発・個人利用専用のローカル運用とし、リポジトリには commit しない。
  - 2026-04-15 時点の OpenAI docs に基づき、Realtime transcription は type=transcription と conversation.item.input_audio_transcription.delta/completed を使う。
  - gpt4-nano は gpt-4.1-nano を意味するものとして固定する。
  - Ubuntu Wayland 専用のため、X11・Windows・macOS・アプリ内グローバルホットキーは対象外にする。
