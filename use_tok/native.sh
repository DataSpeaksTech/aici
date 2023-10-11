#!/bin/sh

T=llama
T=gpt4

set -x
set -e
mkdir -p src/tokenizers
(cd ../regex_llm && cargo run --release -- -t $T --save ../use_tok/tokenizer.bin)
cargo build --release
perf stat ./target/release/use_tok