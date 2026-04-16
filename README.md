# simple-openai-transcribe

Ubuntu Wayland 向けの GTK4 常駐型日本語文字起こしアプリです。OpenAI Realtime API で streaming transcription を行い、結果を編集・翻訳できます。

## 使い方

0. Ubuntu では先に依存パッケージを入れます。

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libgraphene-1.0-dev
```

1. `.env` に `OPENAI_API_KEY=...` を設定します。
2. `cargo build --release` でビルドします。
3. `scripts/install-systemd-user-service.sh` を実行します。

```bash
scripts/install-systemd-user-service.sh --release
```

開発中に debug binary を使いたい場合は `--debug` を使います。

`pw-record` が必要です。通常の Ubuntu Desktop では `pipewire-bin` が入っていれば使えます。

## CLI

- `simple-openai-transcribe daemon`
- `simple-openai-transcribe show`
- `simple-openai-transcribe start`
- `simple-openai-transcribe finish`
- `simple-openai-transcribe quit`

Ubuntu の「キーボードショートカット」設定から、上の `start` `finish` `quit` をそれぞれ任意のキーに割り当ててください。

## systemd

repository には絶対パス入りの service file を置かず、[systemd/simple-openai-transcribe.service.in](./systemd/simple-openai-transcribe.service.in) を template として管理しています。install script が現在の repository path と `target/{debug,release}/simple-openai-transcribe` を埋め込んで `~/.config/systemd/user/simple-openai-transcribe.service` を生成します。
