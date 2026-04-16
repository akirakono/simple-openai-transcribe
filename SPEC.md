# SPEC

## 1. 目的

`simple-openai-transcribe` は Ubuntu Wayland 上で動作する個人用デスクトップ文字起こしアプリケーションである。主目的は、手元のマイク入力を OpenAI Realtime API に流し、日本語 transcript を低遅延で確認しながら必要に応じて英訳・和訳できるようにすることである。

この文書は、他の実装者が同等のアプリケーションを再実装できることを目的とする。実装技術は参考情報であり、外部から観測できる振る舞い、永続化形式、外部インターフェース、状態遷移を優先する。

## 2. スコープ

### 2.1 必須機能

- 日本語音声の streaming 文字起こし
- 確定前テキストのライブプレビュー
- 確定後テキストの日本語ペインへの反映
- 確定後テキストの軽い自動補正
- 日本語から英語への翻訳
- 英語から日本語への翻訳
- clipboard への自動コピー
- 履歴保存
- CLI から GUI daemon を操作する IPC
- `systemd --user` と相性の良い常駐運用

### 2.2 非目標

- Windows, macOS, X11 対応
- アプリ自身によるグローバルショートカット登録
- 複数ユーザー共有やネットワーク越しの利用
- 手動 turn commit を使う transcription モード
- 汎用的なノートアプリや文書管理機能

## 3. 対象環境

- OS: Ubuntu Wayland
- 実行形態: ユーザーセッション内 GUI アプリ
- 音声入力: PipeWire の `pw-record` が利用可能であること
- OpenAI API へ到達可能であること

参考実装は Rust, GTK4, libadwaita, tokio, tokio-tungstenite, reqwest, serde, clap, arboard, directories, dotenvy, tracing を用いるが、同等の外部仕様を満たす限り代替実装でもよい。

## 4. 用語

- daemon: GUI アプリ本体。IPC socket を待ち受ける長寿命プロセス
- transcript: 日本語文字起こし結果
- preview: まだ確定していない Realtime transcript
- committed turn: VAD により区切られ、最終的に確定した発話単位
- finish: 録音停止要求。capture を止めたうえで最終 transcript を待つ
- stop: 即時停止。最終 transcript 待ちをせず終了する

## 5. 高位アーキテクチャ

アプリケーションは以下の 5 要素からなる。

1. CLI front-end
2. GUI daemon
3. OpenAI Realtime transcription client
4. OpenAI Responses API client
5. 永続化層

CLI は daemon を操作するだけであり、文字起こしや翻訳の実処理は daemon が担う。

## 6. プロセスモデルと起動方式

### 6.1 CLI

単一バイナリ `simple-openai-transcribe` は以下の subcommand を持つ。

- `daemon`
- `show`
- `start`
- `finish`
- `quit`

subcommand が省略された場合、`daemon` と同義とする。

### 6.2 `daemon`

- GTK アプリケーションを起動する
- Unix domain socket で IPC command を受け付ける
- 起動直後のメインウィンドウは hidden とする
- `show` または `start` を受けたときにメインウィンドウを前面表示する

### 6.3 daemon 自動起動

`show`, `start`, `finish`, `quit` 実行時、CLI は以下の順で daemon への到達を試みる。

1. 既存 socket に IPC 接続
2. 失敗時、`systemctl --user start simple-openai-transcribe.service`
3. なお失敗した場合、現在のバイナリ自身を `daemon` として background spawn
4. その後、150ms 間隔で最大 20 回 IPC 再試行

再試行後も到達不能ならコマンドは失敗として終了する。

## 7. IPC 仕様

### 7.1 socket

- パス: `$XDG_RUNTIME_DIR/simple-openai-transcribe.sock`
- transport: Unix domain socket
- request/response は 1 行単位

### 7.2 request 形式

JSON を 1 行で送る。

```json
{"command":"Show"}
```

`command` は以下の PascalCase enum のいずれかとする。

- `Show`
- `Start`
- `Finish`
- `Quit`
- `GetState`

