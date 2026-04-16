# simple-openai-transcribe

Ubuntu Wayland 向けの GTK4/libadwaita 製ネイティブ文字起こしツールです。OpenAI Realtime API で日本語を streaming 文字起こしし、編集、双方向翻訳、補正、clipboard 連携を行います。

## 動作概要

- 日本語音声を Realtime API で streaming 文字起こし
- 確定前テキストを Live Preview に表示
- 確定済みテキストを日本語ペインに追記
- 確定後のテキストを `gpt-5.4-nano` で自動補正
- 日本語→英語、英語→日本語の双方向翻訳
- 完了した文字起こし・翻訳結果を自動で clipboard にコピー
- `systemd --user` と CLI を使って Ubuntu のキーボードショートカットから操作可能
- XDG 配下にログと操作履歴を永続保存

## 必要環境

- Ubuntu Wayland
- Rust / Cargo
- GTK4 / libadwaita 開発パッケージ
- PipeWire の `pw-record`
- OpenAI API key

依存パッケージ例:

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libgraphene-1.0-dev pipewire-bin
```

## セットアップ

1. repository root に `.env` を作成します。

```bash
OPENAI_API_KEY=your_api_key_here
```

2. ビルドします。

```bash
cargo build --release
```

3. user service を install します。

```bash
scripts/install-systemd-user-service.sh --release
```

開発中に debug binary を使う場合:

```bash
cargo build
scripts/install-systemd-user-service.sh --debug
```

## 起動方法

### systemd --user 経由

install script 実行後は service が有効化されます。

```bash
systemctl --user restart simple-openai-transcribe.service
journalctl --user -u simple-openai-transcribe.service -f
```

### 手動起動

```bash
cargo run -- daemon
```

別端末から:

```bash
cargo run -- show
cargo run -- start
cargo run -- finish
cargo run -- quit
```

## CLI

- `simple-openai-transcribe daemon`
- `simple-openai-transcribe show`
- `simple-openai-transcribe start`
- `simple-openai-transcribe finish`
- `simple-openai-transcribe quit`

`show/start/finish/quit` は daemon に Unix socket IPC で命令を送ります。daemon が未起動なら systemd user service の起動を試し、service が無い場合は現在のバイナリから daemon を直接 spawn します。

## UI

- HeaderBar
  - 録音ボタン
  - 履歴ボタン
  - 状態ラベルと spinner
  - 設定ボタン
- Live Preview
  - 確定前 transcript を薄い非編集テキストで表示
- 左ペイン
  - 日本語 transcript
  - 編集可能
- 中央
  - `英訳 ➡ Ctrl+Right`
  - `⬅ 和訳 Ctrl+Left`
- 右ペイン
  - 英語 translation
  - 編集可能
- 設定画面
  - 文字起こし prompt
  - 英訳 prompt
  - 和訳 prompt
  - 固有名詞辞書
- 履歴画面
  - 左側に履歴一覧
  - 右側にソース、日本語、英語の詳細表示

## キーボードショートカット

アプリ内:

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
  - アプリを終了

履歴画面では `Escape` でその画面を閉じられます。

## 録音終了の自動条件

録音中は、次のいずれかで自動的に `finish` に入ります。

- メインウィンドウが非アクティブになったとき
- `英訳` / `和訳` を実行したとき
- `設定` / `履歴` を開いたとき
- ローカルでは発話が止まっていて、対応する文字起こしイベントも 10 秒以上来ないとき
- ローカルで発話を検知したまま 60 秒以上文字起こしイベントが返らないとき

手動では `F1` / `Ctrl+R`、または `simple-openai-transcribe finish` でも終了できます。

Ubuntu のシステムショートカットに割り当てる例:

- `.../simple-openai-transcribe start`
- `.../simple-openai-transcribe finish`
- `.../simple-openai-transcribe quit`

ショートカット設定では PATH 依存を避けるため、command は絶対パスを推奨します。

## 設定ファイル

永続設定は以下に保存されます。

```text
~/.config/simple-openai-transcribe/config.toml
```

保存対象:

- 固有名詞辞書
- VAD 設定
- 自動コピー設定
- ウィンドウサイズ
- 文字起こし prompt
- 英訳 prompt
- 和訳 prompt

## ログと履歴

ログ保存先:

```text
~/.local/state/simple-openai-transcribe/simple-openai-transcribe.log
```

履歴保存先:

```text
~/.local/share/simple-openai-transcribe/history/
```

履歴の単位:

- 録音を 1 回開始して停止するたびに 1 件
- 英訳を 1 回実行するたびに 1 件
- 和訳を 1 回実行するたびに 1 件

録音後に `gpt-5.4-nano` の自動補正が入った場合は、同じ録音履歴を更新して最終形を残します。

## 更新時

コード変更後に同じ profile で再ビルドした場合は restart だけで反映できます。

```bash
cargo build --release
systemctl --user restart simple-openai-transcribe.service
```

service template や build profile を変える場合は install script を再実行してください。

```bash
scripts/install-systemd-user-service.sh --release
```
