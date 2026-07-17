#!/usr/bin/env zsh
# taskshoot CLI をリリースビルドする。
# バイナリ: target/release/taskshoot

cd "$(dirname $0)" || exit

cargo build --release