`GetState` は予約済み command とし、現バージョンでは UI 状態取得には使わない。

### 7.3 response 形式

daemon は改行付きのプレーンテキストで応答する。

- 正常: `ok\n`
- request 形式不正: `invalid\n`
- daemon 内部で command を UI thread に渡せない: `error\n`

## 8. 環境変数と設定

### 8.1 `.env` 読み込み

起動時に `OPENAI_API_KEY` を解決する。検索順は以下とする。

1. current working directory の `.env`
2. 実行ファイルの親ディレクトリ群にある `.env`
3. repository root と判定できる祖先ディレクトリにある `.env`
4. ビルド時 manifest directory の `.env`

`.env` が見つからなくても起動処理自体は継続してよいが、最終的に `OPENAI_API_KEY` が空なら fail fast する。

### 8.2 永続設定ファイル

- パス: `~/.config/simple-openai-transcribe/config.toml`
- フォーマット: TOML
- 不存在時はデフォルト値を採用する

必須キーとデフォルト値:

| key | type | default | meaning |
| --- | --- | --- | --- |
| `terms` | `string[]` | `[]` | 固有名詞辞書 |
| `vad_threshold` | `f32` | `0.45` | Realtime server VAD threshold |
| `vad_silence_ms` | `u32` | `900` | VAD の無音判定ミリ秒 |
| `copy_on_finish` | `bool` | `true` | 録音完了時に日本語全文を clipboard にコピーする |
| `auto_show_on_start` | `bool` | `true` | 将来拡張用の予約値。現行仕様では必須利用しない |
| `transcription_system_prompt` | `string` | `日本語の音声を自然な表記で正確に文字起こししてください。` | transcription 用 prompt |
| `translate_to_english_prompt` | `string` | `Translate the user's Japanese text into natural English. Preserve meaning. Output only the translation.` | 日本語→英語 prompt |
| `translate_to_japanese_prompt` | `string` | `Translate the user's English text into natural Japanese. Preserve meaning. Output only the translation.` | 英語→日本語 prompt |
| `window_width` | `i32` | `1100` | 前回または初期表示時の幅 |
| `window_height` | `i32` | `760` | 前回または初期表示時の高さ |

API key は設定ファイルに保存してはならない。

## 9. UI 仕様

### 9.1 メインウィンドウ

単一ウィンドウ構成で、以下の 3 ブロックを持つ。

1. HeaderBar
2. Live Preview
3. 編集ペイン領域

### 9.2 HeaderBar

左側:

- 録音トグルボタン
  - 待機時ラベル: `録音 (F1 / Ctrl+R)`
  - 録音中ラベル: `停止 (F1 / Ctrl+R)`
- 履歴ボタン
  - ラベル: `履歴 (Ctrl+H)`

中央:

- ステータスラベル
- busy 中だけ回る spinner

右側:

- 設定ボタン
  - ラベル: `設定 (Ctrl+,)`

### 9.3 Live Preview

- HeaderBar 直下に配置する
- 非編集
- 確定前 transcript のみを表示する
- `delta` と未確定 tail の現在値を反映する
- transcript が確定したら、その時点の preview を更新または空にする

見た目は薄色かつ補助的に見えることが望ましいが、配色自体は実装依存としてよい。

### 9.4 編集ペイン

横並び 3 領域とする。

- 左: 日本語ペイン
  - 編集可能
  - 文字起こし結果の主表示先
- 中央: 翻訳アクション
  - `英訳 ➡ Ctrl+Right`
  - `⬅ 和訳 Ctrl+Left`
- 右: 英語ペイン
  - 編集可能
  - 翻訳結果の表示先

翻訳ボタンは各対象ペインの全文を置換する。

### 9.5 ステータス

最低限、次の状態を視覚的に区別できること。

- `待機中`
- `録音中`
- `最終結果を待っています`
- `英訳中`
- `和訳中`
- エラーメッセージ

`録音中`, `最終結果を待っています`, `英訳中`, `和訳中` では spinner を回す。

