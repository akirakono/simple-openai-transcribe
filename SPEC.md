# SPEC

## 概要

`simple-openai-transcribe` は Ubuntu Wayland 向けの個人用 GTK4/libadwaita ネイティブアプリケーションである。OpenAI Realtime API による streaming 日本語文字起こし、確定後補正、双方向翻訳、clipboard 連携、`systemd --user` 常駐運用を提供する。

## 対象環境

- OS: Ubuntu Wayland
- GUI: GTK4 + libadwaita
- 実行形態: user session 上の GUI アプリ
- 音声入力: PipeWire `pw-record`
- API: OpenAI Realtime API, Responses API

X11、Windows、macOS、アプリ側のグローバルショートカット実装は対象外とする。

## 実装言語と主要依存

- Rust
- gtk4-rs
- libadwaita
- tokio
- tokio-tungstenite
- reqwest
- serde / serde_json
- clap
- arboard
- directories
- dotenvy
- tracing

## 実行モデル

バイナリは単一 CLI とし、以下の subcommand を持つ。

- `daemon`
- `show`
- `start`
- `finish`
- `quit`

`daemon` は GTK アプリ本体を起動し、Unix socket IPC を待ち受ける。  
`show/start/finish/quit` は daemon に命令を送る。

IPC socket:

- `$XDG_RUNTIME_DIR/simple-openai-transcribe.sock`

CLI 実行時の daemon 自動起動順序:

1. 既存 daemon に IPC 接続を試す
2. 失敗時、`systemctl --user start simple-openai-transcribe.service` を試す
3. さらに失敗した場合、現在のバイナリ自身で `daemon` を background spawn する

## 環境変数と設定

### API key

- 起動時に repository root の `.env` を読み込む
- `OPENAI_API_KEY` が未設定なら fail fast する
- API key は config file に保存しない

### 永続設定

保存先:

- `~/.config/simple-openai-transcribe/config.toml`

保存項目:

- `terms: string[]`
- `vad_threshold: f32`
- `vad_silence_ms: u32`
- `copy_on_finish: bool`
- `auto_show_on_start: bool`
- `transcription_system_prompt: string`
- `translate_to_english_prompt: string`
- `translate_to_japanese_prompt: string`
- `window_width: i32`
- `window_height: i32`

## UI 仕様

### ウィンドウ構成

単一ウィンドウ構成。内容は以下。

1. HeaderBar
2. Live Preview strip
3. Main editor area

### HeaderBar

左:

- 録音トグルボタン
  - 非録音時: `録音 (F1 / Ctrl+R)`
  - 録音時: `停止 (F1 / Ctrl+R)`
- 履歴ボタン
  - `履歴 (Ctrl+H)`

中央:

- ステータスラベル
- GtkSpinner

右:

- 設定ボタン

### Live Preview

HeaderBar 直下の細いエリアで、確定前 transcript を表示する。

- 非編集
- 薄い色
- 斜体
- source は Realtime の delta / 未確定 transcript

### Main Content

`GtkPaned` による左右分割。

左ペイン:

- 日本語 transcript
- 編集可能 `GtkTextView`

中央アクション:

- `英訳 ➡ Ctrl+Right`
- `⬅ 和訳 Ctrl+Left`

右ペイン:

- 英語 translation
- 編集可能 `GtkTextView`

### Toast

`AdwToastOverlay` を用い、以下の完了通知を表示する。

- 文字起こしコピー完了
- 英訳コピー完了
- 和訳コピー完了

### Preferences

`AdwPreferencesWindow` で以下を編集可能にする。

- 文字起こし用 prompt
- 英訳用 prompt
- 和訳用 prompt
- 固有名詞辞書

### 履歴画面

別ウィンドウの履歴ブラウザを持つ。

- 左カラム: 保存済み履歴一覧
- 右カラム: 選択した履歴の詳細
- 詳細表示項目:
  - ソース
  - 日本語
  - 英語
- `Escape` で履歴ウィンドウを閉じる

## ショートカット

アプリ内ショートカット:

- `F1`
- `Ctrl+R`
  - 録音開始 / 停止
- `Ctrl+Right`
  - 日本語を英訳
- `Ctrl+Left`
  - 英語を和訳
- `Ctrl+,`
  - 設定を開く
- `Ctrl+H`
  - 履歴を開く
- `Escape`
  - アプリ終了

システムレベルのグローバルショートカットはアプリではなく Ubuntu 側のキーボードショートカット設定で登録する前提とする。

## 文字起こし仕様

### Realtime session

OpenAI Realtime transcription session を利用する。

使用イベント:

- `conversation.item.input_audio_transcription.delta`
- `conversation.item.input_audio_transcription.completed`
- `input_audio_buffer.committed`

セッション更新は transcription session schema を使う。

設定内容:

- input audio format: `pcm16`
- model: `gpt-4o-transcribe`
- language: `ja`
- noise reduction: `near_field`
- turn detection: `server_vad`
- `include: ["item.input_audio_transcription.logprobs"]`

### 音声入力

- `pw-record` を使って 24kHz / mono / s16 の PCM を取得
- 取得した chunk を base64 化して `input_audio_buffer.append` で送信

### transcript 反映

- delta は Live Preview にのみ表示
- completed は turn 単位で確定
- `input_audio_buffer.committed.item_id` と `previous_item_id` を使って順序を安定化
- 確定済み transcript は日本語ペインへ append

### 録音停止

- `finish` は音声 capture を停止する
- `server_vad` に最終 commit を任せるため、手動 `input_audio_buffer.commit` は送らない
- `vad_silence_ms + 400ms` 以上待って final transcript を受け取る
- 未確定 tail が残った場合は provisional transcript を日本語ペインへ落とす

自動終了条件:

- メインウィンドウが非アクティブになったとき
- 英訳または和訳を開始したとき
- 設定画面または履歴画面を開いたとき
- preview / completed を含む文字起こしイベントが 10 秒以上来ないとき

重複 finish を避けるため、録音セッションごとに finish 要求は 1 回だけ送る。

## transcript 自動補正

日本語ペインへ append された確定済み transcript に対し、post-processing を行う。

- モデル: `gpt-5.4-nano`
- API: Responses API
- 対象: 直近に追加された未補正 tail のみ
- trigger: completed append 後 2 秒 debounce
- 新しい preview / completed が来た場合は debounce を更新
- ユーザーが本文を編集していた場合は補正結果を破棄
- 固有名詞辞書は correction prompt に含める

補正方針:

- spelling discrepancy の修正
- punctuation / spacing / capitalization の最小限修正
- 意味変更、要約、情報追加は禁止

## 翻訳仕様

翻訳 API は Responses API を使用する。

モデル:

- `gpt-4.1-nano`

フロー:

- 日本語ペイン → 英訳 → 英語ペイン
- 英語ペイン → 和訳 → 日本語ペイン

翻訳 prompt は設定画面から編集可能で、永続化される。

翻訳完了後:

- 結果を対象ペインへ反映
- 結果を clipboard にコピー
- Toast を表示

## clipboard

`arboard::Clipboard` をプロセス内で保持する。

用途:

- 文字起こし完了時の自動コピー
- 英訳完了時の自動コピー
- 和訳完了時の自動コピー

## 履歴保存

保存先:

- `~/.local/share/simple-openai-transcribe/history/`

形式:

- 1 エントリ 1 JSON file

履歴の単位:

- 録音 1 セッションごとに 1 件
- 日本語→英語の翻訳 1 実行ごとに 1 件
- 英語→日本語の翻訳 1 実行ごとに 1 件

録音履歴の保存内容:

- その録音セッションで追記されたテキスト
- 録音完了時点の日本語全文
- 録音完了時点の英語欄スナップショット

自動補正が後から適用された場合は、直近の録音履歴を更新して補正後の内容を保持する。

## systemd --user 仕様

repository には絶対パス固定の unit を置かず、template を保持する。

template:

- `systemd/simple-openai-transcribe.service.in`

install script:

- `scripts/install-systemd-user-service.sh`

script の役割:

- 現在の repository path を解決
- `target/debug` または `target/release` の binary path を埋め込む
- `~/.config/systemd/user/simple-openai-transcribe.service` を生成
- `systemctl --user daemon-reload`
- `systemctl --user enable --now simple-openai-transcribe.service`

## エラーハンドリングとログ

`tracing` を用いて daemon 側で system error を出力する。

保存先:

- `~/.local/state/simple-openai-transcribe/simple-openai-transcribe.log`

対象例:

- Realtime API error
- WebSocket read error
- IPC 転送失敗
- translation request failure
- transcript correction failure

systemd 運用時のログ確認:

```bash
journalctl --user -u simple-openai-transcribe.service -f
```

## テスト観点

- `.env` 読み込み
- terms の正規化
- transcription prompt 生成
- correction prompt 生成
- VAD threshold の JSON 表現
- Realtime event type 抽出
- `previous_item_id` に基づく transcript 順序安定化

## 現在の制約

- 音声入力は `pw-record` 前提
- Linux desktop session 前提
- グローバルショートカット取得はアプリ内では行わない
- Realtime transcription は server VAD 前提のため、手動 turn commit モードは未実装