### 9.6 Preferences

別ウィンドウまたは設定ダイアログで、以下を編集可能にする。

- transcription prompt
- 日本語→英語 prompt
- 英語→日本語 prompt
- 固有名詞辞書

固有名詞辞書は 1 行 1 語で編集し、保存時に以下の正規化を行う。

- 前後空白除去
- 空行除去
- 重複除去

設定変更は即時永続化してよい。

### 9.7 履歴画面

別ウィンドウの modal browser とする。

- 左カラム: 履歴一覧
- 右カラム: 詳細
- 詳細項目:
  - ソース
  - 日本語
  - 英語
- `Escape` で履歴画面を閉じる

一覧は新しい順に表示する。

## 10. キーボードショートカット

アプリ内ショートカット:

- `F1`: 録音開始または finish
- `Ctrl+R`: 録音開始または finish
- `Ctrl+Right`: 日本語ペインを英訳
- `Ctrl+Left`: 英語ペインを和訳
- `Ctrl+,`: 設定を開く
- `Ctrl+H`: 履歴を開く
- `Escape`: アプリ終了

グローバルショートカットは Ubuntu 側のキーボードショートカット機能で `show`, `start`, `finish`, `quit` をバインドする前提とする。

## 11. 状態遷移

主要状態は以下。

- `Idle`
- `Recording`
- `Finishing`
- `TranslatingToEnglish`
- `TranslatingToJapanese`

状態遷移:

- `Idle` -> `Recording`
  - 録音トグル
  - `Start` IPC
- `Recording` -> `Finishing`
  - 録音トグル
  - `Finish` IPC
  - window 非アクティブ化
  - 翻訳開始
  - 設定画面表示
  - 履歴画面表示
  - 無音タイムアウト
- `Finishing` -> `Idle`
  - final transcript 待ち完了
- `Recording` -> `Idle`
  - `Stop` 相当の内部終了またはエラー
- `Idle` -> `TranslatingToEnglish`
  - 英訳要求
- `Idle` -> `TranslatingToJapanese`
  - 和訳要求
- 翻訳完了後は `Idle`
- `Quit` はどの状態からでも終了可能

`Escape` またはウィンドウ close request は、録音中なら即時 `Stop`、そうでなければ設定保存後終了とする。

## 12. 文字起こし仕様

### 12.1 Realtime 接続

- endpoint: OpenAI Realtime API transcription intent
- transport: WebSocket
- session update type: `transcription_session.update`

セッション設定:

- `input_audio_format = pcm16`
- `input_audio_noise_reduction.type = near_field`
- `input_audio_transcription.model = gpt-4o-transcribe`
- `input_audio_transcription.language = ja`
- `input_audio_transcription.prompt = transcription_system_prompt + terms`
- `turn_detection.type = server_vad`
- `turn_detection.threshold = vad_threshold`
- `turn_detection.prefix_padding_ms = 300`
- `turn_detection.silence_duration_ms = vad_silence_ms`
- `include = ["item.input_audio_transcription.logprobs"]`

### 12.2 音声入力

- `pw-record` を利用して `24kHz / mono / s16` PCM を取得する
- 推奨 chunk は 960 bytes 前後とする
- 各 chunk を base64 にして `input_audio_buffer.append` で送る

### 12.3 受信イベント

少なくとも以下を処理する。

- `conversation.item.input_audio_transcription.delta`
- `conversation.item.input_audio_transcription.completed`
- `input_audio_buffer.committed`
- `error`

### 12.4 transcript 組み立て

- `delta` は preview のみ更新する
- `completed` は committed turn として保持する
- `input_audio_buffer.committed.item_id` と `previous_item_id` を利用して、turn の最終 append 順序を安定化する
- 確定済み turn は日本語ペイン末尾へ追記する
- 空文字または空白だけの turn は追記しない

### 12.5 finish の意味

`finish` は「録音を止めるが final transcript は待つ」操作である。

処理手順:

1. マイク capture を停止する
2. 追加の `input_audio_buffer.append` は送らない
3. `input_audio_buffer.commit` は送らない
4. `vad_silence_ms + 400ms` 以上待つ
5. その間に来た final transcript を反映する
6. 未確定 tail が残る場合は provisional transcript として日本語ペインへ落とす
7. preview を空にして `Idle` に戻る

### 12.6 自動 finish

録音セッション中、次の条件で自動的に `finish` に入る。

- メインウィンドウが非アクティブになったとき
- 英訳または和訳を開始したとき
- 設定画面を開いたとき
- 履歴画面を開いたとき
- ローカルでは発話が止まっており、最後の local speech activity または transcription activity の遅い方から 10 秒経過したとき
- ローカルで発話を検知しているのに、対応する transcription activity が 60 秒返らないとき

`finish` 要求は 1 セッションにつき 1 回だけ受理する。

### 12.7 transcription activity の定義

transcription activity は以下のいずれかを指す。

- preview の更新
- committed text の append

ローカル発話検知が止まったあとは、最後の local speech activity または transcription activity の遅い方から 10 秒カウントする。

### 12.8 ローカル発話検知

参考実装では PCM chunk に対し軽いエネルギー検出を行う。

- RMS threshold: `0.015`
- peak threshold: `0.08`
- 通知間引き: 250ms

同等の目的を果たせるなら、別アルゴリズムに置き換えてよい。重要なのは「話し続けている間に 10 秒タイムアウトで premature finish しない」ことである。

## 13. transcript 自動補正

### 13.1 目的

確定済み日本語 transcript に対して、意味を変えない軽微な後処理を行う。

### 13.2 API

- endpoint: OpenAI Responses API
- model: `gpt-5.4-nano`

### 13.3 対象テキスト

- 日本語ペインへ直近で append された未補正 tail のみ
- 複数 turn が短時間で連続した場合は 1 つの tail としてまとめてもよい

### 13.4 実行条件

- committed append の 2 秒後に実行する
- その 2 秒の間に新しい preview または committed append が来たら debounce をやり直す
- 既に補正 request が飛んでいる間は重複 request を避ける

### 13.5 適用条件

補正結果は以下をすべて満たすときだけ適用する。

- request generation が最新である
- 日本語ペイン全文が request 時スナップショットと一致する
- 補正対象 tail が現在の末尾と一致する
- 補正結果が空でない
- 補正結果が元テキストと同一でない

上記を満たさない場合、補正結果は破棄する。

### 13.6 補正ルール

- spelling discrepancy の修正
- punctuation の最小限修正
- spacing の最小限修正
- capitalization の最小限修正
- 固有名詞辞書を補助情報として与える
- 意味変更、要約、情報追加は禁止

自動補正を適用した場合、直近の録音履歴も更新する。

## 14. 翻訳仕様

### 14.1 API

- endpoint: OpenAI Responses API
- model: `gpt-4.1-nano`

### 14.2 日本語から英語

- 入力: 日本語ペイン全文
- 出力先: 英語ペイン全文置換
- 入力が空ならエラー状態 `日本語テキストがありません`

### 14.3 英語から日本語

- 入力: 英語ペイン全文
- 出力先: 日本語ペイン全文置換
- 入力が空ならエラー状態 `英語テキストがありません`

### 14.4 録音との関係

翻訳開始前に録音中なら `finish` を要求し、final transcript 待ち状態へ遷移させる。

### 14.5 完了時副作用

- 対象ペインを更新
- 結果を clipboard にコピー
- Toast を表示
- 履歴を 1 件保存
- ステータスを `待機中` に戻す

## 15. clipboard 仕様

プロセス内に clipboard handle を保持し、必要時に再利用する。

clipboard へ書き込むタイミング:

- 録音完了時、`copy_on_finish = true` かつ日本語ペインが空でない場合
- 英訳完了時
- 和訳完了時

clipboard コピーに失敗しても主処理は失敗扱いにせず、ステータスにエラーを表示する。

## 16. 履歴保存

### 16.1 保存先

- ディレクトリ: `~/.local/share/simple-openai-transcribe/history/`
- 1 entry 1 JSON file

### 16.2 JSON schema

各ファイルは以下の shape を持つ。

```json
{
  "id": "1744770000000-1234",
  "created_at_ms": 1744770000000,
  "kind": "Transcription",
  "source_text": "その操作で追加された元テキスト",
  "japanese_text": "保存時点の日本語全文",
  "english_text": "保存時点の英語全文"
}
```

`kind` は以下のいずれか。

- `Transcription`
- `TranslateToEnglish`
- `TranslateToJapanese`

### 16.3 保存単位

- 録音 1 セッションごとに 1 件
- 日本語→英語翻訳 1 実行ごとに 1 件
- 英語→日本語翻訳 1 実行ごとに 1 件

### 16.4 録音履歴

録音履歴では次を保存する。

- `source_text`: その録音セッションで新たに追記された日本語テキスト
- `japanese_text`: 録音完了時点の日本語全文
- `english_text`: 録音完了時点の英語ペイン全文

後続の自動補正で transcript が変更された場合、直近の録音履歴を同じ `id` で更新する。

### 16.5 ソート順

履歴一覧は `created_at_ms` 降順とする。

## 17. `systemd --user` 連携

repository には絶対パス埋め込み済み unit は置かず、template を置く。

- template: `systemd/simple-openai-transcribe.service.in`
- install script: `scripts/install-systemd-user-service.sh`

install script が行うこと:

1. repository root を解決
2. `target/debug` または `target/release` の binary path を確定
3. `~/.config/systemd/user/simple-openai-transcribe.service` を生成
4. `systemctl --user daemon-reload`
5. `systemctl --user enable --now simple-openai-transcribe.service`

## 18. ログとエラー処理

### 18.1 ログ

- ログは `stderr` とファイルの両方へ出力する
- 保存先: `~/.local/state/simple-openai-transcribe/simple-openai-transcribe.log`
- `state_dir` が無ければ `data_local_dir` を使ってもよい

### 18.2 記録対象

少なくとも以下を log する。

- Realtime API error
- WebSocket read error
- IPC request parse error
- IPC forwarding failure
- translation failure
- transcript correction failure
- 履歴保存または更新失敗
- clipboard failure

### 18.3 UI 上のエラー

recoverable error は可能な限りアプリを落とさず、ステータスラベルに人間可読なメッセージを表示する。

例:

- API key 未設定
- clipboard へのコピー失敗
- 履歴の読み込み失敗
- 翻訳元テキストが空

## 19. 受け入れ基準

次を満たせば、この仕様に対する最小実装として妥当とみなす。

1. `start` でウィンドウが表示され、録音が始まる。
2. 話し続けている間は 10 秒タイムアウトで premature finish せず、話し終わったあとは最後の local speech または transcription activity の遅い方から 10 秒待つ。
3. 話し終わったあと、確定 transcript は日本語ペインに追記される。
4. `finish` 後、`vad_silence_ms + 400ms` 以上待って final transcript を回収する。
5. 日本語全文から英訳でき、英語全文から和訳できる。
6. 補正はユーザー編集と衝突した場合に破棄される。
7. 録音・翻訳履歴が JSON として保存される。
8. daemon 非起動時でも `show` または `start` が自己回復的に機能する。

## 20. テスト観点

- `.env` candidate 探索
- API key fail-fast
- terms の正規化
- transcription prompt 生成
- correction prompt 生成
- VAD threshold の JSON 表現
- Realtime event type 抽出
- `previous_item_id` に基づく transcript 順序安定化
- ローカル発話検知
- 10 秒 / 60 秒 auto-finish 判定
- 履歴 JSON の serialize / deserialize

## 21. 既知の制約

- 音声入力は `pw-record` 前提
- Linux desktop session 前提
- グローバルショートカット取得はアプリ内では行わない
- Realtime transcription は `server_vad` 前提
- `GetState` IPC は未実装
